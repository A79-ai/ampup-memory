//! **GitHub pull requests.** A repository's merged pull requests with their human review, in the format a hosted
//! deployment's GitHub PR connector writes, so the same PR reads the same from either: one episode for the PR
//! (description, files changed, reviews, discussion), source `github:OWNER/REPO#N`, and one for each review thread
//! a person started, source `github:OWNER/REPO#N/thread/ID` (where it is, whether resolved or outdated, its diff
//! hunk, its comments); about `repo:OWNER/REPO` and `pr:OWNER/REPO#N`, dated at the merge. Each replaces its
//! source's earlier versions: the PR's from before it merged (the [`super::github`] kind's), a thread's before a
//! later reply.
//!
//! **What's read.** The repository's issues list, as [`super::github`] reads it (from `--since`, the cursor, the
//! overlap and the day's sweep), keeping the pull requests merged; each is then read whole from GitHub's GraphQL API
//! (every page of its files, reviews, comments and threads). One GitHub can't give whole, twice, is written as the
//! issues list shows it instead. GraphQL needs a token: the one this machine holds, found as [`super::github`]
//! finds it. A round ends early, to go on next round, when GitHub says to wait or its hourly points run low.
//!
//! **Nothing a bot wrote.** A bot's pull request is skipped; a bot's reviews and comments, and a whole thread a bot
//! started, people's replies included, are left out. A bot is GitHub's `Bot` account, a `[bot]` login, a known AI
//! reviewer's login, a comment that says it's an AI's, or a comment of a bot's review. A team's own review bot
//! posting from a person's account is known by how its review opens: `AMPUP_MEMORY_GITHUB_REVIEW_BOT`.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::github::{Episodes, Issue, github_token, issue_episode, merged, read_issues};
use super::{Progress, Source, http, micros};

/// `sources add github-prs OWNER/REPO`.
pub fn source(repo: &str, since: String) -> Result<Source> {
    super::github::source(repo, since.clone())?;
    Ok(Source::GithubPrs {
        repo: repo.to_string(),
        since,
    })
}

const DESCRIPTION_MAX: usize = 6000;
const FILES_MAX: usize = 80;
const HUNK_LINES: usize = 40;
/// A hunk line longer than this is cut: a generated file's hunk can be one line of 260,000 characters.
const HUNK_LINE_MAX: usize = 300;
/// GraphQL points kept back: a round stops when fewer are left this hour.
const POINTS_RESERVE: i64 = 100;

/// Bots and AI reviewers by login (lowercased, `[bot]` dropped), should one ever show as a person.
const AI_REVIEWERS: &[&str] = &[
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

/// What an AI review posted from a person's account says of itself.
const AI_MARKERS: &[&str] = &[
    "<!-- This is an auto-generated comment",
    "Generated with [Claude Code]",
    "Generated with Claude Code",
    "🤖 Codex",
    "Codex Review",
    "[Codex]",
    "Reviewed by Greptile",
    "Summary by CodeRabbit",
];

// --- what GraphQL gives ---

#[derive(Deserialize, Default)]
struct Author {
    #[serde(rename = "__typename", default)]
    typename: Option<String>,
    #[serde(default)]
    login: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page<T> {
    #[serde(default)]
    page_info: Option<PageInfo>,
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct File {
    path: String,
    additions: i64,
    deletions: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Review {
    database_id: i64,
    author: Option<Author>,
    state: String,
    body: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    database_id: i64,
    author: Option<Author>,
    body: Option<String>,
    created_at: String,
    #[serde(default)]
    diff_hunk: Option<String>,
    #[serde(default)]
    pull_request_review: Option<ReviewRef>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRef {
    database_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    id: String,
    path: String,
    line: Option<i64>,
    original_line: Option<i64>,
    is_resolved: bool,
    is_outdated: bool,
    comments: Page<Comment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequest {
    number: u64,
    title: String,
    url: String,
    body: Option<String>,
    merged_at: Option<String>,
    author: Option<Author>,
    files: Page<File>,
    reviews: Page<Review>,
    comments: Page<Comment>,
    review_threads: Page<Thread>,
}

// --- GitHub's GraphQL API ---

const PAGE_INFO: &str = "pageInfo { hasNextPage endCursor }";
const AUTHOR: &str = "author { __typename login }";
const FILES: &str = "path additions deletions";
const RATE: &str = "rateLimit { cost remaining resetAt }";
/// Comments per thread in the first page; a longer thread is read on page by page.
const THREAD_COMMENTS: u32 = 30;

fn review_fields() -> String {
    format!("databaseId {AUTHOR} state body submittedAt")
}
fn comment_fields() -> String {
    format!("databaseId {AUTHOR} body createdAt")
}
fn thread_comment_fields() -> String {
    format!("databaseId {AUTHOR} body createdAt diffHunk pullRequestReview {{ databaseId }}")
}
fn thread_fields() -> String {
    format!(
        "id path line originalLine startLine diffSide isResolved isOutdated subjectType \
         comments(first: {THREAD_COMMENTS}) {{ totalCount {PAGE_INFO} nodes {{ {} }} }}",
        thread_comment_fields()
    )
}

/// Each connection of a pull request: its name, how many its first page holds, and the fields of each node.
fn connections() -> [(&'static str, u32, String); 4] {
    [
        ("files", 100, FILES.to_string()),
        ("reviews", 50, review_fields()),
        ("comments", 100, comment_fields()),
        ("reviewThreads", 50, thread_fields()),
    ]
}

/// One pull request with the first page of each connection: under GitHub's node limits (50 threads × 30
/// comments at most), about a point a pull request.
fn pull_request_query() -> String {
    let conns = connections()
        .iter()
        .map(|(k, first, fields)| {
            format!("{k}(first: {first}) {{ totalCount {PAGE_INFO} nodes {{ {fields} }} }}")
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "query($owner: String!, $name: String!, $number: Int!) {{ {RATE} \
         repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{ \
         number title url body createdAt mergedAt baseRefName {AUTHOR} {conns} }} }} }}"
    )
}

/// The next page of one of a pull request's connections.
fn more_query(k: &str, fields: &str) -> String {
    format!(
        "query($owner: String!, $name: String!, $number: Int!, $after: String) {{ {RATE} \
         repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{ \
         {k}(first: 100, after: $after) {{ {PAGE_INFO} nodes {{ {fields} }} }} }} }} }}"
    )
}

/// The next page of a review thread's comments.
fn thread_more_query() -> String {
    format!(
        "query($id: ID!, $after: String) {{ {RATE} node(id: $id) {{ ... on PullRequestReviewThread {{ \
         comments(first: 100, after: $after) {{ {PAGE_INFO} nodes {{ {} }} }} }} }} }}",
        thread_comment_fields()
    )
}

/// Ends the round, to go on next round: GitHub said to wait, or refused the token. Not retried.
#[derive(Debug)]
struct Stop(String);
impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Stop {}

/// GitHub has no such repository or pull request (or not one this token can see).
#[derive(Debug)]
struct NotFound;
impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not found")
    }
}
impl std::error::Error for NotFound {}

struct Graphql {
    token: String,
    url: String,
    /// The points left this hour and when they come back, as the last answer gave them.
    remaining: Option<(i64, String)>,
}

impl Graphql {
    fn new(token: String) -> Self {
        let base = std::env::var("AMPUP_MEMORY_GITHUB_API")
            .unwrap_or_else(|_| "https://api.github.com".into());
        Graphql {
            token,
            url: format!("{}/graphql", base.trim_end_matches('/')),
            remaining: None,
        }
    }

    async fn query(&mut self, query: &str, variables: Value) -> Result<Value> {
        let res = http()
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .context("github graphql")?;
        let status = res.status();
        let header = |k: &str| {
            res.headers()
                .get(k)
                .and_then(|v| v.to_str().ok())
                .map(String::from)
        };
        let (retry_after, remaining) = (header("retry-after"), header("x-ratelimit-remaining"));
        let text = res.text().await.unwrap_or_default();
        if status.as_u16() == 401 {
            return Err(Stop(
                "github graphql: the token was refused (401); sign in again with `gh auth login`"
                    .into(),
            )
            .into());
        }
        if matches!(status.as_u16(), 403 | 429)
            && (retry_after.is_some()
                || remaining.as_deref() == Some("0")
                || text.to_lowercase().contains("rate limit"))
        {
            return Err(Stop("github graphql: rate limited; read again next round".into()).into());
        }
        if !status.is_success() {
            bail!(
                "github graphql: {status} {}",
                text.chars().take(300).collect::<String>()
            );
        }
        let out: Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "github graphql: an answer that isn't JSON ({})",
                text.chars().take(100).collect::<String>()
            )
        })?;
        let errors = out["errors"].as_array().cloned().unwrap_or_default();
        let kind = |e: &Value| e["type"].as_str().unwrap_or("ERROR").to_string();
        if errors.iter().any(|e| kind(e) == "RATE_LIMITED") {
            return Err(Stop("github graphql: rate limited; read again next round".into()).into());
        }
        if !errors.is_empty() {
            if errors.iter().all(|e| kind(e) == "NOT_FOUND") {
                return Err(NotFound.into());
            }
            let what = errors
                .iter()
                .map(|e| format!("{}: {}", kind(e), e["message"].as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join("; ");
            bail!(
                "github graphql: {}",
                what.chars().take(300).collect::<String>()
            );
        }
        let data = out
            .get("data")
            .filter(|d| !d.is_null())
            .cloned()
            .context("github graphql: no data")?;
        if let Some(left) = data["rateLimit"]["remaining"].as_i64() {
            let reset = data["rateLimit"]["resetAt"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            self.remaining = Some((left, reset));
        }
        Ok(data)
    }

    /// The rest of one connection's pages, from `info` on.
    async fn rest<T: DeserializeOwned>(
        &mut self,
        vars: &Value,
        k: &str,
        fields: &str,
        page: &mut Page<T>,
    ) -> Result<()> {
        let query = more_query(k, fields);
        while let Some(PageInfo {
            has_next_page: true,
            end_cursor,
        }) = page.page_info.take()
        {
            let mut v = vars.clone();
            v["after"] = json!(end_cursor);
            let data = self.query(&query, v).await?;
            let next: Page<T> =
                serde_json::from_value(data["repository"]["pullRequest"][k].clone())
                    .with_context(|| format!("github graphql: a page of {k}"))?;
            page.nodes.extend(next.nodes);
            page.page_info = next.page_info;
        }
        Ok(())
    }

    /// A pull request, every connection complete; none when GitHub has no such pull request in the repository.
    async fn pull_request(&mut self, repo: &str, number: u64) -> Result<Option<PullRequest>> {
        let (owner, name) = repo.split_once('/').context("OWNER/REPO")?;
        let vars = json!({ "owner": owner, "name": name, "number": number });
        let data = match self.query(&pull_request_query(), vars.clone()).await {
            Ok(d) => d,
            Err(e) if e.is::<NotFound>() => return Ok(None),
            Err(e) => return Err(e),
        };
        let found = &data["repository"]["pullRequest"];
        if found.is_null() {
            return Ok(None);
        }
        let mut pr: PullRequest = serde_json::from_value(found.clone())
            .with_context(|| format!("github graphql: {repo}#{number}"))?;
        self.rest(&vars, "files", FILES, &mut pr.files).await?;
        self.rest(&vars, "reviews", &review_fields(), &mut pr.reviews)
            .await?;
        self.rest(&vars, "comments", &comment_fields(), &mut pr.comments)
            .await?;
        self.rest(
            &vars,
            "reviewThreads",
            &thread_fields(),
            &mut pr.review_threads,
        )
        .await?;
        let query = thread_more_query();
        for t in &mut pr.review_threads.nodes {
            while let Some(PageInfo {
                has_next_page: true,
                end_cursor,
            }) = t.comments.page_info.take()
            {
                let data = self
                    .query(&query, json!({ "id": t.id, "after": end_cursor }))
                    .await?;
                let next: Page<Comment> = serde_json::from_value(data["node"]["comments"].clone())
                    .context("github graphql: a page of a thread's comments")?;
                t.comments.nodes.extend(next.nodes);
                t.comments.page_info = next.page_info;
            }
        }
        Ok(Some(pr))
    }
}

// --- Python's string rules, which the hosted format's text follows ---

/// What Python's `str.isspace()` takes: Rust's whitespace and the four separators U+001C–U+001F.
fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn py_strip(s: &str) -> &str {
    s.trim_matches(py_space)
}

fn py_rstrip(s: &str) -> &str {
    s.trim_end_matches(py_space)
}

/// At most `n` characters, cut ones ending in "…".
fn clip(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        return text.to_string();
    }
    let head: String = text.chars().take(n - 1).collect();
    format!("{}…", py_rstrip(&head))
}

fn crlf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// A deleted account is `ghost`, as GitHub shows it.
fn login(a: Option<&Author>) -> &str {
    a.and_then(|a| a.login.as_deref())
        .filter(|l| !l.is_empty())
        .unwrap_or("ghost")
}

/// The opening a team's own review bot gives its reviews, when it posts from a person's account.
fn review_bot() -> Option<String> {
    std::env::var("AMPUP_MEMORY_GITHUB_REVIEW_BOT")
        .ok()
        .filter(|m| !m.trim().is_empty())
}

/// Whether a pull request, review or comment is a bot's. `review` is a comment's review: a comment of a bot's
/// review is the bot's, whoever's account posted it.
fn bot(
    author: Option<&Author>,
    body: Option<&str>,
    review: Option<i64>,
    bot_reviews: &HashSet<i64>,
    marker: Option<&str>,
) -> bool {
    let name = author
        .and_then(|a| a.login.as_deref())
        .unwrap_or_default()
        .to_lowercase();
    let text = body.unwrap_or_default();
    author.and_then(|a| a.typename.as_deref()) == Some("Bot")
        || name.ends_with("[bot]")
        || marker.is_some_and(|m| text.trim_start_matches(py_space).starts_with(m))
        || review.is_some_and(|r| bot_reviews.contains(&r))
        || AI_REVIEWERS.contains(&name.as_str())
        || AI_MARKERS.iter().any(|m| text.contains(m))
}

/// A comment as one list item or line: its later lines indented under the first.
fn item(text: &str) -> String {
    let text = crlf(text);
    let mut lines = py_strip(&text).split('\n');
    let first = lines.next().unwrap_or_default().to_string();
    std::iter::once(first)
        .chain(lines.map(|ln| match py_strip(ln).is_empty() {
            true => String::new(),
            false => format!("  {ln}"),
        }))
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_text(s: Option<&str>) -> bool {
    !py_strip(s.unwrap_or_default()).is_empty()
}

fn day(t: &str) -> &str {
    t.get(..10).unwrap_or(t)
}

/// The PR episode's text: its header, description, files, and the human reviews and discussion.
fn pr_body(repo: &str, pr: &PullRequest, reviews: &[&Review], comments: &[&Comment]) -> String {
    let mut parts = vec![format!(
        "# {} (pr {repo}#{}, merged {})\nBy {} · {}",
        pr.title,
        pr.number,
        day(pr.merged_at.as_deref().unwrap_or_default()),
        login(pr.author.as_ref()),
        pr.url
    )];
    let desc = clip(
        py_strip(&crlf(pr.body.as_deref().unwrap_or_default())),
        DESCRIPTION_MAX,
    );
    if !desc.is_empty() {
        parts.push(desc);
    }
    let files = &pr.files.nodes;
    if !files.is_empty() {
        let mut lines: Vec<String> = files
            .iter()
            .take(FILES_MAX)
            .map(|f| format!("- {} (+{} −{})", f.path, f.additions, f.deletions))
            .collect();
        if files.len() > FILES_MAX {
            lines.push(format!("- … and {} more", files.len() - FILES_MAX));
        }
        parts.push(format!("## Files changed\n{}", lines.join("\n")));
    }
    if !reviews.is_empty() {
        let lines: Vec<String> = reviews
            .iter()
            .map(|r| {
                format!(
                    "- {}, {}: {}",
                    login(r.author.as_ref()),
                    r.state,
                    item(r.body.as_deref().unwrap_or_default())
                )
            })
            .collect();
        parts.push(format!("## Reviews\n{}", lines.join("\n")));
    }
    if !comments.is_empty() {
        let lines: Vec<String> = comments
            .iter()
            .map(|c| {
                format!(
                    "- {}: {}",
                    login(c.author.as_ref()),
                    item(c.body.as_deref().unwrap_or_default())
                )
            })
            .collect();
        parts.push(format!("## Discussion\n{}", lines.join("\n")));
    }
    parts.join("\n\n")
}

/// A review thread's text: where it is, whether resolved or outdated, its diff hunk, its comments.
fn thread_body(repo: &str, pr: &PullRequest, t: &Thread, comments: &[&Comment]) -> String {
    let line = t
        .line
        .filter(|l| *l != 0)
        .or(t.original_line.filter(|l| *l != 0));
    let place = match line {
        Some(l) => format!("{}:{l}", t.path),
        None => t.path.clone(),
    };
    let yes = |b: bool| if b { "yes" } else { "no" };
    let mut parts = vec![format!(
        "# Review thread on {place} in pr {repo}#{} ({})\nResolved: {} · Outdated (the code changed after it): {}",
        pr.number,
        pr.title,
        yes(t.is_resolved),
        yes(t.is_outdated)
    )];
    let hunk = crlf(
        comments
            .first()
            .and_then(|c| c.diff_hunk.as_deref())
            .unwrap_or_default(),
    );
    let lines: Vec<&str> = hunk.split('\n').collect();
    let hunk = lines[lines.len().saturating_sub(HUNK_LINES)..]
        .iter()
        .map(|ln| clip(ln, HUNK_LINE_MAX))
        .collect::<Vec<_>>()
        .join("\n");
    let hunk = py_rstrip(&hunk);
    if !hunk.is_empty() {
        parts.push(format!("```diff\n{hunk}\n```"));
    }
    parts.push(
        comments
            .iter()
            .map(|c| {
                format!(
                    "**{}** ({}): {}",
                    login(c.author.as_ref()),
                    day(&c.created_at),
                    item(c.body.as_deref().unwrap_or_default())
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );
    parts.join("\n\n")
}

/// A merged PR's episodes, dated at its merge: the PR's first, then one per thread a person started. None for a
/// bot's PR.
fn pull_request_episodes(repo: &str, pr: &PullRequest, acl: &[String]) -> Result<Vec<Value>> {
    let merged_at = pr
        .merged_at
        .as_deref()
        .with_context(|| format!("{repo}#{} isn't merged", pr.number))?;
    let marker = review_bot();
    let marker = marker.as_deref();
    let none = HashSet::new();
    if bot(pr.author.as_ref(), None, None, &none, marker) {
        return Ok(vec![]);
    }
    let mut bot_reviews = HashSet::new();
    let mut reviews = vec![];
    for r in &pr.reviews.nodes {
        if bot(r.author.as_ref(), r.body.as_deref(), None, &none, marker) {
            bot_reviews.insert(r.database_id);
        } else if has_text(r.body.as_deref()) && r.state != "PENDING" {
            reviews.push(r);
        }
    }
    let comments: Vec<&Comment> = pr
        .comments
        .nodes
        .iter()
        .filter(|c| {
            !bot(c.author.as_ref(), c.body.as_deref(), None, &none, marker)
                && has_text(c.body.as_deref())
        })
        .collect();
    let by_bot = |c: &Comment| {
        let review = c.pull_request_review.as_ref().and_then(|r| r.database_id);
        bot(
            c.author.as_ref(),
            c.body.as_deref(),
            review,
            &bot_reviews,
            marker,
        )
    };
    let mut threads: Vec<(&Thread, Vec<&Comment>)> = vec![];
    for t in &pr.review_threads.nodes {
        let Some(first) = t.comments.nodes.first() else {
            continue;
        };
        if by_bot(first) {
            continue;
        }
        let kept: Vec<&Comment> = t
            .comments
            .nodes
            .iter()
            .filter(|c| !by_bot(c) && has_text(c.body.as_deref()))
            .collect();
        if !kept.is_empty() {
            threads.push((t, kept));
        }
    }
    threads.sort_by(|(_, a), (_, b)| {
        (a[0].created_at.as_str(), a[0].database_id)
            .cmp(&(b[0].created_at.as_str(), b[0].database_id))
    });
    let occurred_at = micros(merged_at)?;
    let episode = |source: String, body: String| {
        json!({
            "source": source,
            "occurred_at": occurred_at,
            "acl": acl,
            "about": [format!("repo:{repo}"), format!("pr:{repo}#{}", pr.number)],
            "index_text": true,
            "body": body,
            "replaces_source": true,
        })
    };
    let source = format!("github:{repo}#{}", pr.number);
    let mut out = vec![episode(
        source.clone(),
        pr_body(repo, pr, &reviews, &comments),
    )];
    for (t, kept) in &threads {
        out.push(episode(
            format!("{source}/thread/{}", kept[0].database_id),
            thread_body(repo, pr, t, kept),
        ));
    }
    Ok(out)
}

/// Read one source of merged pull requests from its cursor, writing as it goes; the cursor moves only past what
/// was written.
pub(super) async fn read<W>(
    repo: &str,
    since: &str,
    p: &mut Progress,
    acl: &[String],
    write: &W,
) -> Result<()>
where
    W: Fn(Value) -> BoxFuture<'static, Result<usize>>,
{
    let Some(token) = github_token().await else {
        bail!(
            "github-prs {repo}: reading reviews needs a GitHub token: set GITHUB_TOKEN or GH_TOKEN, or sign in with `gh auth login`"
        );
    };
    let mut merged = Merged {
        gql: Graphql::new(token.clone()),
        repo,
        acl,
    };
    read_issues(repo, since, p, Some(&token), write, &mut merged).await
}

/// Each merged pull request on a page of the issues list, read whole.
struct Merged<'r> {
    gql: Graphql,
    repo: &'r str,
    acl: &'r [String],
}

impl Episodes for Merged<'_> {
    fn of<'a>(
        &'a mut self,
        issues: &'a [Issue],
    ) -> BoxFuture<'a, (Vec<Value>, Option<anyhow::Error>)> {
        Box::pin(async move {
            let (repo, acl) = (self.repo, self.acl);
            let mut out = vec![];
            for i in issues.iter().filter(|i| merged(i)) {
                if let Some((left, reset)) = &self.gql.remaining
                    && *left < POINTS_RESERVE
                {
                    let why = format!(
                        "github-prs {repo}: GitHub's GraphQL points are low ({left} left until {reset}); read again next round"
                    );
                    return (out, Some(anyhow::anyhow!(why)));
                }
                let made = match pull_request(&mut self.gql, repo, i.number).await {
                    Err(e) => return (out, Some(e)),
                    Ok(Some(pr)) if pr.merged_at.is_some() => pull_request_episodes(repo, &pr, acl),
                    // GitHub couldn't give it whole, or doesn't know it (moved): as the issues list shows it.
                    Ok(_) => issue_episode(repo, i, acl).map(|ep| vec![ep]),
                };
                match made {
                    Ok(eps) => out.extend(eps),
                    Err(e) => return (out, Some(e)),
                }
            }
            (out, None)
        })
    }
}

/// A merged pull request, whole: none when GitHub doesn't have it, or failed on it twice (logged), so one it can't
/// serve (a query timing out on a huge one) doesn't hold every round behind it. GitHub saying to wait, or refusing
/// the token, ends the round.
async fn pull_request(gql: &mut Graphql, repo: &str, number: u64) -> Result<Option<PullRequest>> {
    for attempt in 1.. {
        match gql.pull_request(repo, number).await {
            Ok(pr) => return Ok(pr),
            Err(e) if e.is::<Stop>() => return Err(e),
            Err(_) if attempt < 2 => tokio::time::sleep(Duration::from_secs(2)).await,
            Err(e) => {
                eprintln!(
                    "sources: github-prs {repo}#{number} couldn't be read whole ({e:#}); written as the issues list shows it"
                );
                return Ok(None);
            }
        }
    }
    unreachable!()
}
