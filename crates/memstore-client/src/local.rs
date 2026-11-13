//! The local server (`ampup-memory serve --local` on 127.0.0.1:[`PORT`]): where the hooks go when nobody named a
//! server or signed in to one. A session's start (or the MCP bridge, which an agent may start first) starts it when
//! nothing answers there ([`autostart`]), so nobody keeps a terminal open for it. Started as
//! `serve --local --background`, it serves the same directory (`~/.ampup-memory`) with the same model (`llm.json`
//! there) and the same settings ([`carried_env`]) as `ampup-memory service install`'s service, so either one
//! serves the memory the other kept. Where no model is saved yet, the first start saves the one the agent's
//! environment names, as `service install` does (`ampup-memory service save-model`); the server itself never gets
//! a key in its environment.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

/// The port the hooks and the plugin call when nobody signed in.
pub const PORT: u16 = 8080;
/// Where the hooks go when nothing names a server.
pub const URL: &str = "http://127.0.0.1:8080";
/// Names a file a process writes its output (stdout and stderr) to, capped: set by the service's unit and by
/// `serve --local --background`.
pub const LOG_VAR: &str = "AMPUP_MEMORY_LOG_FILE";
/// `0` (or `off`, `false`, `no`): a session's start never starts the local server.
pub const AUTOSTART_VAR: &str = "AMPUP_MEMORY_AUTOSTART";
/// Held while a start is under way, by the starter `serve --local --background` until its server is ready (or it
/// gives up), so sessions starting at once start one server between them.
const LOCK: &str = "autostart.lock";
/// How long a session's start looks for something answering on the port: a refused connection on this machine
/// answers at once.
const PROBE: Duration = Duration::from_millis(300);

/// The settings a local server started from here gets: `AMPUP_MEMORY_*` from this environment, but for the
/// model's (kept in `llm.json`), the client's own and the installer's; and the names of those left out as secrets.
pub fn carried_env() -> (Vec<(String, String)>, Vec<String>) {
    const NOT_THE_SERVERS: &[&str] = &[
        "AMPUP_MEMORY_LLM",
        "AMPUP_MEMORY_LLM_URL",
        "AMPUP_MEMORY_LLM_MODEL",
        "AMPUP_MEMORY_LLM_KEY",
        "AMPUP_MEMORY_URL",
        "AMPUP_MEMORY_TOKEN",
        "AMPUP_MEMORY_CONTEXT",
        "AMPUP_MEMORY_ORG",
        "AMPUP_MEMORY_TEAM",
        "AMPUP_MEMORY_CONFIG_DIR",
        "AMPUP_MEMORY_CLIENT_UPDATE",
        "AMPUP_MEMORY_SCOPE",
        "AMPUP_MEMORY_VERSION",
        "AMPUP_MEMORY_REPO",
        AUTOSTART_VAR,
        LOG_VAR,
    ];
    let mut carried = vec![];
    let mut secret = vec![];
    for (k, v) in std::env::vars() {
        if !k.starts_with("AMPUP_MEMORY_")
            || NOT_THE_SERVERS.contains(&k.as_str())
            || k.starts_with("AMPUP_MEMORY_INSTALL")
            || v.contains('\n')
        {
            continue;
        }
        let names_a_file = k.ends_with("_FILE");
        if !names_a_file
            && ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "DSN"]
                .iter()
                .any(|w| k.contains(w))
        {
            secret.push(k);
        } else {
            carried.push((k, v));
        }
    }
    carried.sort();
    secret.sort();
    (carried, secret)
}

/// The rest of the environment a server started from here keeps: where home is, what runs, the locale, and how
/// it reaches the network (a proxy, certificates, the embedding model's files). Never a model key: the server
/// reads its model from `llm.json`, as the service does, so a key in an agent's environment isn't spent unasked.
fn kept(name: &str) -> bool {
    const KEEP: &[&str] = &[
        "HOME",
        "PATH",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "LANG",
        "TZ",
        "HF_HOME",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ];
    KEEP.contains(&name.to_ascii_uppercase().as_str())
        || name.starts_with("LC_")
        || name.starts_with("XDG_")
}

/// Whether a session's start may start the local server: on unless [`AUTOSTART_VAR`] says no.
fn wanted() -> bool {
    let v = std::env::var(AUTOSTART_VAR).unwrap_or_default();
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "0" | "off" | "false" | "no"
    )
}

/// Whether anything accepts a connection on 127.0.0.1:[`PORT`]: an AmpUp Memory server, or another program
/// holding the port (which a start would fail on, so it isn't tried).
fn answers() -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], PORT));
    std::net::TcpStream::connect_timeout(&addr, PROBE).is_ok()
}

/// The server binary: this one where it is the full CLI (a harness's hooks installed by `ampup-memory hooks
/// install`), else `ampup-memory` where the installers put it (`AMPUP_MEMORY_INSTALL_DIR`, else `~/.local/bin`,
/// where uv and pipx put it too), else on the PATH. The client alone (the plugin's) serves nothing.
fn server_bin() -> Option<PathBuf> {
    let me = std::env::current_exe().ok();
    if let Some(me) = me.filter(|p| p.file_name().is_some_and(|n| n == crate::naming::BIN)) {
        return Some(me);
    }
    let file = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.is_file());
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    let install_dir = var("AMPUP_MEMORY_INSTALL_DIR")
        .map(PathBuf::from)
        .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".local/bin")));
    let installed = install_dir.map(|d| d.join(crate::naming::BIN));
    if let Some(b) = installed.filter(|b| file(b)) {
        return Some(b);
    }
    std::env::split_paths(&var("PATH")?)
        .map(|d| d.join(crate::naming::BIN))
        .find(|b| file(b))
}

/// The directory a local server keeps its memory in and its log under: `~/.ampup-memory` (still `~/.memstore`
/// where moving it failed), as `serve --local` and `service install` default to.
fn data_dir() -> PathBuf {
    crate::naming::current_state_dir()
}

/// Opens [`LOCK`] in `dir`, locked: None where a start is already under way.
fn lock(dir: &Path) -> Result<Option<std::fs::File>> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK))
        .with_context(|| format!("{}", dir.join(LOCK).display()))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// What a session's start found of the local server.
#[derive(Debug, PartialEq)]
pub enum Start {
    /// Something answers on the port already, or a start is under way.
    Answering,
    /// Started now, by this pid (the starter, which returns once its server is ready), learning with this model.
    Started(u32, Model),
    /// [`AUTOSTART_VAR`] says no.
    Off,
    /// No server binary on this machine: the plugin installs the client alone.
    NotInstalled,
}

/// The model a server started from here learns with.
#[derive(Debug, PartialEq)]
pub enum Model {
    /// The one saved in `llm.json` before (by `service install`, a first `serve --local`, an earlier start).
    Kept,
    /// Saved now from the agent's environment: what was saved and where, in words (never the key).
    Saved(String),
    /// None: the server keeps and searches memory but learns nothing.
    Missing,
}

/// How long saving the model may take: a binary reading its environment and writing a file.
const SAVE_WITHIN: Duration = Duration::from_secs(5);

/// Where `dir` has no `llm.json`, the model this environment names, saved there by `bin service save-model`: the
/// code `service install` saves it with, in a process of its own that ends at once, so the key never reaches the
/// server's environment or command line. A model saved before is never touched.
fn save_model(bin: &Path, dir: &Path) -> Model {
    let saved = || std::fs::symlink_metadata(dir.join("llm.json")).is_ok();
    if saved() {
        return Model::Kept;
    }
    let child = std::process::Command::new(bin)
        .args(["service", "save-model"])
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return Model::Missing;
    };
    let started = std::time::Instant::now();
    let done = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.success(),
            Ok(None) if started.elapsed() < SAVE_WITHIN => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let mut said = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut out, &mut said);
    }
    let said = said.trim();
    match done && !said.is_empty() && saved() {
        true => Model::Saved(said.to_string()),
        false => Model::Missing,
    }
}

/// At a session's start, where the memory is the local default: nothing answering on 127.0.0.1:[`PORT`], starts
/// `ampup-memory serve --local --background` detached and returns at once (the starter waits for its server, not
/// the session). Sessions starting at once start one: the starter holds [`LOCK`] until its server is ready, and
/// a start that finds it held leaves it to that one.
pub fn autostart() -> Result<Start> {
    if !wanted() {
        return Ok(Start::Off);
    }
    if answers() {
        return Ok(Start::Answering);
    }
    let Some(bin) = server_bin() else {
        return Ok(Start::NotInstalled);
    };
    let dir = data_dir();
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs).with_context(|| format!("{}", logs.display()))?;
    let Some(held) = lock(&dir)? else {
        return Ok(Start::Answering);
    };
    // Checked again holding the lock: a start that finished between the first look and the lock left a server.
    if answers() {
        return Ok(Start::Answering);
    }
    let model = save_model(&bin, &dir);
    // The starter's own lines (a server that stopped at once, a port taken meanwhile) go where the server's do.
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("serve.log"))?;
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["serve", "--local", "--background"])
        .current_dir(&dir)
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| kept(k)))
        .envs(carried_env().0)
        // The lock, held by the starter (its stdin) until it exits: once its server is ready, or it gave up.
        .stdin(held)
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group: a harness ending the hook's group doesn't end the start.
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("couldn't start {}", bin.display()))?;
    Ok(Start::Started(child.id(), model))
}

/// In the copy a session's start hands its calls to: where a start is under way, waits for it (its server ready,
/// or the starter given up) up to `within`, so the session opens on the server it started rather than failing at
/// once. Nothing to wait for: returns at once.
pub async fn wait_for_start(within: Duration) {
    let dir = data_dir();
    let started = std::time::Instant::now();
    while started.elapsed() < within {
        match lock(&dir) {
            Ok(None) => tokio::time::sleep(Duration::from_millis(300)).await,
            _ => return,
        }
    }
}

/// The session's line for a server started now: the model it saved, or how to give it one; none where it learns
/// with the model saved before.
pub fn started_line(model: &Model) -> Option<String> {
    match model {
        Model::Kept => None,
        Model::Saved(what) => Some(format!(
            "AmpUp Memory started its local server ({URL}) and saved the model it learns with from this \
             environment ({what}). Tell the user once, briefly, then carry on with their request."
        )),
        Model::Missing => Some(format!(
            "AmpUp Memory started its local server ({URL}) with no model: it keeps and searches memory but learns \
             nothing until one is set. Tell the user once, briefly: with a key in the environment \
             (OPENROUTER_API_KEY, ANTHROPIC_API_KEY or OPENAI_API_KEY), `ampup-memory service save-model` saves \
             it for the server's next start, or `ampup-memory service install` keeps the server always on with it \
             (https://docs.ampup.ai/ampup-memory/configuration#models). Then carry on with their request."
        )),
    }
}

/// The session's line where the server isn't installed: how to install it, said once at the start.
pub fn install_hint() -> String {
    format!(
        "AmpUp Memory is off in this session: the memory is a local server (http://127.0.0.1:{PORT}) and the \
         server, `{}`, isn't installed on this machine (the plugin installs only the client). Tell the user once, \
         briefly: install it with `uv tool install ampup-memory-cli` or `curl -fsSL \
         https://raw.githubusercontent.com/A79-ai/ampup-memory/main/install.sh | sh` and the next session starts \
         it, or sign in to a team's server with /ampup-memory:login (or {} in a terminal). Then carry on with \
         their request.",
        crate::naming::BIN,
        crate::client::sign_in_hint()
    )
}
