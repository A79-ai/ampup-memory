//! **Linear.** One team's issues, each with its comments and the merged GitHub pull requests Linear has attached to
//! it, in the format a hosted deployment's Linear connector writes: source `linear:WORKSPACE/KEY-123`, about
//! `issue:WORKSPACE/KEY-123` and each linked pull request as `pr:OWNER/REPO#N`. An issue is written again whenever
//! its text changes, as a new version that retires the earlier ones (`replaces_source`), dated at the newest change
//! Linear reports for the issue, its comments and its attachments. The key is a personal API key in the server's
//! environment, `LINEAR_API_KEY`; it is never written to a file or taken on the command line.
//!
//! **What's read.** From `--since` (default [`super::BACKFILL_DAYS`] days back) on, then what changed from
//! [`OVERLAP`] before the newest change read (the cursor in `DIR/sources-state.json`). Linear filters by the same
//! `updatedAt` it shows, so there's no day's wider round as GitHub needs; but a comment written or a pull request
//! attached doesn't always move the issue's own `updatedAt`, so comments and GitHub attachments changed since then
//! are listed too. The listings are read whole and sorted, oldest change first, then the issues are fetched and
//! written [`PER_REQUEST`] at a time, at most [`MAX_PAGES`] requests a round; a round cut short (that cap, an
//! error) keeps the last change written as the cursor and starts there next time. What's held already as sent
//! isn't written again.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{MAX_PAGES, OVERLAP, Progress, Source, http, micros};

/// Linear's API host; queries go to its `/graphql`.
const API: &str = "https://api.linear.app";
/// Where Linear keeps uploaded files: a link to one is written as `[image]`.
const UPLOADS: &str = "https://uploads.linear.app";
/// A description is cut at this many characters, the comments at this many in all.
const DESCRIPTION_MAX: usize = 6_000;
const COMMENTS_MAX: usize = 12_000;
/// Issues fetched whole in one request.
const PER_REQUEST: usize = 25;
/// One page of a "what changed since" listing.
const LIST_PAGE: u32 = 250;

/// `WORKSPACE/TEAM`: the workspace's URL key (as in its address) and the team's key, as the hosted connector names
/// a source.
fn parse_target(target: &str) -> Option<(&str, &str)> {
    let (ws, team) = target.split_once('/')?;
    let ws_ok = (1..=64).contains(&ws.len())
        && ws
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_lowercase() || b.is_ascii_digit() || (i > 0 && b == b'-'));
    let team_ok = (1..=16).contains(&team.len())
        && team
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    (ws_ok && team_ok).then_some((ws, team))
}

/// `sources add linear WORKSPACE/TEAM`.
pub fn source(target: &str, since: String) -> Result<Source> {
    let Some((workspace, team)) = parse_target(target) else {
        bail!(
            "{target:?} isn't WORKSPACE/TEAM: the workspace's URL key, as in its address, and the team's key, \
             e.g. acme/ENG"
        );
    };
    Ok(Source::Linear {
        workspace: workspace.to_string(),
        team: team.to_string(),
        since,
    })
}

/// The key from the server's environment.
fn linear_key() -> Option<String> {
    std::env::var("LINEAR_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

async fn query<T: for<'de> Deserialize<'de>>(key: &str, q: &str, variables: Value) -> Result<T> {
    // A personal API key is sent bare; an OAuth token as a bearer.
    let auth = if key.starts_with("lin_api_") {
        key.to_string()
    } else {
        format!("Bearer {key}")
    };
    let res = http()
        .post(format!("{API}/graphql"))
        .header("authorization", auth)
        .json(&json!({ "query": q, "variables": variables }))
        .send()
        .await
        .context("linear")?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    #[derive(Deserialize)]
    struct Out<T> {
        data: Option<T>,
        #[serde(default)]
        errors: Vec<GqlError>,
    }
    #[derive(Deserialize)]
    struct GqlError {
        message: Option<String>,
        extensions: Option<Value>,
    }
    let out: Option<Out<T>> = serde_json::from_str(&text).ok();
    let codes: Vec<&str> = out
        .iter()
        .flat_map(|o| &o.errors)
        .filter_map(|e| e.extensions.as_ref()?.get("code")?.as_str())
        .collect();
    if matches!(status.as_u16(), 401 | 403) || codes.contains(&"AUTHENTICATION_ERROR") {
        bail!("linear: the key was refused ({status}); check LINEAR_API_KEY");
    }
    if status.as_u16() == 429 || codes.contains(&"RATELIMITED") {
        bail!("linear: rate limited; read again next round");
    }
    match out {
        Some(Out {
            data: Some(d),
            errors,
        }) if status.is_success() && errors.is_empty() => Ok(d),
        Some(o) if !o.errors.is_empty() => bail!(
            "linear: {status} {}",
            o.errors
                .iter()
                .filter_map(|e| e.message.as_deref())
                .collect::<Vec<_>>()
                .join("; ")
                .chars()
                .take(300)
                .collect::<String>()
        ),
        _ => bail!(
            "linear: {status} {}",
            text.chars().take(300).collect::<String>()
        ),
    }
}

#[derive(Deserialize)]
struct Page<T> {
    nodes: Vec<T>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

/// That the key is the workspace's and sees the team.
async fn check(key: &str, workspace: &str, team: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct Viewer {
        organization: Org,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Org {
        url_key: String,
    }
    #[derive(Deserialize)]
    struct Team {
        key: String,
    }
    #[derive(Deserialize)]
    struct D {
        viewer: Viewer,
        teams: Page<Team>,
    }
    let d: D = query(
        key,
        "query($team: String!) { viewer { organization { urlKey } } \
         teams(filter: { key: { eq: $team } }) { nodes { key } pageInfo { hasNextPage endCursor } } }",
        json!({ "team": team }),
    )
    .await?;
    let theirs = d.viewer.organization.url_key;
    if theirs != workspace {
        bail!(
            "linear {workspace}/{team}: LINEAR_API_KEY is a key for the workspace {theirs:?}, not {workspace:?}"
        );
    }
    if !d.teams.nodes.iter().any(|t| t.key == team) {
        bail!("linear {workspace}/{team}: no team {team} that this key can see");
    }
    Ok(())
}

/// Every page of a listing, read to its end (Linear lists newest first; the caller sorts).
async fn list_all<T: for<'de> Deserialize<'de>>(
    key: &str,
    field: &str,
    q: &str,
    filter: Value,
) -> Result<Vec<T>> {
    let mut out = vec![];
    let mut after: Option<String> = None;
    loop {
        let mut d: BTreeMap<String, Page<T>> = query(
            key,
            q,
            json!({ "filter": filter, "after": after, "n": LIST_PAGE }),
        )
        .await?;
        let page = d
            .remove(field)
            .with_context(|| format!("linear: no {field} in the answer"))?;
        out.extend(page.nodes);
        after = match page.page_info {
            PageInfo {
                has_next_page: true,
                end_cursor: Some(c),
            } => Some(c),
            _ => break,
        };
    }
    Ok(out)
}

/// The team's issues that changed at or after `from`, each with the newest change seen for it, oldest first.
async fn changed_issues(key: &str, team: &str, from: &str) -> Result<Vec<(String, String)>> {
    let mut at: BTreeMap<String, String> = BTreeMap::new();
    let mut saw = |id: String, t: String| {
        let e = at.entry(id).or_default();
        if t > *e {
            *e = t;
        }
    };
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Changed {
        id: String,
        updated_at: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct OnIssue<I> {
        updated_at: String,
        issue: Option<I>,
    }
    #[derive(Deserialize)]
    struct IssueTeam {
        id: String,
        team: Option<TeamKey>,
    }
    #[derive(Deserialize)]
    struct TeamKey {
        key: String,
    }
    let since = json!({ "gte": from });
    for n in list_all::<Changed>(
        key,
        "issues",
        "query($filter: IssueFilter, $after: String, $n: Int) { issues(first: $n, after: $after, orderBy: updatedAt, \
         filter: $filter) { nodes { id updatedAt } pageInfo { hasNextPage endCursor } } }",
        json!({ "team": { "key": { "eq": team } }, "updatedAt": since }),
    )
    .await?
    {
        saw(n.id, n.updated_at);
    }
    for n in list_all::<OnIssue<Id>>(
        key,
        "comments",
        "query($filter: CommentFilter, $after: String, $n: Int) { comments(first: $n, after: $after, \
         orderBy: updatedAt, filter: $filter) { nodes { updatedAt issue { id } } pageInfo { hasNextPage endCursor } } }",
        json!({ "issue": { "team": { "key": { "eq": team } } }, "updatedAt": since }),
    )
    .await?
    {
        if let Some(i) = n.issue {
            saw(i.id, n.updated_at);
        }
    }
    for n in list_all::<OnIssue<IssueTeam>>(
        key,
        "attachments",
        "query($filter: AttachmentFilter, $after: String, $n: Int) { attachments(first: $n, after: $after, \
         orderBy: updatedAt, filter: $filter) { nodes { updatedAt issue { id team { key } } } \
         pageInfo { hasNextPage endCursor } } }",
        json!({ "sourceType": { "eq": "github" }, "updatedAt": since }),
    )
    .await?
    {
        if let Some(i) = n.issue
            && i.team.is_some_and(|t| t.key == team)
        {
            saw(i.id, n.updated_at);
        }
    }
    let mut out: Vec<(String, String)> = at.into_iter().collect();
    out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    Ok(out)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    body: Option<String>,
    created_at: String,
    updated_at: String,
    user: Option<Named>,
    bot_actor: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attachment {
    url: Option<String>,
    title: Option<String>,
    updated_at: String,
    metadata: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Issue {
    id: String,
    identifier: String,
    title: String,
    description: Option<String>,
    url: String,
    priority_label: Option<String>,
    created_at: String,
    updated_at: String,
    completed_at: Option<String>,
    canceled_at: Option<String>,
    state: Option<Named>,
    labels: Page<Named>,
    creator: Option<Named>,
    project: Option<Named>,
    attachments: Page<Attachment>,
    comments: Page<Comment>,
}

const COMMENT_FIELDS: &str = "body createdAt updatedAt user { name } botActor { name type }";

fn issue_fields() -> String {
    format!(
        "id identifier title description url priorityLabel createdAt updatedAt completedAt canceledAt \
         state {{ name }} labels(first: 50) {{ nodes {{ name }} pageInfo {{ hasNextPage endCursor }} }} \
         creator {{ name }} project {{ name }} \
         attachments(first: 50) {{ nodes {{ url title updatedAt metadata }} pageInfo {{ hasNextPage endCursor }} }} \
         comments(first: 50) {{ nodes {{ {COMMENT_FIELDS} }} pageInfo {{ hasNextPage endCursor }} }}"
    )
}

/// These issues, whole, each with every comment (oldest first), in the order asked. One Linear doesn't return
/// (deleted, archived, not visible to the key) is left out.
async fn fetch_issues(key: &str, ids: &[&str]) -> Result<Vec<Issue>> {
    #[derive(Deserialize)]
    struct D {
        issues: Page<Issue>,
    }
    let d: D = query(
        key,
        &format!(
            "query($ids: [ID!], $n: Int) {{ issues(first: $n, filter: {{ id: {{ in: $ids }} }}) {{ nodes {{ {} }} \
             pageInfo {{ hasNextPage endCursor }} }} }}",
            issue_fields()
        ),
        json!({ "ids": ids, "n": ids.len() }),
    )
    .await?;
    let mut by_id: BTreeMap<String, Issue> = d
        .issues
        .nodes
        .into_iter()
        .map(|i| (i.id.clone(), i))
        .collect();
    let mut out = vec![];
    for id in ids {
        let Some(mut issue) = by_id.remove(*id) else {
            continue;
        };
        let mut after = match &issue.comments.page_info {
            PageInfo {
                has_next_page: true,
                end_cursor: Some(c),
            } => Some(c.clone()),
            _ => None,
        };
        while let Some(a) = after {
            #[derive(Deserialize)]
            struct More {
                issue: MoreIssue,
            }
            #[derive(Deserialize)]
            struct MoreIssue {
                comments: Page<Comment>,
            }
            let more: More = query(
                key,
                &format!(
                    "query($id: String!, $after: String) {{ issue(id: $id) {{ comments(first: 100, after: $after) \
                     {{ nodes {{ {COMMENT_FIELDS} }} pageInfo {{ hasNextPage endCursor }} }} }} }}"
                ),
                json!({ "id": id, "after": a }),
            )
            .await?;
            issue.comments.nodes.extend(more.issue.comments.nodes);
            after = match more.issue.comments.page_info {
                PageInfo {
                    has_next_page: true,
                    end_cursor: Some(c),
                } => Some(c),
                _ => None,
            };
        }
        // Oldest first; a stable sort keeps ties in Linear's order.
        issue
            .comments
            .nodes
            .sort_by(|a, b| a.created_at.cmp(&b.created_at));
        out.push(issue);
    }
    Ok(out)
}

// --- the episode: the hosted connector's text, byte for byte ---

/// What Python's `str.isspace()` takes for space, which the format's stripping follows: Unicode's white space and
/// U+001C to U+001F.
fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn py_strip(s: &str) -> &str {
    s.trim_matches(py_space)
}

/// Where a link to an uploaded file starting at `i` ends: `[text](UPLOADS/…)`, `![text](UPLOADS/…)` or a bare
/// `UPLOADS/…`.
fn upload_at(s: &str, i: usize) -> Option<usize> {
    let rest = &s[i..];
    let after_bang = rest.strip_prefix('!').unwrap_or(rest);
    if let Some(inner) = after_bang.strip_prefix('[')
        && let Some(close) = inner.find(']')
        && let Some(link) = inner[close + 1..].strip_prefix('(')
        && let Some(path) = link.strip_prefix(UPLOADS).and_then(|l| l.strip_prefix('/'))
    {
        let n = path
            .find(|c: char| c == ')' || py_space(c))
            .unwrap_or(path.len());
        if n > 0 && path[n..].starts_with(')') {
            return Some(s.len() - path.len() + n + 1);
        }
    }
    let path = rest.strip_prefix(UPLOADS)?.strip_prefix('/')?;
    let n = path.find(py_space).unwrap_or(path.len());
    (n > 0).then(|| s.len() - path.len() + n)
}

/// The text with Windows line ends made plain, links to uploaded files as `[image]`, and stripped.
fn clean(text: Option<&str>) -> String {
    let s = text.unwrap_or_default().replace("\r\n", "\n");
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(c) = s[i..].chars().next() {
        if let Some(end) = upload_at(&s, i) {
            out.push_str("[image]");
            i = end;
        } else {
            out.push(c);
            i += c.len_utf8();
        }
    }
    py_strip(&out).to_string()
}

/// A comment as one list item: its later lines indented under the first.
fn item(text: Option<&str>) -> String {
    let c = clean(text);
    let mut lines = c.split('\n');
    let mut out = lines.next().unwrap_or_default().to_string();
    for ln in lines {
        out.push('\n');
        if !ln.is_empty() {
            out.push_str("  ");
            out.push_str(ln);
        }
    }
    out
}

fn day(t: &str) -> &str {
    t.get(..10).unwrap_or(t)
}

/// A GitHub pull request's `OWNER/REPO` and number from its address.
fn pull_url(url: &str) -> Option<(String, u64)> {
    let rest = url.strip_prefix("https://github.com/")?;
    let owner_end = rest.find(|c: char| c == '/' || c.is_whitespace())?;
    if owner_end == 0 || !rest[owner_end..].starts_with('/') {
        return None;
    }
    let after_owner = &rest[owner_end + 1..];
    let repo_end = after_owner
        .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
        .unwrap_or(after_owner.len());
    if repo_end == 0 {
        return None;
    }
    let digits = after_owner[repo_end..].strip_prefix("/pull/")?;
    let n_end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if n_end == 0 || !matches!(digits[n_end..].chars().next(), None | Some('/' | '?' | '#')) {
        return None;
    }
    Some((
        format!("{}/{}", &rest[..owner_end], &after_owner[..repo_end]),
        digits[..n_end].parse().ok()?,
    ))
}

/// Bots and AI reviewers by login, as the hosted GitHub connector knows them: a merged pull request of theirs is
/// left out of an issue's list.
const BOT_LOGINS: &[&str] = &[
    "github-actions",
    "dependabot",
    "renovate",
    "copilot",
    "copilot-pull-request-reviewer",
    "copilot-swe-agent",
    "coderabbitai",
    "claude",
    "anthropic-claude",
    "chatgpt-codex-connector",
    "codex",
    "openai-codex",
    "greptile-apps",
    "greptileai",
    "cursor",
    "cursoragent",
    "sourcery-ai",
    "ellipsis-dev",
    "devin-ai-integration",
    "qodo-merge-pro",
    "codiumai-pr-agent-pro",
    "gemini-code-assist",
    "korbit-ai",
    "codeant-ai",
    "graphite-app",
    "sentry-io",
    "seer-by-sentry",
    "vercel",
    "linear",
    "codecov",
    "sonarqubecloud",
    "gitguardian",
];

fn is_bot(login: &str) -> bool {
    let l = login.to_lowercase();
    l.ends_with("[bot]") || BOT_LOGINS.contains(&l.as_str())
}

struct Merged {
    repo: String,
    number: u64,
    title: String,
    merged_at: String,
}

/// The merged GitHub pull requests Linear has attached to the issue, oldest merge first; a bot's left out.
fn merged_pulls(issue: &Issue) -> Vec<Merged> {
    let mut seen: Vec<Merged> = vec![];
    for a in &issue.attachments.nodes {
        let Some((repo, number)) = a.url.as_deref().and_then(pull_url) else {
            continue;
        };
        let md = a.metadata.as_ref();
        let s = |k: &str| md.and_then(|m| m.get(k)).and_then(Value::as_str);
        let Some(merged_at) =
            s("mergedAt").filter(|m| !m.is_empty() && s("status") == Some("merged"))
        else {
            continue;
        };
        if s("userLogin").is_some_and(|l| !l.is_empty() && is_bot(l)) {
            continue;
        }
        let pr = Merged {
            repo,
            number,
            title: s("title")
                .map(String::from)
                .unwrap_or_else(|| a.title.clone().unwrap_or_default()),
            merged_at: merged_at.to_string(),
        };
        match seen
            .iter_mut()
            .find(|p| p.repo == pr.repo && p.number == pr.number)
        {
            Some(p) => *p = pr,
            None => seen.push(pr),
        }
    }
    seen.sort_by(|a, b| {
        (a.merged_at.as_str(), a.number, a.repo.as_str()).cmp(&(
            b.merged_at.as_str(),
            b.number,
            b.repo.as_str(),
        ))
    });
    seen
}

/// What the issue's episode is about: the issue, and each GitHub pull request attached to it (open, closed or
/// merged, a bot's too) as the GitHub connector names it.
fn about(workspace: &str, issue: &Issue) -> Vec<String> {
    let mut out = vec![format!("issue:{workspace}/{}", issue.identifier)];
    let mut pulls = BTreeSet::new();
    for a in &issue.attachments.nodes {
        if let Some((repo, n)) = a.url.as_deref().and_then(pull_url)
            && pulls.insert(format!("{repo}#{n}"))
        {
            out.push(format!("pr:{repo}#{n}"));
        }
    }
    out
}

/// The issue's text: header, meta line, description, people's comments and its merged pull requests.
fn body(workspace: &str, i: &Issue) -> String {
    let status = i.state.as_ref().map_or("?", |s| s.name.as_str());
    let head = format!(
        "# {} {} (linear {workspace}, {status})",
        i.identifier,
        py_strip(&i.title)
    );
    let mut meta = vec![];
    let labels: Vec<&str> = i.labels.nodes.iter().map(|l| l.name.as_str()).collect();
    if !labels.is_empty() {
        meta.push(format!("Labels: {}", labels.join(", ")));
    }
    if let Some(p) = i
        .priority_label
        .as_deref()
        .filter(|p| !p.is_empty() && *p != "No priority")
    {
        meta.push(format!("Priority: {p}"));
    }
    if let Some(p) = &i.project {
        meta.push(format!("Project: {}", p.name));
    }
    meta.push(format!(
        "Created {}{}",
        day(&i.created_at),
        i.creator
            .as_ref()
            .map_or(String::new(), |c| format!(" by {}", c.name))
    ));
    if let Some(t) = &i.completed_at {
        meta.push(format!("Completed {}", day(t)));
    }
    if let Some(t) = &i.canceled_at {
        meta.push(format!("Canceled {}", day(t)));
    }
    meta.push(i.url.clone());
    let mut parts = vec![format!("{head}\n{}", meta.join(" · "))];
    let desc = clean(i.description.as_deref());
    if !desc.is_empty() {
        parts.push(if desc.chars().count() <= DESCRIPTION_MAX {
            desc
        } else {
            format!(
                "{} …",
                desc.chars().take(DESCRIPTION_MAX).collect::<String>()
            )
        });
    }
    let people: Vec<&Comment> = i
        .comments
        .nodes
        .iter()
        .filter(|c| {
            c.user.is_some() && c.bot_actor.is_none() && !clean(c.body.as_deref()).is_empty()
        })
        .collect();
    if !people.is_empty() {
        let mut lines: Vec<String> = vec![];
        let mut used = 0;
        for c in &people {
            let line = format!(
                "**{}** ({}): {}",
                c.user.as_ref().map_or("", |u| u.name.as_str()),
                day(&c.created_at),
                item(c.body.as_deref())
            );
            let n = line.chars().count();
            if used + n > COMMENTS_MAX {
                lines.push(format!("({} more comments)", people.len() - lines.len()));
                break;
            }
            lines.push(line);
            used += n;
        }
        parts.push(format!("## Comments\n{}", lines.join("\n")));
    }
    let prs = merged_pulls(i);
    if !prs.is_empty() {
        let lines: Vec<String> = prs
            .iter()
            .map(|p| {
                format!(
                    "- {}#{}: {} (merged {})",
                    p.repo,
                    p.number,
                    py_strip(&p.title),
                    day(&p.merged_at)
                )
            })
            .collect();
        parts.push(format!("## Pull requests\n{}", lines.join("\n")));
    }
    format!("{}\n", parts.join("\n\n"))
}

/// The newest change Linear reports for the issue, its comments and its attachments.
fn changed_at(i: &Issue) -> &str {
    let mut t = i.updated_at.as_str();
    for x in i
        .comments
        .nodes
        .iter()
        .map(|c| c.updated_at.as_str())
        .chain(i.attachments.nodes.iter().map(|a| a.updated_at.as_str()))
    {
        if x > t {
            t = x;
        }
    }
    t
}

/// The issue's newest version, retiring the earlier ones: the hosted connector's episode.
fn issue_episode(workspace: &str, i: &Issue, acl: &[String]) -> Result<Value> {
    Ok(json!({
        "source": format!("linear:{workspace}/{}", i.identifier),
        "occurred_at": micros(changed_at(i))?,
        "acl": acl,
        "about": about(workspace, i),
        "index_text": true,
        "body": body(workspace, i),
        "replaces_source": true,
    }))
}

/// Read one Linear source from its cursor, writing as it goes; the cursor moves only past what was written.
pub(super) async fn read<W>(
    workspace: &str,
    team: &str,
    since: &str,
    p: &mut Progress,
    acl: &[String],
    write: &W,
) -> Result<()>
where
    W: Fn(Value) -> BoxFuture<'static, Result<usize>>,
{
    let Some(key) = linear_key() else {
        bail!(
            "linear {workspace}/{team}: no key; set LINEAR_API_KEY (a personal API key, from Linear's settings under \
             Security & access) in the server's environment"
        );
    };
    check(&key, workspace, team).await?;
    // Picking up after a round cut short starts at the cursor itself.
    let from = match &p.cursor {
        Some(c) => {
            let back = if p.partial {
                chrono::Duration::zero()
            } else {
                OVERLAP
            };
            (chrono::DateTime::parse_from_rfc3339(c)? - back)
                .to_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        }
        None => since.to_string(),
    };
    let changed = changed_issues(&key, team, &from).await?;
    let mut last_written: Option<&str> = None;
    let mut failed = None;
    let mut chunks = changed.chunks(PER_REQUEST);
    for chunk in chunks.by_ref().take(MAX_PAGES as usize) {
        let ids: Vec<&str> = chunk.iter().map(|(id, _)| id.as_str()).collect();
        let written = async {
            let issues = fetch_issues(&key, &ids).await?;
            let episodes = issues
                .iter()
                .map(|i| issue_episode(workspace, i, acl))
                .collect::<Result<Vec<_>>>()?;
            if !episodes.is_empty() {
                p.records += write(json!({ "episodes": episodes })).await? as u64;
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = written {
            failed = Some(e);
            break;
        }
        last_written = chunk.last().map(|(_, t)| t.as_str());
    }
    let done = failed.is_none() && chunks.next().is_none();
    if done {
        if let Some((_, newest)) = changed.last()
            && p.cursor.as_deref().is_none_or(|c| newest.as_str() > c)
        {
            p.cursor = Some(newest.clone());
        }
        p.partial = false;
    } else if let Some(t) = last_written {
        p.cursor = Some(t.to_string());
        p.partial = true;
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
