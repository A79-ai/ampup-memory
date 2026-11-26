//! **GitHub.** A repository's issues and pull requests, each as of its last update (a later update is a new
//! episode beside it), in the format a hosted deployment's GitHub connector writes: source `github:OWNER/REPO#N`,
//! about `repo:OWNER/REPO` and `issue:`/`pr:OWNER/REPO#N`. The token is the one this machine already holds:
//! `GITHUB_TOKEN` or `GH_TOKEN` in the server's environment, else `gh auth token` (`gh` on `PATH`, else where it
//! installs: the background service starts with a bare `PATH`); a public repository is also
//! read with none (at GitHub's lower unauthenticated rate). It is never written to a file or taken on the
//! command line.
//!
//! **What's read.** From `--since` (default [`super::BACKFILL_DAYS`] days back) on, oldest change first, then what changed
//! from [`OVERLAP`] before the newest change read (the cursor in `DIR/sources-state.json`). GitHub lists and filters
//! by a time that can lag the `updated_at` it shows (a few records in a thousand, by up to days), so a round reads
//! offset pages from one `since` rather than moving `since` along, and once a day a round starts [`SWEEP`] back
//! instead, for a change listed under an older time. What's held already as sent isn't written again. A round reads at
//! most [`MAX_PAGES`] pages of 100 a source; one cut short (that cap, an error) keeps the earliest change on its last
//! page as the cursor, since a record listed early can carry a later time than the pages not yet read.
//!

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{BATCH, MAX_PAGES, OVERLAP, Progress, SWEEP, SWEEP_EVERY, Source, http, micros};

pub(super) fn check_repo(repo: &str) -> Result<()> {
    let ok = |p: &str| {
        !p.is_empty()
            && p.len() <= 100
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    match repo.split_once('/') {
        Some((owner, name)) if ok(owner) && ok(name) => Ok(()),
        _ => bail!("{repo:?} isn't OWNER/REPO"),
    }
}

/// `sources add github OWNER/REPO`.
pub fn source(repo: &str, since: String) -> Result<Source> {
    check_repo(repo)?;
    Ok(Source::Github {
        repo: repo.to_string(),
        since,
    })
}

/// Where `gh` is looked for when it isn't on `PATH`: the background service starts with the system's bare `PATH`.
const GH_PLACES: &[&str] = &["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"];

/// The GitHub token this machine already holds, if any: the environment's, else the `gh` CLI's.
pub(super) async fn github_token() -> Option<String> {
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(t) = std::env::var(var)
            && !t.trim().is_empty()
        {
            return Some(t.trim().to_string());
        }
    }
    for gh in std::iter::once("gh").chain(GH_PLACES.iter().copied()) {
        let Ok(Ok(out)) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(gh)
                .args(["auth", "token"])
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        else {
            continue;
        };
        let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !t.is_empty() {
            return Some(t);
        }
    }
    None
}

#[derive(Deserialize)]
pub(super) struct Issue {
    pub(super) number: u64,
    title: String,
    body: Option<String>,
    html_url: String,
    state: String,
    pub(super) updated_at: String,
    user: Option<Login>,
    pub(super) pull_request: Option<Value>,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

/// One page of a repository's issues and pull requests changed since `since`, oldest first.
async fn issues_since(
    repo: &str,
    token: Option<&str>,
    since: &str,
    page: u32,
) -> Result<Vec<Issue>> {
    let base = std::env::var("AMPUP_MEMORY_GITHUB_API")
        .unwrap_or_else(|_| "https://api.github.com".into());
    let mut req = http()
        .get(format!(
            "{}/repos/{repo}/issues",
            base.trim_end_matches('/')
        ))
        .query(&[
            ("state", "all"),
            ("sort", "updated"),
            ("direction", "asc"),
            ("per_page", "100"),
            ("since", since),
            ("page", &page.to_string()),
        ])
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28");
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await.with_context(|| format!("github {repo}"))?;
    let status = res.status();
    if status.is_success() {
        return Ok(res.json().await?);
    }
    let remaining = res
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let text = res.text().await.unwrap_or_default();
    match status.as_u16() {
        401 => {
            bail!("github {repo}: the token was refused (401); sign in again with `gh auth login`")
        }
        403 | 429 if remaining.as_deref() == Some("0") => bail!(
            "github {repo}: rate limited{}; read again next round",
            if token.is_none() {
                " (no token: set GITHUB_TOKEN or sign in with `gh auth login`)"
            } else {
                ""
            }
        ),
        404 => bail!(
            "github {repo}: not found{}",
            if token.is_none() {
                " (no token found: a private repository needs GITHUB_TOKEN, GH_TOKEN or `gh auth login`)"
            } else {
                " or not readable with this token"
            }
        ),
        _ => bail!(
            "github {repo}: {status} {}",
            text.chars().take(300).collect::<String>()
        ),
    }
}

/// An issue, or a pull request, as of its last update: the hosted GitHub connector's format.
pub(super) fn issue_episode(repo: &str, i: &Issue, acl: &[String]) -> Result<Value> {
    let kind = if i.pull_request.is_some() {
        "pr"
    } else {
        "issue"
    };
    let n = i.number;
    let body = [
        format!("# {} ({kind} {repo}#{n}, {})", i.title, i.state),
        format!(
            "By {} · {}",
            i.user.as_ref().map_or("unknown", |u| u.login.as_str()),
            i.html_url
        ),
        i.body.clone().unwrap_or_default(),
    ]
    .join("\n\n");
    Ok(json!({
        "source": format!("github:{repo}#{n}"),
        "occurred_at": micros(&i.updated_at)?,
        "acl": acl,
        "about": [format!("repo:{repo}"), format!("{kind}:{repo}#{n}")],
        "index_text": true,
        "body": body,
    }))
}

/// Read one GitHub source from its cursor, writing as it goes; the cursor moves only past what was written.
///
/// `merged_elsewhere`: the repository is also a `github-prs` source, which writes its merged pull requests whole;
/// here they're left to it, or each round would write the merged one back over the other.
pub(super) async fn read<W>(
    repo: &str,
    since: &str,
    merged_elsewhere: bool,
    p: &mut Progress,
    acl: &[String],
    write: &W,
) -> Result<()>
where
    W: Fn(Value) -> BoxFuture<'static, Result<usize>>,
{
    let token = github_token().await;
    let mut each = Each {
        repo,
        acl,
        merged_elsewhere,
    };
    read_issues(repo, since, p, token.as_deref(), write, &mut each).await
}

/// What a page of the issues list is written as. It gives what it could make and, where it stopped short, why:
/// what it made is written, and the round ends there without moving past that page.
pub(super) trait Episodes: Send {
    fn of<'a>(
        &'a mut self,
        issues: &'a [Issue],
    ) -> BoxFuture<'a, (Vec<Value>, Option<anyhow::Error>)>;
}

/// Each issue and pull request as of its last update.
struct Each<'r> {
    repo: &'r str,
    acl: &'r [String],
    merged_elsewhere: bool,
}

/// Whether an item of the issues list is a merged pull request.
pub(super) fn merged(i: &Issue) -> bool {
    i.pull_request
        .as_ref()
        .is_some_and(|p| p["merged_at"].is_string())
}

impl Episodes for Each<'_> {
    fn of<'a>(
        &'a mut self,
        issues: &'a [Issue],
    ) -> BoxFuture<'a, (Vec<Value>, Option<anyhow::Error>)> {
        let made: Result<Vec<Value>> = issues
            .iter()
            .filter(|i| !(self.merged_elsewhere && merged(i)))
            .map(|i| issue_episode(self.repo, i, self.acl))
            .collect();
        Box::pin(std::future::ready(match made {
            Ok(episodes) => (episodes, None),
            Err(e) => (vec![], Some(e)),
        }))
    }
}

/// The issues list read from the cursor, page by page, each page made into episodes by `episodes` and written.
pub(super) async fn read_issues<W>(
    repo: &str,
    since: &str,
    p: &mut Progress,
    token: Option<&str>,
    write: &W,
    episodes: &mut dyn Episodes,
) -> Result<()>
where
    W: Fn(Value) -> BoxFuture<'static, Result<usize>>,
{
    let now = chrono::Utc::now().timestamp();
    // Picking up after a round cut short starts at the cursor itself: more records than a round reads can share
    // the overlap, and starting before it again would read the same pages forever.
    let sweep = p.cursor.is_some() && !p.partial && now - p.swept_at >= SWEEP_EVERY;
    let back = if p.partial {
        chrono::Duration::zero()
    } else if sweep {
        SWEEP
    } else {
        OVERLAP
    };
    // Counted when it starts: one cut short isn't run again every round.
    if sweep {
        p.swept_at = now;
    }
    let from = match &p.cursor {
        Some(c) => (chrono::DateTime::parse_from_rfc3339(c)? - back)
            .to_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        None => since.to_string(),
    };
    // The newest change read this round, and the earliest on the last page written: a round cut short keeps the
    // latter, as a record listed early may carry a later time than pages not read yet.
    let mut newest = p.cursor.clone();
    let mut last_page_earliest: Option<String> = None;
    let mut done = false;
    let mut failed = None;
    for page in 1..=MAX_PAGES {
        let issues = match issues_since(repo, token, &from, page).await {
            Ok(i) => i,
            Err(e) => {
                failed = Some(e);
                break;
            }
        };
        let (made, stopped) = episodes.of(&issues).await;
        failed = stopped;
        for chunk in made.chunks(BATCH) {
            match write(json!({ "episodes": chunk })).await {
                Ok(n) => p.records += n as u64,
                Err(e) => {
                    failed = failed.or(Some(e));
                    break;
                }
            }
        }
        if failed.is_some() {
            break;
        }
        for i in &issues {
            if newest.as_deref().is_none_or(|c| i.updated_at.as_str() > c) {
                newest = Some(i.updated_at.clone());
            }
        }
        last_page_earliest = issues.iter().map(|i| i.updated_at.clone()).min();
        if issues.len() < 100 {
            done = true;
            break;
        }
    }
    if done {
        p.cursor = newest;
        p.partial = false;
    } else if let Some(earliest) = last_page_earliest {
        // Cut short: on from the earliest change on the last page, or, where that wouldn't move (more records in
        // one second than a round reads), from the newest read.
        p.cursor = match p.cursor.as_deref().is_none_or(|c| earliest.as_str() > c) {
            true => Some(earliest),
            false => newest,
        };
        p.partial = true;
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
