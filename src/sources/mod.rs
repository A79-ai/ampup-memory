//! Sources a `serve --local` server reads by itself, with the credentials already on this machine: no account
//! with anyone, no webhook, no public address. `ampup-memory sources add KIND TARGET` names one in
//! `DIR/sources.json`; the server reads that file every minute and each source on it every [`EVERY`], writing
//! what changed as episodes through the same path `/ingest` takes (redaction, the episode types' checks, and a
//! record sent again as it was being taken without a write).
//!
//! Each kind is a module here ([`github`], [`github_prs`], [`git`], [`linear`], [`sentry`], [`slack`]): it makes
//! its [`Source`] from the command line's target and reads it from its [`Progress`]. Adding one is that module,
//! its variant of [`Source`], and a line each in [`KINDS`], [`add`] and the round's dispatch.
//!
//! **Who reads it.** The groups the server reads as (`group:local` by default): on `serve --local` that's the one
//! person whose machine it is.

pub mod git;
pub mod github;
pub mod github_prs;
pub mod linear;
pub mod sentry;
pub mod slack;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// How often each source is read.
pub const EVERY: Duration = Duration::from_secs(600);
/// How often the list of sources is looked at again (a source just added is read within this).
const TICK: Duration = Duration::from_secs(60);
/// How far back a source just added is read, unless `--since` says otherwise.
pub const BACKFILL_DAYS: i64 = 30;
/// Pages of 100 read from one source in one round.
pub(crate) const MAX_PAGES: u32 = 50;
/// How far before the newest change last read a round starts again.
pub(crate) const OVERLAP: chrono::Duration = chrono::Duration::hours(1);
/// How far back the day's wider round starts, and how often it runs.
pub(crate) const SWEEP: chrono::Duration = chrono::Duration::days(4);
pub(crate) const SWEEP_EVERY: i64 = 86_400;
/// Records written per call (the ingest path takes at most 500).
pub(crate) const BATCH: usize = 100;

const CONFIG: &str = "sources.json";
const STATE: &str = "sources-state.json";

#[derive(Serialize, Deserialize, Default)]
struct Config {
    #[serde(default)]
    sources: Vec<Source>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    Github {
        repo: String,
        /// RFC 3339: nothing last changed before it is read.
        since: String,
    },
    #[serde(rename = "github-prs")]
    GithubPrs { repo: String, since: String },
    Git {
        /// The work tree's top directory, absolute.
        path: String,
        since: String,
    },
    Linear {
        /// The workspace's URL key, as in its address.
        workspace: String,
        team: String,
        /// RFC 3339: nothing last changed before it is read.
        since: String,
    },
    Sentry {
        /// `ORG/PROJECT`.
        project: String,
        /// RFC 3339: no issue whose last event is before it is read.
        since: String,
    },
    Slack {
        /// The channel's name (no `#`) or id, as `sources add` was given it.
        channel: String,
        /// RFC 3339: nothing posted before it is read.
        since: String,
    },
}

impl Source {
    pub fn key(&self) -> String {
        match self {
            Source::Github { repo, .. } => format!("github:{repo}"),
            Source::GithubPrs { repo, .. } => format!("github-prs:{repo}"),
            Source::Git { path, .. } => format!("git:{path}"),
            Source::Linear {
                workspace, team, ..
            } => format!("linear:{workspace}/{team}"),
            Source::Sentry { project, .. } => format!("sentry:{project}"),
            Source::Slack { channel, .. } => format!("slack:{channel}"),
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Progress {
    /// Where the next round reads from: the newest change read (GitHub's `updated_at`, Linear's `updatedAt`; for
    /// git, the last commit read), or the Slack `ts` a round read everything up to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cursor: Option<String>,
    /// When it was last read, in seconds since the epoch.
    #[serde(default)]
    pub(crate) read_at: i64,
    /// When a round last started [`SWEEP`] back, in seconds since the epoch.
    #[serde(default)]
    pub(crate) swept_at: i64,
    /// The last round was cut short: the next starts at the cursor itself, not [`OVERLAP`] before it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) partial: bool,
    /// Records sent to memory (one already held as sent is taken without a write).
    #[serde(default)]
    pub(crate) records: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    /// Per record, the version last written, for a kind whose records change without a newer time to read them
    /// from (Sentry's issues: a status or an assignee).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) versions: BTreeMap<String, Version>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Version {
    /// What a change is told by (Sentry: status, assignee, comments, title, culprit, level).
    pub(crate) key: String,
    /// The record's own time when written (Sentry: its last event).
    pub(crate) seen: String,
    /// When it was written, in seconds since the epoch.
    pub(crate) at: i64,
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            serde_json::from_str(&s).with_context(|| format!("{} isn't valid", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Written whole and renamed into place, so the server never reads half a file.
fn write_json(path: &Path, v: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The kinds `sources add` takes.
pub const KINDS: &[&str] = &["github", "github-prs", "git", "linear", "sentry", "slack"];

/// `sources add KIND TARGET [--since DATE]`: kept in `dir`; a running server reads it within a minute.
pub fn add(dir: &Path, kind: &str, target: &str, since: Option<&str>) -> Result<Source> {
    let since = match since {
        Some(d) => parse_since(d)?,
        None => chrono::Utc::now() - chrono::Duration::days(BACKFILL_DAYS),
    }
    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let source = match kind {
        "github" => github::source(target, since)?,
        "github-prs" => github_prs::source(target, since)?,
        "git" => git::source(target, since)?,
        "linear" => linear::source(target, since)?,
        "sentry" => sentry::source(target, since)?,
        "slack" => slack::source(target, since)?,
        _ => bail!("{kind:?} isn't a kind of source ({})", KINDS.join(", ")),
    };
    std::fs::create_dir_all(dir)?;
    let path = dir.join(CONFIG);
    let mut config: Config = read_json(&path)?;
    if config.sources.iter().any(|s| s.key() == source.key()) {
        bail!(
            "{} is already a source (`sources remove {kind} {target}` first)",
            source.key()
        );
    }
    config.sources.push(source.clone());
    write_json(&path, &config)?;
    Ok(source)
}

fn parse_since(d: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(d) {
        return Ok(t.with_timezone(&chrono::Utc));
    }
    let day = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
        .with_context(|| format!("--since {d:?} isn't a date (YYYY-MM-DD) or an RFC 3339 time"))?;
    Ok(day.and_hms_opt(0, 0, 0).unwrap().and_utc())
}

/// `sources remove KIND TARGET`: no longer read. What it wrote stays in memory.
pub fn remove(dir: &Path, kind: &str, target: &str) -> Result<bool> {
    let path = dir.join(CONFIG);
    let mut config: Config = read_json(&path)?;
    let key = match kind {
        // As `add` kept it: the work tree's top directory, absolute (as given where it's gone).
        "git" => format!(
            "git:{}",
            git::top(target).unwrap_or_else(|_| target.to_string())
        ),
        // As `add` kept it: the channel without its `#`.
        "slack" => format!("slack:{}", target.trim().trim_start_matches('#')),
        _ => format!("{kind}:{target}"),
    };
    let before = config.sources.len();
    config.sources.retain(|s| s.key() != key);
    if config.sources.len() == before {
        return Ok(false);
    }
    write_json(&path, &config)?;
    let state_path = dir.join(STATE);
    let mut state: BTreeMap<String, Progress> = read_json(&state_path)?;
    if state.remove(&key).is_some() {
        write_json(&state_path, &state)?;
    }
    Ok(true)
}

/// `sources list`: each source, how many records it has sent, when it was last read and what went wrong then.
pub fn list(dir: &Path) -> Result<Vec<Value>> {
    let config: Config = read_json(&dir.join(CONFIG))?;
    let state: BTreeMap<String, Progress> = read_json(&dir.join(STATE))?;
    Ok(config
        .sources
        .iter()
        .map(|s| {
            let p = state.get(&s.key()).cloned().unwrap_or_default();
            let mut v = serde_json::to_value(s).unwrap_or_default();
            v["records"] = json!(p.records);
            v["cursor"] = json!(p.cursor);
            v["read_at"] = match p.read_at {
                0 => Value::Null,
                t => json!(chrono::DateTime::from_timestamp(t, 0).map(|t| t.to_rfc3339())),
            };
            if let Some(e) = p.error {
                v["error"] = json!(e);
            }
            v
        })
        .collect())
}

pub(crate) fn http() -> &'static reqwest::Client {
    static HTTP: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .user_agent(concat!("ampup-memory/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("an HTTP client")
    });
    &HTTP
}

pub(crate) fn micros(rfc3339: &str) -> Result<i64> {
    Ok(chrono::DateTime::parse_from_rfc3339(rfc3339)?.timestamp_micros())
}

/// The loop `serve --local` runs: every minute, each source due is read and written through `write` (the server's
/// ingest), as `acl`'s readers.
pub async fn run<W>(dir: PathBuf, acl: Vec<String>, write: W)
where
    W: Fn(Value) -> BoxFuture<'static, Result<usize>>,
{
    loop {
        if let Err(e) = round(&dir, &acl, &write).await {
            eprintln!("sources: {e:#}");
        }
        tokio::time::sleep(TICK).await;
    }
}

async fn round<W>(dir: &Path, acl: &[String], write: &W) -> Result<()>
where
    W: Fn(Value) -> BoxFuture<'static, Result<usize>>,
{
    let config: Config = read_json(&dir.join(CONFIG))?;
    if config.sources.is_empty() {
        return Ok(());
    }
    let state_path = dir.join(STATE);
    let now = chrono::Utc::now().timestamp();
    for source in &config.sources {
        let key = source.key();
        let state: BTreeMap<String, Progress> = read_json(&state_path)?;
        let mut p = state.get(&key).cloned().unwrap_or_default();
        if now - p.read_at < EVERY.as_secs() as i64 {
            continue;
        }
        let before = p.records;
        let read = match source {
            Source::Github { repo, since } => {
                let merged_elsewhere = config
                    .sources
                    .iter()
                    .any(|s| matches!(s, Source::GithubPrs { repo: r, .. } if r == repo));
                github::read(repo, since, merged_elsewhere, &mut p, acl, write).await
            }
            Source::GithubPrs { repo, since } => {
                github_prs::read(repo, since, &mut p, acl, write).await
            }
            Source::Git { path, since } => git::read(path, since, &mut p, acl, write).await,
            Source::Linear {
                workspace,
                team,
                since,
            } => linear::read(workspace, team, since, &mut p, acl, write).await,
            Source::Sentry { project, since } => {
                sentry::read(project, since, &mut p, acl, write).await
            }
            Source::Slack { channel, since } => {
                slack::read(channel, since, &mut p, acl, write).await
            }
        };
        p.read_at = now;
        p.error = read.as_ref().err().map(|e| format!("{e:#}"));
        match &read {
            Ok(()) => eprintln!("sources: {key}: {} records read", p.records - before),
            Err(e) => eprintln!("sources: {e:#}"),
        }
        // Re-read: `sources remove` may have dropped it meanwhile.
        let mut state: BTreeMap<String, Progress> = read_json(&state_path)?;
        let still: Config = read_json(&dir.join(CONFIG))?;
        if still.sources.iter().any(|s| s.key() == key) {
            state.insert(key, p);
            write_json(&state_path, &state)?;
        }
    }
    Ok(())
}
