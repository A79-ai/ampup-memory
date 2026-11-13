//! Advice from a repository's first session: what the repository already says about how its work is done, sent
//! to the memory (`POST /advice/seed`), which writes it into notes for the kinds of task coding sessions are routed
//! to (implementing a change, investigating a failure, reviewing or opening a pull request, …), served by
//! `get_advice` before any session there has been rewarded.
//!
//! **When.** In the copy a session's start hands its work to (never in the hook the person waits on), at most once
//! a day per repository and memory (server, org, team and scope), with a lock so sessions starting at once send it
//! once. The first time it is said in the session (the agent tells the user once); later days send it again
//! quietly, and the memory keeps (and writes notes from) only what changed: a file's new text, a newly merged pull
//! request. A memory that can't write advice (no model) is asked again a day later, and nothing is read or sent
//! meanwhile.
//!
//! **What.** Only what a person expects a coding agent to read, from files git tracks (so `.gitignore` holds), never
//! code: the agent instructions (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `.claude/rules`, `.cursor/rules`), the
//! repository's guides at its top or under `.github/` (`CONTRIBUTING`, `README`, `DEVELOPMENT`, `ARCHITECTURE`,
//! `TESTING`, style and release guides, the pull request template), and up to [`DOCS`] Markdown files under
//! `docs/`; the person's own Claude Code memory files for the repository (what their past sessions there kept);
//! and the last [`PRS`] merged pull requests (title, description, changed files, reviews and review comments, bots'
//! left out) through `gh` where it is signed in, else the default branch's last [`COMMITS`] commits' messages and
//! changed files. Each file at most [`MAX_TEXT`] bytes, [`MAX_TOTAL`] in all; a link out of the repository is
//! not followed; secret-looking words are redacted. `AMPUP_MEMORY_REPO_ADVICE=off` (or `AMPUP_MEMORY_CAPTURE=off`)
//! sends nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use super::{Client, git_repo, redact, repo_root, starter_files};

/// `off` (or `0`, `false`, `no`): no repository's material is sent.
pub(super) const OFF_VAR: &str = "AMPUP_MEMORY_REPO_ADVICE";
/// How long a repository's material isn't looked at again, once sent or found not compilable.
const EVERY: Duration = Duration::from_secs(86_400);
/// A start that holds the lock longer than this is taken to have died.
const LOCK_STALE: Duration = Duration::from_secs(600);
/// Longest text of one source (the server takes 32 KB), and of all of them (it takes 600 KB).
const MAX_TEXT: usize = 30_000;
const MAX_TOTAL: usize = 500_000;
/// A file larger than this is generated or vendored, not a guide: left out. A smaller one past [`MAX_TEXT`] is cut.
const MAX_FILE: u64 = 1024 * 1024;
/// Most Markdown files from `docs/`, merged pull requests, and commits when there are none.
const DOCS: usize = 15;
const PRS: usize = 25;
const COMMITS: usize = 60;
/// How long `gh` and `git` may take, each call.
const TOOL_WAIT: Duration = Duration::from_secs(20);
/// A pull request's description, and one review or comment, at most.
const PR_BODY: usize = 6_000;
const REVIEW_TEXT: usize = 800;

fn off() -> bool {
    let off = |k: &str| {
        std::env::var(k).is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false" | "no"
            )
        })
    };
    off(OFF_VAR) || off("AMPUP_MEMORY_CAPTURE")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn hex(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_be_bytes());
        h.update(p.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// What was last sent for a repository to a memory: when, each source's hash, and why it wasn't, if it wasn't.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Sent {
    at: u64,
    #[serde(default)]
    sources: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    why: Option<String>,
}

/// One source: its name (a path in the repository, `pr/<n>`, `commits/<sha>`, `memory/<file>`) and its text.
struct Source {
    name: String,
    text: String,
}

/// Sends the session's repository's material once a day ([`self`]), and says what it read the first time it does,
/// or why it read nothing the first time it can't. None when there was nothing to say.
pub(super) async fn seed(client: &Client, input: &Value) -> Result<Option<String>> {
    if off() {
        return Ok(None);
    }
    let Some(repo) = git_repo(input) else {
        return Ok(None);
    };
    let root = repo_root(input);
    let home = std::env::var_os("HOME").and_then(|h| Path::new(&h).canonicalize().ok());
    if root.canonicalize().ok() == home {
        return Ok(None);
    }
    let dir = crate::client::config_dir().join("repo-advice");
    std::fs::create_dir_all(&dir)?;
    let key = hex(&[
        client.url.as_str(),
        client.org.as_deref().unwrap_or(""),
        client.team.as_deref().unwrap_or(""),
        client.scope.as_deref().unwrap_or(""),
        repo.as_str(),
    ]);
    let file = dir.join(format!("{key}.json"));
    let before: Sent = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if now_secs().saturating_sub(before.at) < EVERY.as_secs() {
        return Ok(None);
    }
    let Some(_lock) = Lock::take(&dir.join(format!("{key}.lock"))) else {
        return Ok(None);
    };
    let save = |s: &Sent| -> Result<()> {
        let tmp = file.with_extension(format!("json.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(s)?)?;
        std::fs::rename(tmp, &file)?;
        Ok(())
    };
    // First whether the memory compiles at all: where it can't, nothing is read.
    let check = match client.post("/advice/seed", &json!({"check": true})).await {
        Ok(v) => v,
        // A server from before this route: asked again in a day, quietly.
        Err(e) if [": 404", ": 405"].iter().any(|s| e.to_string().contains(s)) => {
            save(&Sent {
                at: now_secs(),
                why: Some("the server doesn't compile repositories' material".into()),
                ..before
            })?;
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    if check["advises"].as_bool() != Some(true) {
        let why = check["why"]
            .as_str()
            .unwrap_or("it can't write advice")
            .to_string();
        let said = before.why.as_deref() == Some(why.as_str());
        save(&Sent {
            at: now_secs(),
            why: Some(why.clone()),
            ..before
        })?;
        return Ok((!said).then(|| {
            format!(
                "AmpUp Memory read nothing from this repository to advise from: {why}. Once it has one, a \
                 session's start reads the repository's docs and recent pull requests (within a day) and advice \
                 works from then on. Tell the user once, briefly, then carry on with their request."
            )
        }));
    }
    let root2 = root.clone();
    let docs = tokio::task::spawn_blocking(move || docs(&root2)).await?;
    let mine = memory_files(input);
    let (history, from_prs) = history(&root).await;
    let mut all: Vec<Source> = docs
        .iter()
        .chain(&mine)
        .chain(&history)
        .map(|s| Source {
            name: s.name.clone(),
            text: redact(&s.text),
        })
        .collect();
    // Within what one call takes, the docs first.
    let mut total = 0;
    all.retain(|s| {
        total += s.text.len();
        total <= MAX_TOTAL
    });
    all.truncate(80);
    let hashes: BTreeMap<String, String> = all
        .iter()
        .map(|s| (s.name.clone(), hex(&[s.text.as_str()])))
        .collect();
    // All of it, every time: the memory keeps only what is new, and writes the notes from the whole.
    let sent: Vec<Value> = all
        .iter()
        .map(|s| json!({"name": s.name, "text": s.text}))
        .collect();
    let answer = client
        .post("/advice/seed", &json!({"repo": repo, "sources": sent}))
        .await?;
    let first = before.sources.is_empty();
    save(&Sent {
        at: now_secs(),
        sources: hashes,
        why: None,
    })?;
    if !first || all.is_empty() {
        return Ok(None);
    }
    let named: Vec<&str> = docs.iter().map(|s| s.name.as_str()).collect();
    let mut read = vec![];
    if !named.is_empty() {
        let shown: Vec<&str> = named.iter().take(4).copied().collect();
        read.push(match named.len() - shown.len() {
            0 => shown.join(", "),
            n => format!("{} and {n} other docs", shown.join(", ")),
        });
    }
    if !mine.is_empty() {
        read.push(format!(
            "{} of the user's own Claude Code memory files for it",
            mine.len()
        ));
    }
    if !history.is_empty() {
        read.push(match from_prs {
            true => format!(
                "the last {} merged pull requests (title, description, changed files, review comments)",
                history.len()
            ),
            false => "recent commit messages and the files they changed".to_string(),
        });
    }
    let writing = match answer["writing_notes"].as_bool() {
        Some(true) => {
            "is writing advice notes from it (implementing a change, investigating a failure, reviewing or opening a \
             pull request, planning, explaining code, running it locally) that get_advice serves in a minute or two"
        }
        _ => "keeps it",
    };
    Ok(Some(format!(
        "AmpUp Memory read this repository once so its advice works from the first session: {}. It is kept in the \
         user's own memory (no teammate reads it) and the memory {writing}; what changes is sent again at most once \
         a day. {OFF_VAR}=off turns this off. Tell the user once, in one sentence, then carry on with their request.",
        read.join("; ")
    )))
}

/// A lock file held while one start sends a repository's material, removed when dropped. None while another
/// start holds it (unless it is [`LOCK_STALE`]).
struct Lock(PathBuf);
impl Lock {
    fn take(path: &Path) -> Option<Lock> {
        let open = || {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
        };
        if open().is_ok() {
            return Some(Lock(path.to_path_buf()));
        }
        let stale = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|a| a > LOCK_STALE));
        if stale {
            let _ = std::fs::remove_file(path);
            return open().ok().map(|_| Lock(path.to_path_buf()));
        }
        None
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `text` cut to at most `n` bytes, at a character boundary, saying so.
fn cut(text: &str, n: usize) -> String {
    if text.len() <= n {
        return text.to_string();
    }
    let mut end = n.saturating_sub(16);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[… cut]", &text[..end])
}

/// A file of the repository whose name says it is a guide for working in it: at its top or under `.github/`.
fn is_guide(path: &str) -> bool {
    let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
    if !(dir.is_empty() || dir == ".github" || dir == "docs") {
        return false;
    }
    let stem = file
        .rsplit_once('.')
        .map_or(file, |(s, _)| s)
        .to_ascii_lowercase();
    [
        "readme",
        "contributing",
        "development",
        "developing",
        "hacking",
        "architecture",
        "testing",
        "style",
        "styleguide",
        "conventions",
        "release",
        "releasing",
        "pull_request_template",
    ]
    .contains(&stem.as_str())
}

/// The repository's guides and docs ([`self`]), as git tracks them under `root`, the agent instructions first.
fn docs(root: &Path) -> Vec<Source> {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--", "*.md", "*.mdc", "*.MD"])
        .output();
    let Some(out) = out.ok().filter(|o| o.status.success()) else {
        return vec![];
    };
    let files: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(String::from)
        .collect();
    let agent = |f: &str| {
        super::STARTER_REPO_FILES.contains(&f)
            || super::STARTER_REPO_RULES
                .iter()
                .any(|d| f.starts_with(&format!("{d}/")))
    };
    let vendored = |f: &str| {
        f.split('/').any(|p| {
            matches!(
                p,
                "node_modules" | "vendor" | "third_party" | "third-party" | ".venv" | "target"
            )
        })
    };
    let mut picked: Vec<&String> = files.iter().filter(|f| agent(f)).collect();
    let mut guides: Vec<&String> = files.iter().filter(|f| !agent(f) && is_guide(f)).collect();
    guides.sort_by_key(|f| (f.matches('/').count(), f.to_ascii_lowercase()));
    picked.extend(guides);
    let mut under_docs: Vec<&String> = files
        .iter()
        .filter(|f| {
            (f.starts_with("docs/") || f.starts_with("doc/"))
                && !is_guide(f)
                && !vendored(f)
                && f.matches('/').count() <= 3
        })
        .collect();
    // Guides for doing the work first (a `guide/` or `development/` folder, a testing or style doc), then the
    // shallowest.
    let howto = |f: &str| {
        let l = f.to_ascii_lowercase();
        [
            "guide",
            "develop",
            "contribut",
            "test",
            "style",
            "convention",
            "architect",
            "setup",
            "getting-started",
            "quickstart",
            "release",
            "deploy",
            "runbook",
        ]
        .iter()
        .any(|w| l.contains(w))
    };
    under_docs.sort_by_key(|f| (!howto(f), f.matches('/').count(), f.to_ascii_lowercase()));
    picked.extend(under_docs.into_iter().take(DOCS));
    let base = root.canonicalize().ok();
    let mut seen = std::collections::HashSet::new();
    picked
        .into_iter()
        .filter(|f| !f.contains(':') && !f.to_ascii_lowercase().contains("secret"))
        .filter_map(|f| {
            let path = root.join(f);
            // A link is read only when it stays in the repository.
            let real = path.canonicalize().ok()?;
            if !base.as_ref().is_some_and(|b| real.starts_with(b)) {
                return None;
            }
            let meta = std::fs::metadata(&real).ok()?;
            if !meta.is_file() || meta.len() > MAX_FILE {
                return None;
            }
            let text = std::fs::read_to_string(&real).ok()?;
            // The same text under two names (AGENTS.md linking to CLAUDE.md) once, under the first.
            if text.trim().is_empty() || !seen.insert(hex(&[text.as_str()])) {
                return None;
            }
            Some(Source {
                name: f.clone(),
                text: cut(&text, MAX_TEXT),
            })
        })
        .collect()
}

/// The person's own Claude Code memory files for the repository: what their past sessions there kept.
fn memory_files(input: &Value) -> Vec<Source> {
    starter_files(input)
        .into_iter()
        .filter(|(mine, _, _)| *mine)
        .filter_map(|(_, _, path)| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&path).ok()?;
            (!text.trim().is_empty() && !name.contains(':')).then(|| Source {
                name: format!("memory/{name}"),
                text: cut(&text, MAX_TEXT),
            })
        })
        .collect()
}

/// A command's output in `root`, or None when it fails or takes longer than [`TOOL_WAIT`].
async fn run(root: &Path, program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(root)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(TOOL_WAIT, cmd.output())
        .await
        .ok()?
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn is_bot(login: &str) -> bool {
    login.ends_with("[bot]") || login.ends_with("-bot") || login == "github-actions"
}

/// The repository's recent history: its last merged pull requests through `gh` (true), else its default branch's
/// last commits (false).
async fn history(root: &Path) -> (Vec<Source>, bool) {
    let prs = prs(root).await;
    if !prs.is_empty() {
        return (prs, true);
    }
    (commits(root).await, false)
}

async fn prs(root: &Path) -> Vec<Source> {
    let limit = PRS.to_string();
    let Some(list) = run(
        root,
        "gh",
        &[
            "pr",
            "list",
            "--state",
            "merged",
            "--limit",
            &limit,
            "--json",
            "number,title,body,mergedAt,author,files,reviews",
        ],
    )
    .await
    else {
        return vec![];
    };
    let Ok(Value::Array(list)) = serde_json::from_str::<Value>(&list) else {
        return vec![];
    };
    // The newest review comments on code, across pull requests, in one call: each under its pull request.
    let mut inline: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    if let Some(c) = run(
        root,
        "gh",
        &[
            "api",
            "repos/{owner}/{repo}/pulls/comments?sort=created&direction=desc&per_page=100",
        ],
    )
    .await
        && let Ok(Value::Array(cs)) = serde_json::from_str::<Value>(&c)
    {
        for c in cs {
            let who = c["user"]["login"].as_str().unwrap_or_default();
            let n = c["pull_request_url"]
                .as_str()
                .and_then(|u| u.rsplit('/').next())
                .and_then(|n| n.parse::<u64>().ok());
            let (Some(n), Some(body)) = (n, c["body"].as_str()) else {
                continue;
            };
            if is_bot(who) || body.trim().is_empty() {
                continue;
            }
            let path = c["path"].as_str().unwrap_or_default();
            inline.entry(n).or_default().push(format!(
                "- {who} on {path}: {}",
                cut(body.trim(), REVIEW_TEXT)
            ));
        }
    }
    list.iter()
        .filter_map(|pr| {
            let n = pr["number"].as_u64()?;
            let title = pr["title"].as_str().unwrap_or_default();
            let author = pr["author"]["login"].as_str().unwrap_or_default();
            let mut text = format!(
                "Pull request #{n}: {title}\nMerged {} by {author}\n\n{}\n",
                pr["mergedAt"].as_str().unwrap_or_default(),
                cut(pr["body"].as_str().unwrap_or_default().trim(), PR_BODY)
            );
            let files: Vec<&str> = pr["files"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f["path"].as_str())
                .collect();
            if !files.is_empty() {
                text.push_str("\n## Files changed\n");
                for f in files.iter().take(40) {
                    text.push_str(&format!("- {f}\n"));
                }
                if files.len() > 40 {
                    text.push_str(&format!("- and {} more\n", files.len() - 40));
                }
            }
            let mut reviews: Vec<String> = pr["reviews"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| {
                    let who = r["author"]["login"].as_str().unwrap_or_default();
                    let body = r["body"].as_str()?.trim();
                    (!is_bot(who) && !body.is_empty()).then(|| {
                        format!(
                            "- {who} ({}): {}",
                            r["state"].as_str().unwrap_or("reviewed").to_lowercase(),
                            cut(body, REVIEW_TEXT)
                        )
                    })
                })
                .collect();
            reviews.extend(inline.remove(&n).unwrap_or_default());
            if !reviews.is_empty() {
                text.push_str("\n## Review\n");
                text.push_str(&reviews.join("\n"));
                text.push('\n');
            }
            Some(Source {
                name: format!("pr/{n}"),
                text: cut(&text, MAX_TEXT),
            })
        })
        .collect()
}

/// The default branch's last [`COMMITS`] commits (its origin's, else this checkout's HEAD), twenty to a source:
/// each one's subject, message and changed files. Authors by name, never by address.
async fn commits(root: &Path) -> Vec<Source> {
    let branch = run(
        root,
        "git",
        &["rev-parse", "--abbrev-ref", "--verify", "-q", "origin/HEAD"],
    )
    .await
    .map(|b| b.trim().to_string())
    .filter(|b| !b.is_empty())
    .unwrap_or_else(|| "HEAD".into());
    let n = COMMITS.to_string();
    let Some(log) = run(
        root,
        "git",
        &[
            "-c",
            "core.fsmonitor=false",
            "log",
            &branch,
            "-n",
            &n,
            "--no-merges",
            "--name-only",
            "--format=%x1e%h%x1f%an%x1f%as%x1f%s%x1f%b%x1f",
        ],
    )
    .await
    else {
        return vec![];
    };
    let entries: Vec<String> = log
        .split('\u{1e}')
        .filter(|e| !e.trim().is_empty())
        .filter_map(|e| {
            let f: Vec<&str> = e.splitn(6, '\u{1f}').collect();
            if f.len() < 6 {
                return None;
            }
            let files: Vec<&str> = f[5].lines().filter(|l| !l.trim().is_empty()).collect();
            let mut s = format!(
                "commit {} ({}, {}): {}\n{}",
                f[0],
                f[1],
                f[2],
                f[3],
                cut(f[4].trim(), 1_500)
            );
            if !files.is_empty() {
                let shown: Vec<&str> = files.iter().take(20).copied().collect();
                s.push_str(&format!("\nFiles: {}", shown.join(", ")));
                if files.len() > 20 {
                    s.push_str(&format!(" and {} more", files.len() - 20));
                }
            }
            Some(s)
        })
        .collect();
    entries
        .chunks(20)
        .filter_map(|c| {
            let first = c.first()?.split_whitespace().nth(1)?.to_string();
            Some(Source {
                name: format!("commits/{first}"),
                text: cut(&c.join("\n\n"), MAX_TEXT),
            })
        })
        .collect()
}
