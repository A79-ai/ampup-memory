//! Keeping the client current with nothing for the person to run. A session's start checks, at most hourly and in
//! a detached copy of this process (it never holds the session up), whether a newer release is out and installs it
//! over this binary; the next hook runs the new one. A sign-in checks every time and waits for it, then runs the
//! new binary: a fix to signing in reaches the person at the moment they sign in. Whatever plugin version runs it,
//! a client keeps itself current from then on.
//!
//! Client releases are the repository's `client-v*` tags (`client-v0.1.7`, built as `v0.1.7`); its `v*` tags are
//! server releases, which this never installs. The newest is the highest `client-v` version among its releases,
//! leaving out drafts and pre-releases.
//!
//! The install is the repository's `install.sh` at the release's tag (the plugin's own copy where a hook names
//! `CLAUDE_PLUGIN_ROOT`), with `AMPUP_MEMORY_INSTALL=client`, which checks the archive's checksum and moves the binary
//! into place whole. A build that isn't a release (`cargo build`), a pinned `AMPUP_MEMORY_VERSION`, or
//! `AMPUP_MEMORY_CLIENT_UPDATE=off` is left as it is.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

/// Set in the binary a sign-in runs after updating, so it doesn't update again.
const UPDATED: &str = "AMPUP_MEMORY_CLIENT_UPDATED";

/// The repository whose releases hold the client (`AMPUP_MEMORY_CLIENT_REPO`, for a fork).
fn repo() -> String {
    std::env::var("AMPUP_MEMORY_CLIENT_REPO").unwrap_or_else(|_| "A79-ai/ampup-memory".into())
}

/// The tag prefix of a client release.
const CLIENT_TAG: &str = "client-";

/// `v1.2.3` (or `1.2.3`) as numbers; None for anything else, a pre-release (`v1.2.3-rc.1`) too.
fn version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.strip_prefix('v').unwrap_or(v).split('.');
    let n = |p: Option<&str>| p?.parse::<u64>().ok();
    let got = (n(parts.next())?, n(parts.next())?, n(parts.next())?);
    parts.next().is_none().then_some(got)
}

/// The newest client release among a releases listing's (`GET /repos/{repo}/releases`): its tag and version.
fn newest_client(releases: &serde_json::Value) -> Option<(String, (u64, u64, u64))> {
    releases
        .as_array()?
        .iter()
        .filter(|r| r["draft"] != true && r["prerelease"] != true)
        .filter_map(|r| {
            let tag = r["tag_name"].as_str()?;
            Some((tag.to_string(), version(tag.strip_prefix(CLIENT_TAG)?)?))
        })
        .max_by_key(|(_, v)| *v)
}

/// A client release (`v0.1.7`) only: not a build that isn't one, nor the copy a server release bundles
/// (`v0.1.0-rc.22`), which the public client releases would replace with a different program's version.
fn enabled(current: &str) -> bool {
    current.starts_with('v')
        && !current.contains('-')
        && std::env::var_os("AMPUP_MEMORY_VERSION").is_none()
        && std::env::var("AMPUP_MEMORY_CLIENT_UPDATE").as_deref() != Ok("off")
        && std::env::var_os(UPDATED).is_none()
}

fn stamp() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".cache")
        });
    cache.join("memstore").join("update-checked")
}

/// The newest client release: its tag (`client-v0.1.7`) and version, from GitHub's list of the repository's releases
/// (its newest 100 releases, server and client together).
async fn latest() -> Option<(String, (u64, u64, u64))> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(crate::naming::CLIENT_BIN)
        .build()
        .ok()?;
    let releases: serde_json::Value = http
        .get(format!(
            "https://api.github.com/repos/{}/releases?per_page=100",
            repo()
        ))
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    newest_client(&releases)
}

/// Installs release `tag` into `dir`; whether it did.
fn install(tag: &str, dir: &Path) -> bool {
    let local = std::env::var_os("CLAUDE_PLUGIN_ROOT")
        .map(|r| PathBuf::from(r).join("bin").join("install.sh"))
        .filter(|p| p.is_file());
    let script = match &local {
        Some(p) => format!("sh '{}'", p.display()),
        None => format!(
            "curl -fsSL --retry 3 https://raw.githubusercontent.com/{}/{tag}/install.sh | sh",
            repo()
        ),
    };
    let repo = repo();
    let mut sh = Command::new("sh");
    sh.args(["-c", &script]);
    // By both names: the plugin's copy of install.sh may be from before the rename, and read only the old.
    for (name, value) in [
        ("AMPUP_MEMORY_VERSION", OsStr::new(tag)),
        ("AMPUP_MEMORY_INSTALL", OsStr::new("client")),
        ("AMPUP_MEMORY_REPO", OsStr::new(&repo)),
        ("AMPUP_MEMORY_CLIENT_REPO", OsStr::new(&repo)),
        ("AMPUP_MEMORY_INSTALL_DIR", dir.as_os_str()),
    ] {
        sh.env(name, value);
        sh.env(crate::naming::old_env_name(name).as_ref(), value);
    }
    sh.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The client binary an install left in `dir`: `ampup-memory-client`, with `memstore-client` a link to it, from an
/// install.sh since the rename; `memstore-client` alone, a file, from one before it (the plugin's older copy, which
/// installs the release's `memstore-client` archive).
fn installed(dir: &Path) -> PathBuf {
    let old = dir.join(crate::naming::OLD_CLIENT_BIN);
    match std::fs::symlink_metadata(&old) {
        Ok(m) if m.is_file() => old,
        _ => dir.join(crate::naming::CLIENT_BIN),
    }
}

/// Checks for a newer client release than `current` and installs it over this binary; the binary installed, if it
/// did.
pub async fn now(current: &str) -> Option<PathBuf> {
    if !enabled(current) {
        return None;
    }
    let stamp = stamp();
    if let Some(d) = stamp.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(&stamp, b"");
    let (tag, newest) = latest().await?;
    if Some(newest) <= version(current) {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    install(&tag, dir).then(|| installed(dir))
}

/// At a session's start: at most hourly, a detached copy of this binary checks and installs (`update`), and the
/// hook goes on at once.
pub fn in_background(current: &str) {
    if !enabled(current) {
        return;
    }
    let fresh = std::fs::metadata(stamp())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < Duration::from_secs(3600));
    if fresh {
        return;
    }
    let _ = std::fs::write(stamp(), b"");
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe)
            .arg("update")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

/// Before a sign-in: updates, and where it did, runs the new binary with the same arguments in this one's place.
pub async fn before_sign_in(current: &str) {
    let Some(new) = now(current).await else {
        return;
    };
    use std::os::unix::process::CommandExt;
    let err = Command::new(new)
        .args(std::env::args_os().skip(1))
        .env(UPDATED, current)
        .exec();
    // Couldn't run it: carry on with this one.
    eprintln!(
        "updated {}, but couldn't run it: {err}",
        crate::naming::CLIENT_BIN
    );
}
