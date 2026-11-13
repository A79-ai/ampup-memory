//! `ampup-memory service`: `serve --local` kept running in the background, started at login and again if it stops,
//! so nobody keeps a terminal open for it. On macOS a LaunchAgent (`~/Library/LaunchAgents/ai.ampup.memory.plist`)
//! that launchd runs in the login session; on Linux a systemd user unit
//! (`~/.config/systemd/user/ampup-memory.service`). Either runs this binary as `serve org <port> --local <dir>`
//! and writes its output to `<dir>/logs/serve.log`.
//!
//! Nothing secret goes in the unit. The model the server learns with, key included, is kept in `<dir>/llm.json`
//! (readable by its owner alone), which `serve --local` reads; other settings (`AMPUP_MEMORY_*` that aren't keys or
//! tokens) are carried into the unit from the environment `service install` runs in.
//!
//! Also [`background`] (`serve --local --background`): the same server started once, detached, for whatever
//! starts one when nothing answers; it returns once the server is ready.

use std::io::{IsTerminal, Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::extract::llm;
// The settings carried into the unit: the same a session's start gives the server it starts, so either serves the
// same way.
use memstore_client::local::carried_env;

/// The LaunchAgent's label.
pub const LABEL: &str = "ai.ampup.memory";
/// The systemd user unit.
pub const UNIT: &str = "ampup-memory.service";
/// The port the hooks and the plugin call when nobody signed in, and the variable naming the file a process writes
/// its output to ([`own_log_from_env`]): the client's, which starts this server at a session's start.
pub use memstore_client::local::{LOG_VAR, PORT};
/// A log bigger than this is rotated (`serve.log` to `serve.log.1`, and so on).
const LOG_KEEP: u64 = 20 << 20;
/// How many rotated logs are kept beside the current one.
const LOG_ROTATED: u32 = 3;
/// How often a running process checks its log's size.
const LOG_CHECK: Duration = Duration::from_secs(30);
/// How long a start waits for the server to be ready (a first start fetches the embedding model).
const READY_WITHIN: Duration = Duration::from_secs(120);

/// `serve --local`'s directory as given, absolute: `~/...` under HOME, the default the state directory (still
/// `~/.memstore` where moving it failed).
pub fn data_dir(dir: &str) -> Result<PathBuf> {
    Ok(match dir.strip_prefix("~/") {
        Some(crate::naming::STATE_DIR) => crate::naming::current_state_dir(),
        Some(rest) => PathBuf::from(std::env::var("HOME")?).join(rest),
        None => std::path::absolute(dir)?,
    })
}

/// Where the server started from here writes its output.
pub fn log_file(dir: &Path) -> PathBuf {
    dir.join("logs").join("serve.log")
}

/// What answers on 127.0.0.1 at a port.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Answer {
    /// Nothing: the port is free.
    Free,
    /// An AmpUp Memory server, ready or still warming up.
    Ours { ready: bool },
    /// Something else holds the port.
    Other,
}

/// Who holds 127.0.0.1:`port`: an AmpUp Memory server answers `/ready` with its writer's state.
pub async fn probe(port: u16) -> Answer {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let connected =
        tokio::time::timeout(Duration::from_secs(1), tokio::net::TcpStream::connect(addr)).await;
    if !matches!(connected, Ok(Ok(_))) {
        // Nothing accepts here; a port held some other way still refuses a bind.
        return match std::net::TcpListener::bind(addr) {
            Ok(_) => Answer::Free,
            Err(_) => Answer::Other,
        };
    }
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
    else {
        return Answer::Other;
    };
    let answer = client
        .get(format!("http://127.0.0.1:{port}/ready"))
        .send()
        .await;
    match answer {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) if v.get("ready").is_some() && v.get("lease").is_some() => Answer::Ours {
                ready: v["ready"] == true,
            },
            _ => Answer::Other,
        },
        Err(_) => Answer::Other,
    }
}

/// Port `port` held by a program that isn't AmpUp Memory, in words.
fn taken(port: u16) -> String {
    format!(
        "port {port} is in use by another program, not AmpUp Memory (`lsof -iTCP:{port} -sTCP:LISTEN` shows which); \
         free it, or use another port"
    )
}

/// Before `serve --local` binds: a port already served says by whom, instead of a bind error. The background
/// service holding it points at `service status`.
pub async fn refuse_if_taken(port: u16, dir: &Path) -> Result<()> {
    match probe(port).await {
        Answer::Free => Ok(()),
        Answer::Other => bail!(
            "{}: `ampup-memory serve --local {} org <port>`",
            taken(port),
            dir.display()
        ),
        Answer::Ours { .. } => match platform::read().ok().flatten() {
            Some(u) if u.port == port && platform::pid().is_some() => bail!(
                "the background service is already serving {} on http://127.0.0.1:{port} (`ampup-memory service \
                 status`); `ampup-memory service uninstall` stops it, to run it here instead",
                u.dir.display()
            ),
            _ => bail!(
                "an AmpUp Memory server is already serving on http://127.0.0.1:{port} (in another terminal, or \
                 started with --background): stop it, or serve on another port: `ampup-memory serve --local {} \
                 org <port>`",
                dir.display()
            ),
        },
    }
}

/// This binary, by its real path (the old name, `memstore`, is a link to it).
fn exe() -> Result<PathBuf> {
    let me = std::env::current_exe().context("couldn't find this binary")?;
    Ok(std::fs::canonicalize(&me).unwrap_or(me))
}

/// Where `f` is over [`LOG_KEEP`]: moves it to `f.1`, `f.1` to `f.2` and so on, the oldest dropped.
fn rotate(f: &Path) {
    if !std::fs::metadata(f).is_ok_and(|m| m.len() > LOG_KEEP) {
        return;
    }
    let old = |i: u32| PathBuf::from(format!("{}.{i}", f.display()));
    let _ = std::fs::remove_file(old(LOG_ROTATED));
    for i in (1..LOG_ROTATED).rev() {
        let _ = std::fs::rename(old(i), old(i + 1));
    }
    let _ = std::fs::rename(f, old(1));
}

/// First in `main`: where [`LOG_VAR`] names a file, this process's stdout and stderr go to it (appended,
/// readable by its owner alone), rotated past 20 MB when it starts and while it runs, with 3 older ones kept. The
/// process owns its log, so the cap holds whoever started it: launchd or systemd restarting it after a crash, or
/// `--background`. The variable is taken out of the environment, so nothing this process starts writes there too.
/// Where the file can't be opened, the outputs stay as they were.
///
/// # Safety
///
/// Called before any other thread starts: it changes the environment.
pub unsafe fn own_log_from_env() {
    let Some(path) = std::env::var_os(LOG_VAR).filter(|p| !p.is_empty()) else {
        return;
    };
    // SAFETY: the caller's: no other thread runs yet.
    unsafe { std::env::remove_var(LOG_VAR) };
    #[cfg(unix)]
    {
        let path = PathBuf::from(path);
        if let Err(e) = redirect(&path) {
            eprintln!(
                "ampup-memory: couldn't write the log to {}: {e}",
                path.display()
            );
            return;
        }
        let _ = std::thread::Builder::new()
            .name("log-rotate".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(LOG_CHECK);
                    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_KEEP) {
                        let _ = redirect(&path);
                    }
                }
            });
    }
}

/// Rotates `path` if it is due, and points stdout and stderr at it.
#[cfg(unix)]
fn redirect(path: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    rotate(path);
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    for fd in [1, 2] {
        // SAFETY: dup2 onto a standard descriptor of this process, from a file open for the call.
        if unsafe { libc::dup2(f.as_raw_fd(), fd) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The last lines of `dir`'s log, for an error.
fn log_tail(dir: &Path, lines: usize) -> String {
    let Ok(mut f) = std::fs::File::open(log_file(dir)) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(std::io::SeekFrom::Start(len.saturating_sub(8192)));
    let mut s = String::new();
    let _ = f.read_to_string(&mut s);
    let all: Vec<&str> = s.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Waits until the server on `port` is ready, `alive` checking meanwhile that it hasn't stopped: true once
/// ready, false where it still warms up at `within`.
async fn wait_ready(
    port: u16,
    within: Duration,
    mut alive: impl FnMut() -> Result<()>,
) -> Result<bool> {
    let start = Instant::now();
    loop {
        if let Answer::Ours { ready: true } = probe(port).await {
            return Ok(true);
        }
        alive()?;
        if start.elapsed() > within {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Whether someone is at the terminal to answer the first-run question.
fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// `serve --local --background`: starts the server detached (its own session, no terminal; output to
/// `dir/logs/serve.log`) and returns once it is ready. Nothing to do where an AmpUp Memory server already answers
/// on the port; a port another program holds is an error. At a terminal, a first run asks which model to use
/// first, as `serve --local` does.
pub async fn background(dir: &Path, scope: &str, port: u16, groups: Option<&str>) -> Result<()> {
    match probe(port).await {
        Answer::Ours { ready } => {
            if !ready {
                wait_ready(port, READY_WITHIN, || Ok(())).await?;
            }
            println!("AmpUp Memory is already serving on http://127.0.0.1:{port}");
            return Ok(());
        }
        Answer::Other => bail!(taken(port)),
        Answer::Free => {}
    }
    std::fs::create_dir_all(dir)?;
    llm::first_run(dir).await?;
    let mut cmd = Command::new(exe()?);
    cmd.arg("serve").arg(scope).arg(port.to_string());
    if let Some(g) = groups {
        cmd.arg(g);
    }
    cmd.arg("--local")
        .arg(dir)
        .current_dir(dir)
        // The server writes its own log, capped ([`own_log_from_env`]).
        .env(LOG_VAR, log_file(dir))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe, and nothing else runs between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().context("couldn't start the server")?;
    let pid = child.id();
    let ready = wait_ready(port, READY_WITHIN, || match child.try_wait()? {
        Some(s) => bail!("the server stopped ({s}):\n{}", log_tail(dir, 10)),
        None => Ok(()),
    })
    .await?;
    println!(
        "AmpUp Memory {} {} on http://127.0.0.1:{port} in the background (pid {pid}, log {}); `kill {pid}` stops it",
        if ready {
            "is serving"
        } else {
            "started (still warming up) serving"
        },
        dir.display(),
        log_file(dir).display()
    );
    Ok(())
}

/// What `service install` wrote: its file, the binary, directory and port it runs, and the names of the
/// settings carried into it.
#[derive(Debug, Clone)]
pub struct Unit {
    pub file: PathBuf,
    pub exe: PathBuf,
    pub dir: PathBuf,
    pub port: u16,
    pub carried: Vec<String>,
}

/// The command the unit runs.
fn command(exe: &Path, dir: &Path, port: u16) -> Vec<String> {
    vec![
        exe.display().to_string(),
        "serve".into(),
        "org".into(),
        port.to_string(),
        "--local".into(),
        dir.display().to_string(),
    ]
}

/// The binary, directory and port in a command [`command`] made, and the settings named in the unit (but for the
/// log's, the unit's own).
fn parse_command(file: PathBuf, args: &[String], env: Vec<String>) -> Option<Unit> {
    let local = args.iter().position(|a| a == "--local")?;
    let mut carried: Vec<String> = env.into_iter().filter(|k| k != LOG_VAR).collect();
    carried.sort();
    Some(Unit {
        file,
        exe: PathBuf::from(args.first()?),
        port: args.get(3)?.parse().ok()?,
        dir: PathBuf::from(args.get(local + 1)?),
        carried,
    })
}

/// `ampup-memory service install [dir] [--port p]`.
pub async fn install(dir: &str, port: u16) -> Result<()> {
    let dir = data_dir(dir)?;
    platform::check()?;
    let exe = exe()?;
    let before = platform::read()?;
    let ours_running = before.as_ref().is_some_and(|u| u.port == port) && platform::pid().is_some();
    match probe(port).await {
        Answer::Other => bail!(
            "{}: `ampup-memory service install --port <port>`",
            taken(port)
        ),
        Answer::Ours { .. } if !ours_running => bail!(
            "an AmpUp Memory server is already serving on http://127.0.0.1:{port}, not as the service (in a \
             terminal, or started with --background): stop it first, then install"
        ),
        _ => {}
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("{}", dir.display()))?;
    // The model: this environment's, else what was chosen before, else asked now at a terminal.
    match llm::adopt_env(&dir)? {
        Some(line) => println!("{line}"),
        None => match llm::saved(&dir)? {
            Some(l) => println!(
                "model: {}, as saved in {}",
                llm::describe(&l),
                dir.join("llm.json").display()
            ),
            None if interactive() => llm::first_run(&dir).await?,
            None => println!(
                "model: none, so memory is kept and searched but nothing is extracted or learned; to add one, run \
                 `ampup-memory service install` again in a terminal, or with OPENROUTER_API_KEY (or \
                 ANTHROPIC_API_KEY, OPENAI_API_KEY, AMPUP_MEMORY_LLM=ollama) set"
            ),
        },
    }
    let (env, secret) = carried_env();
    let names: Vec<String> = env.iter().map(|(k, _)| k.clone()).collect();
    let mut unit_env = vec![(LOG_VAR.to_string(), log_file(&dir).display().to_string())];
    unit_env.extend(env);
    let file = platform::write(&exe, &dir, port, &unit_env)?;
    platform::start(&file, ours_running)?;
    // Which of this shell's settings the service now has, by name, and that the next install replaces them.
    if names.is_empty() {
        println!(
            "settings: no AMPUP_MEMORY_* settings carried from this shell (`service install` run again carries that \
             shell's)"
        );
    } else {
        println!(
            "settings carried from this shell into the service: {} (`service install` run again replaces them with \
             that shell's AMPUP_MEMORY_* settings)",
            names.join(", ")
        );
    }
    let dropped: Vec<&str> = before
        .iter()
        .flat_map(|u| &u.carried)
        .filter(|k| !names.contains(k))
        .map(String::as_str)
        .collect();
    if !dropped.is_empty() {
        println!(
            "no longer carried (not set in this shell): {}",
            dropped.join(", ")
        );
    }
    if !secret.is_empty() {
        println!(
            "not carried (secrets stay out of the service's file): {}",
            secret.join(", ")
        );
    }
    println!("installed {}", file.display());
    let ready = wait_ready(port, READY_WITHIN, || Ok(())).await?;
    println!(
        "AmpUp Memory {} at http://127.0.0.1:{port} in the background, from {}; it starts again at login and if \
         it stops",
        if ready {
            "is running"
        } else {
            "started (still warming up; `ampup-memory service logs`)"
        },
        dir.display()
    );
    println!("  log: {}", log_file(&dir).display());
    if port != PORT {
        println!(
            "  the hooks call port {PORT} unless told: set AMPUP_MEMORY_URL=http://127.0.0.1:{port} for them"
        );
    }
    platform::after_install();
    println!(
        "  `ampup-memory service status` says how it is; `ampup-memory service uninstall` stops it (the data stays)"
    );
    Ok(())
}

/// `ampup-memory service save-model [dir]`: the model this environment names, kept in `dir/llm.json` as `install`
/// keeps it ([`llm::adopt_env`]), for whatever serves `dir` later. Prints what it did on stdout (never the key);
/// nothing there where the environment names no model, which a session's start reads as no model saved.
pub fn save_model(dir: &str) -> Result<()> {
    let dir = data_dir(dir)?;
    match llm::adopt_env(&dir)? {
        Some(line) => println!("{line}"),
        None => eprintln!(
            "model: none in this environment, nothing saved; {}",
            llm::HOW
        ),
    }
    Ok(())
}

/// `ampup-memory service uninstall`: stops it and removes its file; the data stays.
pub async fn uninstall() -> Result<()> {
    let Some(u) = platform::read()? else {
        println!("the service isn't installed: nothing to remove");
        return Ok(());
    };
    platform::remove(&u.file)?;
    println!(
        "stopped and removed {}; the data stays in {} (`ampup-memory serve --local {}` serves it in a terminal)",
        u.file.display(),
        u.dir.display(),
        u.dir.display()
    );
    Ok(())
}

/// `ampup-memory service restart`.
pub async fn restart() -> Result<()> {
    let Some(u) = platform::read()? else {
        bail!("the service isn't installed: `ampup-memory service install` sets it up");
    };
    platform::restart()?;
    let ready = wait_ready(u.port, READY_WITHIN, || Ok(())).await?;
    println!(
        "restarted: {} on http://127.0.0.1:{}",
        if ready { "serving" } else { "still warming up" },
        u.port
    );
    Ok(())
}

/// `ampup-memory service status`: installed, running (pid), port, data, log and whether `/ready` answers. Fails
/// where it isn't installed or doesn't answer.
pub async fn status() -> Result<()> {
    let Some(u) = platform::read()? else {
        let here = match probe(PORT).await {
            Answer::Ours { .. } => format!(
                " (an AmpUp Memory server answers on http://127.0.0.1:{PORT}, run some other way)"
            ),
            _ => String::new(),
        };
        bail!("the service isn't installed{here}: `ampup-memory service install` sets it up");
    };
    let pid = platform::pid();
    println!("service: installed ({})", u.file.display());
    println!(
        "running: {}",
        match pid {
            Some(p) => format!("yes, pid {p}"),
            None => format!("no ({})", platform::state()),
        }
    );
    let answer = probe(u.port).await;
    println!(
        "serving: http://127.0.0.1:{} {}",
        u.port,
        match answer {
            Answer::Ours { ready: true } => "(ready)",
            Answer::Ours { ready: false } => "(warming up)",
            Answer::Other => "(another program holds the port)",
            Answer::Free => "(not answering)",
        }
    );
    println!("data: {}", u.dir.display());
    println!(
        "log: {} (rotated at 20 MB, {LOG_ROTATED} older kept)",
        log_file(&u.dir).display()
    );
    println!(
        "settings carried in: {}",
        if u.carried.is_empty() {
            "none".to_string()
        } else {
            u.carried.join(", ")
        }
    );
    println!("binary: {}", u.exe.display());
    if !u.exe.exists() {
        println!("  it is gone: `ampup-memory service install` again with the binary you use now");
    }
    match llm::saved(&u.dir) {
        Ok(Some(l)) => println!("model: {}", llm::describe(&l)),
        Ok(None) => println!("model: none (memory is kept and searched, nothing is extracted)"),
        Err(e) => println!("model: {e:#}"),
    }
    match answer {
        Answer::Ours { .. } => Ok(()),
        Answer::Other => bail!(taken(u.port)),
        Answer::Free => bail!(
            "nothing answers on port {}: `ampup-memory service logs` says why",
            u.port
        ),
    }
}

/// `ampup-memory service logs [-f] [-n lines]`.
pub fn logs(follow: bool, lines: usize) -> Result<()> {
    let Some(u) = platform::read()? else {
        bail!("the service isn't installed: `ampup-memory service install` sets it up");
    };
    let f = log_file(&u.dir);
    if !f.exists() {
        bail!("no log yet at {}", f.display());
    }
    let mut tail = Command::new("tail");
    tail.arg("-n").arg(lines.to_string());
    if follow {
        tail.arg("-F");
    }
    let s = tail.arg(&f).status().context("couldn't run tail")?;
    if !s.success() && !follow {
        bail!("tail {} failed ({s})", f.display());
    }
    Ok(())
}

/// A command's output, failing with its stderr.
fn run(cmd: &str, args: &[&str]) -> Result<std::process::Output> {
    let o = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("couldn't run {cmd}"))?;
    if !o.status.success() {
        bail!(
            "{cmd} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(o)
}

/// Writes `body` to `file` by a rename, so a half-written one is never read.
fn write_file(file: &Path, body: &str) -> Result<()> {
    std::fs::create_dir_all(file.parent().unwrap())?;
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, body).with_context(|| format!("{}", tmp.display()))?;
    std::fs::rename(&tmp, file).with_context(|| format!("{}", file.display()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
mod platform {
    //! A LaunchAgent in the login session's domain (`gui/<uid>`), through `launchctl bootstrap`, `bootout` and
    //! `kickstart`.

    use super::*;

    fn file() -> Result<PathBuf> {
        Ok(PathBuf::from(std::env::var("HOME")?)
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    fn domain() -> String {
        // SAFETY: getuid has no preconditions and can't fail.
        format!("gui/{}", unsafe { libc::getuid() })
    }

    fn target() -> String {
        format!("{}/{LABEL}", domain())
    }

    pub fn check() -> Result<()> {
        if run("launchctl", &["print", &domain()]).is_err() {
            bail!(
                "there is no login session ({}) to run the service in (signed in over ssh?): log in on the Mac, or \
                 run `ampup-memory serve --local` in a terminal instead",
                domain()
            );
        }
        Ok(())
    }

    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    fn unesc(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    pub fn write(exe: &Path, dir: &Path, port: u16, env: &[(String, String)]) -> Result<PathBuf> {
        let f = file()?;
        let args: String = command(exe, dir, port)
            .iter()
            .map(|a| format!("\n    <string>{}</string>", esc(a)))
            .collect();
        let vars: String = env
            .iter()
            .map(|(k, v)| {
                format!(
                    "\n    <key>{}</key>\n    <string>{}</string>",
                    esc(k),
                    esc(v)
                )
            })
            .collect();
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- AmpUp Memory's local server, written by `ampup-memory service install`; `ampup-memory service uninstall` removes it. -->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>{args}
  </array>
  <key>WorkingDirectory</key>
  <string>{wd}</string>
  <key>EnvironmentVariables</key>
  <dict>{vars}
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>10</integer>
</dict>
</plist>
"#,
            wd = esc(&dir.display().to_string()),
        );
        write_file(&f, &body)?;
        Ok(f)
    }

    pub fn read() -> Result<Option<Unit>> {
        let f = file()?;
        let Ok(s) = std::fs::read_to_string(&f) else {
            return Ok(None);
        };
        let args: Vec<String> = s
            .split_once("<key>ProgramArguments</key>")
            .and_then(|(_, rest)| rest.split_once("</array>"))
            .map(|(array, _)| {
                array
                    .split("<string>")
                    .skip(1)
                    .filter_map(|p| p.split_once("</string>").map(|(a, _)| unesc(a)))
                    .collect()
            })
            .unwrap_or_default();
        // The settings' names: the keys of its EnvironmentVariables.
        let env: Vec<String> = s
            .split_once("<key>EnvironmentVariables</key>")
            .and_then(|(_, rest)| rest.split_once("</dict>"))
            .map(|(dict, _)| {
                dict.split("<key>")
                    .skip(1)
                    .filter_map(|p| p.split_once("</key>").map(|(k, _)| unesc(k)))
                    .collect()
            })
            .unwrap_or_default();
        match parse_command(f.clone(), &args, env) {
            Some(u) => Ok(Some(u)),
            None => bail!(
                "{} isn't one `ampup-memory service install` wrote: remove it, then install",
                f.display()
            ),
        }
    }

    fn print() -> Option<String> {
        run("launchctl", &["print", &target()])
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    pub fn pid() -> Option<u32> {
        print()?
            .lines()
            .find_map(|l| l.trim().strip_prefix("pid = "))
            .and_then(|p| p.trim().parse().ok())
    }

    pub fn state() -> String {
        match print() {
            None => "not loaded in launchd".into(),
            Some(p) => {
                let field = |name: &str| {
                    p.lines()
                        .find_map(|l| l.trim().strip_prefix(name).map(|v| v.trim().to_string()))
                };
                match (field("state = "), field("last exit code = ")) {
                    (Some(s), Some(c)) => format!("{s}, last exit {c}"),
                    (Some(s), None) => s,
                    _ => "loaded".into(),
                }
            }
        }
    }

    /// Takes it out of launchd, stopping it, and waits until it's gone.
    fn bootout() {
        if print().is_none() {
            return;
        }
        let _ = run("launchctl", &["bootout", &target()]);
        let start = Instant::now();
        while print().is_some() && start.elapsed() < Duration::from_secs(15) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Loads the agent as written now (again where it was loaded, so a new binary or port takes), which starts it.
    pub fn start(file: &Path, _running: bool) -> Result<()> {
        bootout();
        // A service someone disabled (`launchctl disable`) would load but never start.
        let _ = run("launchctl", &["enable", &target()]);
        let f = file.display().to_string();
        let mut last = None;
        for _ in 0..5 {
            match run("launchctl", &["bootstrap", &domain(), &f]) {
                Ok(_) => return Ok(()),
                Err(e) => last = Some(e),
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        Err(last.unwrap())
    }

    pub fn restart() -> Result<()> {
        if print().is_none() {
            return start(&file()?, false);
        }
        run("launchctl", &["kickstart", "-k", &target()]).map(|_| ())
    }

    pub fn remove(file: &Path) -> Result<()> {
        bootout();
        std::fs::remove_file(file).with_context(|| format!("{}", file.display()))
    }

    pub fn after_install() {}
}

#[cfg(target_os = "linux")]
mod platform {
    //! A systemd user unit, through `systemctl --user`.

    use super::*;

    fn file() -> Result<PathBuf> {
        let config = match std::env::var("XDG_CONFIG_HOME") {
            Ok(c) if !c.is_empty() => PathBuf::from(c),
            _ => PathBuf::from(std::env::var("HOME")?).join(".config"),
        };
        Ok(config.join("systemd/user").join(UNIT))
    }

    fn systemctl(args: &[&str]) -> Result<std::process::Output> {
        let mut all = vec!["--user"];
        all.extend_from_slice(args);
        run("systemctl", &all)
    }

    pub fn check() -> Result<()> {
        const INSTEAD: &str = "run `ampup-memory serve --local` in a terminal instead (or `ampup-memory serve \
                               --local --background` to start it detached until the machine restarts)";
        if !Path::new("/run/systemd/system").exists() {
            bail!(
                "this machine doesn't run systemd (a container, or WSL without systemd), so there is no user \
                 service to install: {INSTEAD}"
            );
        }
        if let Err(e) = systemctl(&["show-environment"]) {
            bail!(
                "there is no systemd user session here ({e:#}; signed in with su or sudo, or no login session?): \
                 {INSTEAD}"
            );
        }
        Ok(())
    }

    /// One word of a unit's command line or setting, quoted for systemd.
    fn quote(s: &str) -> String {
        format!(
            "\"{}\"",
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%")
                .replace('$', "$$")
        )
    }

    /// The words of a command line [`quote`] wrote.
    fn words(line: &str) -> Vec<String> {
        let mut out = vec![];
        let mut cur = String::new();
        let (mut quoted, mut any) = (false, false);
        let mut it = line.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '\\' if quoted => {
                    if let Some(n) = it.next() {
                        cur.push(n);
                    }
                }
                '"' => {
                    quoted = !quoted;
                    any = true;
                }
                '%' | '$' if it.peek() == Some(&c) => {
                    it.next();
                    cur.push(c);
                }
                c if c.is_whitespace() && !quoted => {
                    if any || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        any = false;
                    }
                }
                c => cur.push(c),
            }
        }
        if any || !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    pub fn write(exe: &Path, dir: &Path, port: u16, env: &[(String, String)]) -> Result<PathBuf> {
        let f = file()?;
        let exec: Vec<String> = command(exe, dir, port).iter().map(|a| quote(a)).collect();
        let vars: String = env
            .iter()
            .map(|(k, v)| format!("Environment={}\n", quote(&format!("{k}={v}"))))
            .collect();
        // No StandardOutput: the server writes its own log, capped (`AMPUP_MEMORY_LOG_FILE`); anything before it
        // takes over goes to the journal.
        let body = format!(
            "# AmpUp Memory's local server, written by `ampup-memory service install`; `ampup-memory service \
             uninstall` removes it.\n\
             [Unit]\n\
             Description=AmpUp Memory (local server)\n\
             \n\
             [Service]\n\
             ExecStart={}\n\
             WorkingDirectory={}\n\
             {vars}\
             Restart=on-failure\n\
             RestartSec=5\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            exec.join(" "),
            dir.display().to_string().replace('%', "%%"),
        );
        write_file(&f, &body)?;
        Ok(f)
    }

    pub fn read() -> Result<Option<Unit>> {
        let f = file()?;
        let Ok(s) = std::fs::read_to_string(&f) else {
            return Ok(None);
        };
        let args = s
            .lines()
            .find_map(|l| l.strip_prefix("ExecStart="))
            .map(words)
            .unwrap_or_default();
        let env: Vec<String> = s
            .lines()
            .filter_map(|l| l.strip_prefix("Environment="))
            .flat_map(words)
            .filter_map(|w| w.split_once('=').map(|(k, _)| k.to_string()))
            .collect();
        match parse_command(f.clone(), &args, env) {
            Some(u) => Ok(Some(u)),
            None => bail!(
                "{} isn't one `ampup-memory service install` wrote: remove it, then install",
                f.display()
            ),
        }
    }

    fn show(property: &str) -> Option<String> {
        systemctl(&["show", UNIT, "-p", property, "--value"])
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    pub fn pid() -> Option<u32> {
        show("MainPID")?.parse().ok().filter(|p| *p != 0)
    }

    pub fn state() -> String {
        match (show("ActiveState"), show("SubState")) {
            (Some(a), Some(s)) => format!("{a} ({s})"),
            _ => "unknown to systemd".into(),
        }
    }

    pub fn start(_file: &Path, running: bool) -> Result<()> {
        systemctl(&["daemon-reload"])?;
        let _ = systemctl(&["reset-failed", UNIT]);
        systemctl(&["enable", "--now", UNIT])?;
        // Enabling doesn't restart one running already: the unit may name a new binary or port.
        if running || show("ActiveState").as_deref() == Some("active") {
            systemctl(&["restart", UNIT])?;
        }
        Ok(())
    }

    pub fn restart() -> Result<()> {
        systemctl(&["restart", UNIT]).map(|_| ())
    }

    pub fn remove(file: &Path) -> Result<()> {
        let _ = systemctl(&["disable", "--now", UNIT]);
        std::fs::remove_file(file).with_context(|| format!("{}", file.display()))?;
        let _ = systemctl(&["daemon-reload"]);
        let _ = systemctl(&["reset-failed", UNIT]);
        Ok(())
    }

    /// Says how to keep it running without a login session, where it would stop at logout.
    pub fn after_install() {
        let user = std::env::var("USER").unwrap_or_default();
        let linger = run("loginctl", &["show-user", &user, "-p", "Linger", "--value"])
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        if linger.as_deref() != Some("yes") {
            println!(
                "  it runs while you are logged in; `loginctl enable-linger {user}` keeps it running without a \
                 login session (and starts it at boot)"
            );
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::*;

    const NONE: &str = "the background service is for macOS and Linux: run `ampup-memory serve --local` in a \
                        terminal instead";

    pub fn check() -> Result<()> {
        bail!(NONE)
    }
    pub fn write(_: &Path, _: &Path, _: u16, _: &[(String, String)]) -> Result<PathBuf> {
        bail!(NONE)
    }
    pub fn read() -> Result<Option<Unit>> {
        Ok(None)
    }
    pub fn pid() -> Option<u32> {
        None
    }
    pub fn state() -> String {
        "unsupported".into()
    }
    pub fn start(_: &Path, _: bool) -> Result<()> {
        bail!(NONE)
    }
    pub fn restart() -> Result<()> {
        bail!(NONE)
    }
    pub fn remove(_: &Path) -> Result<()> {
        bail!(NONE)
    }
    pub fn after_install() {}
}
