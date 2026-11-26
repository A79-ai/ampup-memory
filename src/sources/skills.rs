//! **A repository's skills.** Each `SKILL.md` a GitHub repository holds on its default branch (Claude Code's
//! `.claude/skills/<name>/SKILL.md`, a plugin's `skills/<name>/SKILL.md`, wherever it is), as it is there: a version
//! of the document `repo-skills/OWNER/REPO/<path>` ([`crate::skill_review`]), source `github:OWNER/REPO#skill/<path>` (under the repository's own prefix, which a forget retracts),
//! about `repo:OWNER/REPO`. Read with the token this machine already holds, as [`super::github`] reads.
//!
//! **What's read.** The default branch's tree, each round: a file whose blob changed since it was last written is
//! written again (its next version), one unchanged is not. At most [`MAX_SKILLS`] files of at most [`MAX_BYTES`].

use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};

use super::github::github_token;
use super::{Progress, Source, Version, http};

/// Most `SKILL.md` files read from one repository.
const MAX_SKILLS: usize = 200;
/// Largest `SKILL.md` read.
const MAX_BYTES: usize = 256 * 1024;

/// `sources add skills OWNER/REPO`.
pub fn source(repo: &str) -> Result<Source> {
    super::github::check_repo(repo)?;
    Ok(Source::Skills {
        repo: repo.to_string(),
    })
}

fn api() -> String {
    std::env::var("AMPUP_MEMORY_GITHUB_API")
        .unwrap_or_else(|_| "https://api.github.com".into())
        .trim_end_matches('/')
        .to_string()
}

async fn get(url: &str, token: Option<&str>, accept: &str) -> Result<reqwest::Response> {
    let mut req = http()
        .get(url)
        .header("accept", accept)
        .header("x-github-api-version", "2022-11-28");
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await.with_context(|| format!("github {url}"))?;
    match res.status().as_u16() {
        200..=299 => Ok(res),
        401 => bail!("github: the token was refused (401); sign in again with `gh auth login`"),
        404 => bail!("github {url}: not found or not readable with this token"),
        s => bail!(
            "github {url}: {s} {}",
            res.text()
                .await
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect::<String>()
        ),
    }
}

#[derive(Deserialize)]
struct Repo {
    default_branch: String,
}

#[derive(Deserialize)]
struct Tree {
    tree: Vec<Entry>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct Entry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
    #[serde(default)]
    size: Option<usize>,
}

/// Whether `path` is a skill's file.
fn is_skill(path: &str) -> bool {
    path == "SKILL.md" || path.ends_with("/SKILL.md")
}

/// Read the repository's skills, writing each one changed since it was last written.
pub(super) async fn read<W>(repo: &str, p: &mut Progress, acl: &[String], write: &W) -> Result<()>
where
    W: Fn(Value) -> BoxFuture<'static, Result<usize>>,
{
    let token = github_token().await;
    let token = token.as_deref();
    let base = api();
    let info: Repo = get(
        &format!("{base}/repos/{repo}"),
        token,
        "application/vnd.github+json",
    )
    .await?
    .json()
    .await?;
    let branch = info.default_branch;
    let tree: Tree = get(
        &format!("{base}/repos/{repo}/git/trees/{branch}?recursive=1"),
        token,
        "application/vnd.github+json",
    )
    .await?
    .json()
    .await?;
    if tree.truncated {
        eprintln!(
            "sources: skills {repo}: its tree is larger than GitHub lists at once; some skills may be missed"
        );
    }
    let files: Vec<&Entry> = tree
        .tree
        .iter()
        .filter(|e| e.kind == "blob" && is_skill(&e.path))
        .filter(|e| e.size.is_none_or(|s| s <= MAX_BYTES))
        .take(MAX_SKILLS)
        .collect();
    let now = chrono::Utc::now();
    for e in files {
        if p.versions.get(&e.path).is_some_and(|v| v.key == e.sha) {
            continue;
        }
        let body = get(
            &format!("{base}/repos/{repo}/contents/{}?ref={branch}", e.path),
            token,
            "application/vnd.github.raw+json",
        )
        .await?
        .text()
        .await?;
        if body.trim().is_empty() {
            continue;
        }
        let episode = json!({
            "source": format!("github:{repo}#skill/{}", e.path),
            "occurred_at": now.timestamp_micros(),
            "acl": acl,
            "about": [format!("repo:{repo}")],
            "index_text": true,
            "document": crate::skill_review::key_of(repo, &e.path),
            "body": body,
        });
        p.records += write(json!({ "episodes": [episode] })).await? as u64;
        p.versions.insert(
            e.path.clone(),
            Version {
                key: e.sha.clone(),
                seen: branch.clone(),
                at: now.timestamp(),
            },
        );
    }
    p.cursor = Some(now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    Ok(())
}
