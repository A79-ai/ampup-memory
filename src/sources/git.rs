//! **Git.** A local repository's commit history, read with the `git` already on this machine: no network, no
//! account. Each commit is one episode: its message, author, date, and the files it touched with lines added and
//! removed (a diffstat, never the diff). Source `git:NAME@SHA`, where NAME is `OWNER/REPO` when the `origin`
//! remote is on GitHub (about `repo:OWNER/REPO`, as the GitHub source writes it, and `pr:OWNER/REPO#N` for a
//! squash-merged commit whose subject ends `(#N)`), else the directory's name. The same commit read from two
//! clones is the same episode.
//!
//! **What's read.** Every commit reachable from the checkout's `HEAD` (whatever branch is checked out), merges
//! left out: a merge's own message says little and its diffstat repeats its parents'. That reads the real commits
//! of a merge-commit workflow and each squashed commit of a squash workflow alike. Commits later rebased away stay
//! in memory beside their rewrites. The first read goes back `--since` (default
//! [`super::BACKFILL_DAYS`] days); later ones read `CURSOR..HEAD`, the cursor being the last commit read. Where that
//! commit is gone (a force-push, then garbage collection), a round reads from `--since` again; what's held already as
//! sent isn't written again. A round reads at most [`MAX_PAGES`] × 100 commits, oldest first in an order where a
//! commit never comes before its parents, so the next round's `CURSOR..HEAD` picks up exactly what's left.
//!
//! `git` is looked for on `PATH`, then where it installs: the background service starts with a bare `PATH`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use serde_json::{Value, json};

use super::{BATCH, MAX_PAGES, Progress, Source};

/// Where `git` is looked for after `PATH`.
const GIT_PLACES: &[&str] = &[
    "/usr/bin/git",
    "/opt/homebrew/bin/git",
    "/usr/local/bin/git",
];
/// How long one `git` call may take.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Files named in one commit's episode; the rest are counted.
const MAX_FILES: usize = 50;

/// Flags every call takes: no pager, no signature checks, paths as they are.
fn args<'a>(dir: &'a str, rest: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec![
        "-C",
        dir,
        "-c",
        "core.quotePath=false",
        "-c",
        "log.showSignature=false",
        "--no-pager",
    ];
    a.extend_from_slice(rest);
    a
}

fn candidates() -> impl Iterator<Item = &'static str> {
    std::iter::once("git").chain(GIT_PLACES.iter().copied())
}

/// `git` run synchronously, for `sources add`: its stdout, or why it failed.
fn git_sync(dir: &str, rest: &[&str]) -> Result<String> {
    for git in candidates() {
        match std::process::Command::new(git)
            .args(args(dir, rest))
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => {
                return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
            }
            Ok(out) => bail!("{}", String::from_utf8_lossy(&out.stderr).trim()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!(
        "git isn't installed (looked on PATH and in {})",
        GIT_PLACES.join(", ")
    )
}

/// `git` run by the server: its stdout, `None` where it exited non-zero (a revision that isn't there).
async fn git(dir: &str, rest: &[&str]) -> Result<Option<String>> {
    for git in candidates() {
        let child = tokio::process::Command::new(git)
            .args(args(dir, rest))
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(TIMEOUT, child).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(Err(e)) => return Err(e).with_context(|| format!("git in {dir}")),
            Err(_) => bail!("git in {dir}: no answer in {}s", TIMEOUT.as_secs()),
        };
        return Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned()));
    }
    bail!(
        "git isn't installed (looked on PATH and in {})",
        GIT_PLACES.join(", ")
    )
}

/// The work tree's top directory, as an absolute path: the source's key, whichever directory in it was named.
pub fn top(target: &str) -> Result<String> {
    let dir = std::fs::canonicalize(target).with_context(|| format!("{target:?}"))?;
    let dir = dir.to_string_lossy();
    let inside = git_sync(&dir, &["rev-parse", "--is-inside-work-tree"])
        .with_context(|| format!("{target:?} isn't a git work tree"))?;
    if inside != "true" {
        bail!("{target:?} isn't a git work tree (a bare repository has no checkout to read)");
    }
    let top = git_sync(&dir, &["rev-parse", "--show-toplevel"])?;
    Ok(std::fs::canonicalize(&top)?.to_string_lossy().into_owned())
}

/// `sources add git PATH`.
pub fn source(target: &str, since: String) -> Result<Source> {
    Ok(Source::Git {
        path: top(target)?,
        since,
    })
}

/// `OWNER/REPO` of a GitHub remote URL (https, `git@github.com:` or `ssh://`), else `None`. Nothing else of the URL
/// (which can carry a token) is kept.
fn github_repo(url: &str) -> Option<String> {
    let url = url.trim();
    let at = url.find("github.com")?;
    if at > 0 && !url[..at].ends_with(['@', '/']) {
        return None;
    }
    let rest = url[at + "github.com".len()..].strip_prefix([':', '/'])?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    let ok = |p: &str| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (ok(owner) && ok(name)).then(|| format!("{owner}/{name}"))
}

struct Commit {
    sha: String,
    author: String,
    email: String,
    date: String,
    message: String,
    /// `(added, removed, path)`; a binary file's counts are `-`.
    files: Vec<(String, String, String)>,
}

/// What `git log --format=FORMAT --numstat` printed: one record a commit.
const FORMAT: &str = "--format=%x1e%H%x1f%aN%x1f%aE%x1f%aI%x1f%B%x1f";

fn parse(out: &str) -> Vec<Commit> {
    out.split('\x1e')
        .filter_map(|rec| {
            let f: Vec<&str> = rec.splitn(6, '\x1f').collect();
            let [sha, author, email, date, message, stat] = f.as_slice() else {
                return None;
            };
            let files = stat
                .lines()
                .filter_map(|l| {
                    let mut p = l.splitn(3, '\t');
                    Some((
                        p.next()?.to_string(),
                        p.next()?.to_string(),
                        p.next()?.to_string(),
                    ))
                })
                .collect();
            Some(Commit {
                sha: sha.trim().to_string(),
                author: author.to_string(),
                email: email.to_string(),
                date: date.to_string(),
                message: message.trim().to_string(),
                files,
            })
        })
        .collect()
}

/// A commit as an episode. Nothing in it changes from one read to the next, so a commit read again is held as sent.
fn commit_episode(name: &str, github: Option<&str>, c: &Commit, acl: &[String]) -> Result<Value> {
    let short = &c.sha[..c.sha.len().min(12)];
    let (subject, rest) = c.message.split_once('\n').unwrap_or((&c.message, ""));
    let (mut added, mut removed) = (0u64, 0u64);
    for (a, r, _) in &c.files {
        added += a.parse::<u64>().unwrap_or(0);
        removed += r.parse::<u64>().unwrap_or(0);
    }
    let mut files = format!(
        "{} file{} changed, +{added} -{removed}",
        c.files.len(),
        if c.files.len() == 1 { "" } else { "s" }
    );
    for (a, r, path) in c.files.iter().take(MAX_FILES) {
        files.push_str(&format!("\n- {path} (+{a} -{r})"));
    }
    if c.files.len() > MAX_FILES {
        files.push_str(&format!("\n- and {} more", c.files.len() - MAX_FILES));
    }
    let mut by = format!("By {} <{}> · {}", c.author, c.email, c.date);
    if let Some(repo) = github {
        by.push_str(&format!(" · https://github.com/{repo}/commit/{}", c.sha));
    }
    let body = [
        format!("# {subject} (commit {short} in {name})"),
        by,
        rest.trim().to_string(),
        files,
    ]
    .into_iter()
    .filter(|p| !p.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    let mut about = vec![];
    if let Some(repo) = github {
        about.push(format!("repo:{repo}"));
        // A squash merge's subject ends with its pull request: `Fix the thing (#123)`.
        if let Some(n) = subject
            .trim_end()
            .strip_suffix(')')
            .and_then(|s| s.rsplit_once("(#"))
            .map(|(_, n)| n)
            .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        {
            about.push(format!("pr:{repo}#{n}"));
        }
    }
    Ok(json!({
        "source": format!("git:{name}@{}", c.sha),
        "occurred_at": chrono::DateTime::parse_from_rfc3339(&c.date)
            .with_context(|| format!("commit {}: date {:?}", c.sha, c.date))?
            .timestamp_micros(),
        "acl": acl,
        "about": about,
        "index_text": true,
        "body": body,
    }))
}

/// Read one local repository from its cursor (the last commit read), writing as it goes; the cursor moves only past
/// what was written.
pub(super) async fn read<W>(
    path: &str,
    since: &str,
    p: &mut Progress,
    acl: &[String],
    write: &W,
) -> Result<()>
where
    W: Fn(Value) -> BoxFuture<'static, Result<usize>>,
{
    if !Path::new(path).is_dir() {
        bail!("git {path}: not there any more (`sources remove git {path}` stops reading it)");
    }
    // A repository with no commit yet has nothing to read.
    if git(path, &["rev-parse", "--verify", "-q", "HEAD^{commit}"])
        .await?
        .is_none()
    {
        return Ok(());
    }
    let github = git(path, &["config", "--get", "remote.origin.url"])
        .await?
        .and_then(|u| github_repo(&u));
    let name = match &github {
        Some(r) => r.clone(),
        None => Path::new(path)
            .file_name()
            .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned()),
    };
    // From the cursor where it's still a commit here; else (none yet, or rewritten away) from `since`.
    let mut range = "HEAD".to_string();
    if let Some(c) = &p.cursor
        && git(
            path,
            &["rev-parse", "--verify", "-q", &format!("{c}^{{commit}}")],
        )
        .await?
        .is_some()
    {
        range = format!("{c}..HEAD");
    }
    let since_arg = format!("--since={since}");
    let list = git(
        path,
        &[
            "rev-list",
            "--no-merges",
            "--topo-order",
            "--reverse",
            &since_arg,
            &range,
        ],
    )
    .await?
    .with_context(|| format!("git {path}: rev-list {range} failed"))?;
    let shas: Vec<&str> = list.split_whitespace().collect();
    let cap = MAX_PAGES as usize * 100;
    p.partial = shas.len() > cap;
    for chunk in shas[..shas.len().min(cap)].chunks(BATCH) {
        let mut a = vec![
            "log",
            "--no-walk=unsorted",
            "--no-color",
            FORMAT,
            "--numstat",
        ];
        a.extend_from_slice(chunk);
        let out = git(path, &a)
            .await?
            .with_context(|| format!("git {path}: log failed"))?;
        let commits = parse(&out);
        let episodes = commits
            .iter()
            .map(|c| commit_episode(&name, github.as_deref(), c, acl))
            .collect::<Result<Vec<_>>>()?;
        if !episodes.is_empty() {
            p.records += write(json!({ "episodes": episodes })).await? as u64;
        }
        p.cursor = chunk.last().map(|s| s.to_string());
    }
    Ok(())
}
