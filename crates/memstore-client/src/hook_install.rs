//! `ampup-memory hooks install --harness <name>`: registers AmpUp Memory's hooks and MCP server (`ampup-memory`) in a
//! coding agent's user-level config, so its sessions are remembered and what the memory knows is recalled into new
//! ones. `ampup-memory hooks uninstall --harness <name>` takes them out again. An install from before the rename
//! (an MCP server named `memstore`, hooks running `memstore hook …`, `~/.copilot/hooks/memstore.json`) is replaced
//! by the next install, and removed by an uninstall.
//!
//! | harness | hooks file | MCP file | recall |
//! |---|---|---|---|
//! | codex | `~/.codex/hooks.json` | `~/.codex/config.toml` | session start; advice and facts at the first prompt |
//! | gemini | `~/.gemini/settings.json` | the same | session start; advice and facts at the first prompt |
//! | cursor | `~/.cursor/hooks.json` | `~/.cursor/mcp.json` | session start (Cursor's prompt hook can't add context) |
//! | copilot | `~/.copilot/hooks/ampup-memory.json` | `~/.copilot/mcp-config.json` | session start (the same) |
//!
//! Every harness records its sessions as runs when they compact or end, as the Claude Code plugin does; its
//! session-end hook hands the work to a copy of itself, so a harness's short session-end budget never cuts it off.
//!
//! Claude Code installs through its plugin (`/plugin install ampup-memory@ampup-memory`) instead.
//!
//! Everything else in those files is kept: the installer adds its own entries (every hook command it writes runs
//! `ampup-memory hook … --harness <name>`, which is how it finds them again, `memstore`'s commands too) and backs a file up before its first
//! change (`<file>.ampup-memory-backup`). Installing twice changes nothing. The hooks read `AMPUP_MEMORY_URL`,
//! `AMPUP_MEMORY_TOKEN`, `AMPUP_MEMORY_ORG`, `AMPUP_MEMORY_TEAM` and `AMPUP_MEMORY_SCOPE` when they run; `--url` writes the URL
//! into the commands and the MCP entry instead, with the org, team and scope set when installing. A token is never written into a file the agent's config already references by env var; where a
//! harness can't read one from the environment (Gemini, Copilot), the MCP entry carries it, and the installer says so.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::hook::Harness;

/// What an install or uninstall did to one file.
#[derive(Debug)]
pub struct Change {
    pub path: PathBuf,
    pub what: String,
}

/// Where the agent's config lives: `$HOME`, so a test (or a sandbox) can point it elsewhere.
fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// This binary, by its absolute path when it has one, so a harness that runs hooks or MCP servers without the
/// user's PATH still finds it.
fn exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| crate::naming::BIN.into())
}

/// A word for a shell command line, quoted where it needs to be.
fn quoted(s: &str) -> String {
    shlex::try_quote(s).map_or_else(|_| crate::naming::BIN.into(), |q| q.into_owned())
}

/// The hook command for `event`: [`exe`], every part shell-quoted. With `url`, the server (and `org`, the org)
/// written in front of it, so the hooks reach that memory whatever context the machine's CLI has.
fn command(harness: Harness, event: &str, url: Option<&str>, org: Option<&str>) -> String {
    let mut c = format!(
        "{} hook {event} --harness {}",
        quoted(&exe()),
        harness.name()
    );
    if let Some(u) = url {
        if let Some(o) = org {
            c = format!("AMPUP_MEMORY_ORG={} {c}", quoted(o));
        }
        c = format!("AMPUP_MEMORY_URL={} {c}", quoted(u));
        // Hermes runs a hook's words without a shell: `env` sets them.
        if harness == Harness::Hermes {
            c = format!("env {c}");
        }
    }
    c
}

/// Whether a hook command is one this installer wrote: `[AMPUP_MEMORY_URL=… ]…/memstore hook <event> --harness
/// <name>`, exactly. A user's own hook never matches, whatever it mentions.
fn ours_command(cmd: &str) -> bool {
    let Some(mut words) = shlex::split(cmd) else {
        return false;
    };
    // The server and org written in front (an older install's by their old names), by `env` for Hermes.
    if words.first().is_some_and(|w| w == "env") {
        words.remove(0);
    }
    while words.first().is_some_and(|w| {
        w.split_once('=').is_some_and(|(k, _)| {
            matches!(
                crate::naming::env_name(k).as_ref(),
                "AMPUP_MEMORY_URL" | "AMPUP_MEMORY_ORG"
            )
        })
    }) {
        words.remove(0);
    }
    let events = ["session-start", "prompt", "stop", "session-end", "end"];
    words.len() == 5
        && Path::new(&words[0]).file_name().is_some_and(|n| {
            n == crate::naming::BIN
                || n == crate::naming::OLD_BIN
                || n == crate::naming::CLIENT_BIN
                || n == crate::naming::OLD_CLIENT_BIN
        })
        && words[1] == "hook"
        && events.contains(&words[2].as_str())
        && words[3] == "--harness"
        && Harness::parse(&words[4]).is_ok()
}

/// A hook entry's command, in any harness's shape.
fn command_of(entry: &Value) -> Option<&str> {
    entry["command"].as_str().or(entry["bash"].as_str())
}

/// Reads a JSON config: `{}` when there is none. One that isn't a JSON object is refused, never replaced.
fn read_json(path: &Path) -> Result<Value> {
    let v = match std::fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => json!({}),
        Ok(t) => serde_json::from_str(&t).with_context(|| {
            format!(
                "{} is not JSON: fix or move it, then install again",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    if !v.is_object() {
        bail!(
            "{} is not a JSON object: fix or move it, then install again",
            path.display()
        );
    }
    Ok(v)
}

/// `doc[key]` as an object, made when absent; refused (never replaced) when it is something else.
fn object_at<'a>(
    doc: &'a mut Value,
    key: &str,
    path: &Path,
) -> Result<&'a mut serde_json::Map<String, Value>> {
    if doc.get(key).is_none() {
        doc[key] = json!({});
    }
    doc[key].as_object_mut().with_context(|| {
        format!(
            "{key} in {} is not an object: fix it, then install again",
            path.display()
        )
    })
}

/// Writes `content` to `path`, backing up what was there before its first change, unless `dry`. A file that
/// doesn't exist is left so when `content` adds nothing (uninstalling where nothing was installed). A `secret` file
/// (one holding a token) is owner-only, whatever mode it had.
fn write(path: &Path, content: &str, dry: bool, secret: bool) -> Result<bool> {
    // A config kept elsewhere (dotfiles) and linked in: written where it lives, the link kept.
    let resolved = std::fs::canonicalize(path).ok();
    let path = resolved.as_deref().unwrap_or(path);
    let before = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    if before.as_deref() == Some(content) {
        return Ok(false);
    }
    if before.is_none() && matches!(content.trim(), "" | "{}" | "{\n  \"version\": 1\n}") {
        return Ok(false);
    }
    if dry {
        return Ok(true);
    }
    if let Some(dir) = path.parent() {
        create_dir(dir)?;
    }
    let backup = PathBuf::from(format!("{}.ampup-memory-backup", path.display()));
    // An older install's backup (`.memstore-backup`) is the file as it was first: none is made over it.
    let old_backup = PathBuf::from(format!("{}.memstore-backup", path.display()));
    if let Some(b) = &before
        && !backup.exists()
        && !old_backup.exists()
    {
        put(&backup, b, 0o600).with_context(|| format!("back up {}", path.display()))?;
    }
    // The user's own mode on a file they had; owner-only on a new one, and on any that holds a token.
    let mode = match std::fs::metadata(path) {
        _ if secret => 0o600,
        Ok(m) => mode_of(&m),
        Err(_) => 0o600,
    };
    put(path, content, mode).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// Writes `content` to a new file beside `path` with `mode`, then renames it over `path`: a reader never sees
/// half a config, and a token is never readable by others, not even for a moment.
fn put(path: &Path, content: &str, mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = PathBuf::from(format!("{}.ampup-memory-tmp", path.display()));
    let _ = std::fs::remove_file(&tmp);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, mode);
    #[cfg(not(unix))]
    let _ = mode;
    let mut f = o.open(&tmp)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(unix)]
fn mode_of(m: &std::fs::Metadata) -> u32 {
    std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777
}

#[cfg(not(unix))]
fn mode_of(_: &std::fs::Metadata) -> u32 {
    0o600
}

/// A config directory the agent hasn't made yet: owner-only.
fn create_dir(dir: &Path) -> Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
        .with_context(|| format!("create {}", dir.display()))
}

/// The hooks a harness gets, as (its event name, our hook event, an entry in its shape).
fn hooks_for(harness: Harness, url: Option<&str>, org: Option<&str>) -> Vec<(&'static str, Value)> {
    let c = |event: &str| command(harness, event, url, org);
    // Claude-shaped: event → [{matcher?, hooks: [{type, command, timeout}]}]; timeouts in seconds.
    let claude = |events: [(&'static str, &str, u64); 5]| {
        events
            .into_iter()
            .map(|(e, ours, timeout)| {
                (
                    e,
                    json!({"hooks": [{"type": "command", "command": c(ours), "timeout": timeout}]}),
                )
            })
            .collect()
    };
    match harness {
        // Droid and Qwen Code: Claude Code's events.
        Harness::Droid | Harness::Qwen => claude([
            ("SessionStart", "session-start", 30),
            ("UserPromptSubmit", "prompt", 30),
            ("Stop", "stop", 30),
            ("PreCompact", "session-end", 30),
            ("SessionEnd", "end", 30),
        ]),
        // Devin: no hook before a compaction, one after it (what came before is in the transcript either way).
        Harness::Devin => claude([
            ("SessionStart", "session-start", 30),
            ("UserPromptSubmit", "prompt", 30),
            ("Stop", "stop", 30),
            ("PostCompaction", "session-end", 30),
            ("SessionEnd", "end", 30),
        ]),
        // Kiro: a list of {name, trigger, action: {type: command, command}, timeout (seconds)}, in a file of its own.
        Harness::Kiro => [
            ("SessionStart", "session-start"),
            ("UserPromptSubmit", "prompt"),
            ("Stop", "stop"),
            ("PreCompact", "session-end"),
            ("SessionEnd", "end"),
        ]
        .into_iter()
        .map(|(trigger, ours)| {
            (
                trigger,
                json!({"name": format!("AmpUp Memory: {ours}"), "trigger": trigger,
                       "action": {"type": "command", "command": c(ours)}, "timeout": 30}),
            )
        })
        .collect(),
        // Hooks of their own shape (Hermes, Cline) or none (a plugin's).
        Harness::Hermes
        | Harness::Cline
        | Harness::Grok
        | Harness::OpenCode
        | Harness::Amp
        | Harness::Kilo => vec![],
        Harness::Codex => vec![
            (
                "SessionStart",
                json!({"hooks": [{"type": "command", "command": c("session-start"), "timeout": 30}]}),
            ),
            (
                "UserPromptSubmit",
                json!({"hooks": [{"type": "command", "command": c("prompt"), "timeout": 30}]}),
            ),
            (
                "Stop",
                json!({"hooks": [{"type": "command", "command": c("stop"), "timeout": 30}]}),
            ),
            (
                "PreCompact",
                json!({"hooks": [{"type": "command", "command": c("session-end"), "timeout": 30}]}),
            ),
            (
                "SessionEnd",
                json!({"hooks": [{"type": "command", "command": c("end"), "timeout": 3}]}),
            ),
        ],
        // The same shape; timeouts in milliseconds.
        Harness::Gemini => vec![
            (
                "SessionStart",
                json!({"hooks": [{"type": "command", "command": c("session-start"), "timeout": 30000}]}),
            ),
            (
                "BeforeAgent",
                json!({"hooks": [{"type": "command", "command": c("prompt"), "timeout": 30000}]}),
            ),
            (
                "AfterAgent",
                json!({"hooks": [{"type": "command", "command": c("stop"), "timeout": 30000}]}),
            ),
            (
                "PreCompress",
                json!({"hooks": [{"type": "command", "command": c("session-end"), "timeout": 30000}]}),
            ),
            (
                "SessionEnd",
                json!({"hooks": [{"type": "command", "command": c("end"), "timeout": 30000}]}),
            ),
        ],
        // event → [{command, timeout}]; no prompt hook (it can't add context).
        Harness::Cursor => vec![
            (
                "sessionStart",
                json!({"command": c("session-start"), "timeout": 30}),
            ),
            ("stop", json!({"command": c("stop"), "timeout": 30})),
            (
                "preCompact",
                json!({"command": c("session-end"), "timeout": 30}),
            ),
            ("sessionEnd", json!({"command": c("end"), "timeout": 30})),
        ],
        // event → [{type, bash, timeoutSec}]; no prompt hook (the same).
        Harness::Copilot => vec![
            (
                "sessionStart",
                json!({"type": "command", "bash": c("session-start"), "timeoutSec": 30}),
            ),
            (
                "agentStop",
                json!({"type": "command", "bash": c("stop"), "timeoutSec": 30}),
            ),
            (
                "preCompact",
                json!({"type": "command", "bash": c("session-end"), "timeoutSec": 30}),
            ),
            (
                "sessionEnd",
                json!({"type": "command", "bash": c("end"), "timeoutSec": 30}),
            ),
        ],
        Harness::ClaudeCode => vec![],
    }
}

/// `doc["hooks"]` with our entries taken out, and (installing) put back in. Only our own commands are
/// removed: a user's hook that shares a group with one of ours stays.
fn set_hooks(doc: &mut Value, add: &[(&str, Value)], path: &Path) -> Result<()> {
    let hooks = object_at(doc, "hooks", path)?;
    set_events(hooks, add, path)?;
    prune(doc, "hooks");
    Ok(())
}

/// [`set_hooks`] on a map of events itself (Droid's `hooks.json` is one).
fn set_events(
    hooks: &mut serde_json::Map<String, Value>,
    add: &[(&str, Value)],
    path: &Path,
) -> Result<()> {
    for list in hooks.values_mut() {
        let Some(a) = list.as_array_mut() else {
            continue;
        };
        a.retain_mut(|e| {
            if command_of(e).is_some_and(ours_command) {
                return false;
            }
            // A group (Codex, Gemini): its own commands, ours taken out; gone only if ours were all it had.
            if let Some(inner) = e.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = inner.len();
                inner.retain(|h| !command_of(h).is_some_and(ours_command));
                return !(inner.is_empty() && before > 0);
            }
            true
        });
    }
    hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    for (event, entry) in add {
        let list = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        match list.as_array_mut() {
            Some(a) => a.push(entry.clone()),
            None => bail!("hooks.{event} in {} is not a list", path.display()),
        }
    }
    Ok(())
}

/// Drops `doc[key]` when it is an empty object: uninstalling leaves a file as the user had it.
fn prune(doc: &mut Value, key: &str) {
    if doc[key].as_object().is_some_and(|o| o.is_empty())
        && let Some(o) = doc.as_object_mut()
    {
        o.remove(key);
    }
}

/// The server's MCP endpoint, naming the org and team (`AMPUP_MEMORY_ORG`, `AMPUP_MEMORY_TEAM`) and scope
/// (`AMPUP_MEMORY_SCOPE`) when set, so the org's own servers answer it. The server is `url`, else
/// `AMPUP_MEMORY_URL`, else the server of the context `join` made (with its org and team), else the local one; the
/// org is `org` where given.
fn mcp_url(url: Option<&str>, org: Option<&str>) -> String {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let ctx = crate::client::Config::load().ok().map(|c| c.context());
    let ctx_get = |k: &str| ctx.as_ref().and_then(|c| c.get(k)).map(str::to_string);
    let ctx_server = ctx_get("server").map(|s| s.trim_end_matches('/').to_string());
    let base = url
        .map(String::from)
        .or_else(|| var("AMPUP_MEMORY_URL"))
        .or_else(|| ctx_server.clone())
        .unwrap_or_else(|| crate::local::URL.into());
    let base = base.trim_end_matches('/');
    // The context's org and team name them on the context's own server only.
    let theirs = |k: &str| match ctx_server.as_deref() == Some(base) {
        true => ctx_get(k),
        false => None,
    };
    let org = org
        .map(String::from)
        .or_else(|| var("AMPUP_MEMORY_ORG"))
        .or_else(|| theirs("org"));
    let team = var("AMPUP_MEMORY_TEAM").or_else(|| theirs("team"));
    let query: Vec<String> = [
        ("org", org),
        ("team", team),
        ("scope", var("AMPUP_MEMORY_SCOPE")),
    ]
    .into_iter()
    .filter_map(|(param, v)| v.map(|v| format!("{param}={v}")))
    .collect();
    match query.is_empty() {
        true => format!("{base}/mcp"),
        false => format!("{base}/mcp?{}", query.join("&")),
    }
}

/// How the agent reaches the memory's MCP server.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mcp {
    /// Over HTTP, at `--url` (else `AMPUP_MEMORY_URL`), with `AMPUP_MEMORY_TOKEN` where the harness can't read it from
    /// the environment.
    Http,
    /// `memstore mcp`, a command the agent starts: it signs in as the CLI does (`memstore join`) and renews the
    /// token itself, so no URL or token is written anywhere. What `memstore setup` installs.
    SignedIn,
}

/// Install (or, with `remove`, uninstall) memstore in `harness`'s user config, its MCP entry reaching `url` (and
/// `org`). Returns what changed.
pub fn apply(
    harness: Harness,
    url: Option<&str>,
    org: Option<&str>,
    remove: bool,
    dry: bool,
) -> Result<Vec<Change>> {
    apply_with(harness, url, org, Mcp::Http, remove, dry)
}

/// [`apply`], reaching the MCP server as `mcp` says. With [`Mcp::SignedIn`] the hooks carry no URL either: they
/// read the CLI's context when they run.
pub fn apply_with(
    harness: Harness,
    url: Option<&str>,
    org: Option<&str>,
    mcp: Mcp,
    remove: bool,
    dry: bool,
) -> Result<Vec<Change>> {
    let home = home()?;
    let signed_in = mcp == Mcp::SignedIn;
    let url = if signed_in { None } else { url };
    let org = if signed_in { None } else { org };
    // The stdio entry each harness takes: the command and its arguments.
    let stdio = json!({"command": exe(), "args": ["mcp"]});
    if let Some(u) = url {
        let p = reqwest::Url::parse(u).with_context(|| format!("--url {u} is not a URL"))?;
        if !matches!(p.scheme(), "http" | "https") || p.host_str().is_none() {
            bail!("--url is the AmpUp Memory server, http(s)://host[:port], not {u}");
        }
    }
    let add = match remove {
        true => vec![],
        false => hooks_for(harness, url, org),
    };
    let token = std::env::var("AMPUP_MEMORY_TOKEN")
        .ok()
        .filter(|t| !t.is_empty() && !signed_in);
    let mut out = vec![];
    let mut done = |path: PathBuf, changed: bool, what: &str| {
        if changed {
            out.push(Change {
                path,
                what: what.to_string(),
            });
        }
    };
    let verb = if remove { "removed" } else { "added" };
    match harness {
        Harness::ClaudeCode => bail!(
            "Claude Code installs AmpUp Memory as a plugin: run /plugin marketplace add {MARKETPLACE_REPO}, then \
             /plugin install {PLUGIN}, in Claude Code"
        ),
        Harness::Grok => bail!(
            "Grok Bot runs no hooks that see the conversation: add the memory's MCP server as a custom connector \
             (<server>/mcp, signed in with OAuth) and the skills of ampup-memory-grok from {MARKETPLACE_REPO}"
        ),
        Harness::Droid => {
            // Its hooks.json is the map of events itself.
            let path = home.join(".factory/hooks.json");
            let mut doc = read_json(&path)?;
            let events = doc.as_object_mut().context("checked an object")?;
            set_events(events, &add, &path)?;
            let changed = write(&path, &format!("{:#}\n", doc), dry, false)?;
            done(path, changed, &format!("hooks {verb}"));
            let path = home.join(".factory/mcp.json");
            let entry = match signed_in {
                true => json!({"type": "stdio", "command": exe(), "args": ["mcp"]}),
                false => with_bearer(
                    json!({"type": "http", "url": mcp_url(url, org)}),
                    token.as_deref(),
                ),
            };
            let changed = set_server(
                &path,
                "mcpServers",
                (!remove).then_some(entry),
                dry,
                token.is_some(),
            )?;
            done(path, changed, &format!("MCP server {verb}"));
        }
        Harness::Qwen => {
            let path = home.join(".qwen/settings.json");
            let mut doc = read_json(&path)?;
            set_hooks(&mut doc, &add, &path)?;
            let servers = object_at(&mut doc, "mcpServers", &path)?;
            remove_servers(servers);
            if !remove {
                let entry = match signed_in {
                    true => stdio.clone(),
                    false => with_bearer(json!({"httpUrl": mcp_url(url, org)}), token.as_deref()),
                };
                servers.insert(MCP_SERVER.into(), entry);
            }
            prune(&mut doc, "mcpServers");
            let changed = write(
                &path,
                &format!("{:#}\n", doc),
                dry,
                token.is_some() && !remove,
            )?;
            done(path, changed, &format!("hooks and MCP server {verb}"));
        }
        Harness::Devin => {
            // Devin Local's user config: its hooks under `hooks`, Claude Code's shape.
            let path = home.join(".config/devin/config.json");
            let mut doc = read_json(&path)?;
            set_hooks(&mut doc, &add, &path)?;
            let changed = write(&path, &format!("{:#}\n", doc), dry, false)?;
            done(path, changed, &format!("hooks {verb}"));
            let path = home.join(".config/devin/mcp_config.json");
            let entry = match signed_in {
                true => stdio.clone(),
                false => {
                    let mut e = json!({"url": mcp_url(url, org), "transport": "http"});
                    if token.is_some() {
                        e["headers"] =
                            json!({"Authorization": format!("Bearer ${{env:{}}}", token_var())});
                    }
                    e
                }
            };
            let changed = set_server(&path, "mcpServers", (!remove).then_some(entry), dry, false)?;
            done(path, changed, &format!("MCP server {verb}"));
        }
        Harness::Kiro => {
            // A file of its own, so nothing of the user's is touched.
            let path = home.join(".kiro/hooks/ampup-memory.json");
            match remove {
                true => {
                    if remove_file(&path, dry)? {
                        done(path, true, "hooks removed");
                    }
                }
                false => {
                    let hooks: Vec<Value> = add.iter().map(|(_, e)| e.clone()).collect();
                    let doc = json!({"version": "v1", "hooks": hooks});
                    let changed = write(&path, &format!("{:#}\n", doc), dry, false)?;
                    done(path, changed, "hooks added");
                }
            }
            let path = home.join(".kiro/settings/mcp.json");
            let entry = match signed_in {
                true => stdio.clone(),
                false => {
                    let mut e = json!({"url": mcp_url(url, org)});
                    if token.is_some() {
                        e["headers"] =
                            json!({"Authorization": format!("Bearer ${{{}}}", token_var())});
                    }
                    e
                }
            };
            let changed = set_server(&path, "mcpServers", (!remove).then_some(entry), dry, false)?;
            done(path, changed, &format!("MCP server {verb}"));
        }
        Harness::OpenCode | Harness::Kilo => {
            // The plugin (the hooks), then the MCP entry in the agent's own config (`mcp.<name>`, its V1 shape).
            // OpenCode 1.x loads `plugins/`, Kilo Code 7 `plugin/` (and `plugins/`).
            let (dir, plugins, config) = match harness {
                Harness::OpenCode => ("opencode", "plugins", "opencode"),
                _ => ("kilo", "plugin", "kilo"),
            };
            let dir = xdg_config(&home).join(dir);
            let path = dir.join(plugins).join(PLUGIN_FILE);
            let changed = put_plugin(harness, &path, remove, dry)?;
            done(path, changed, &format!("plugin {verb}"));
            let path = json_config(&dir, config);
            let entry = match signed_in {
                true => json!({"type": "local", "command": [exe(), "mcp"], "enabled": true}),
                false => {
                    let mut e =
                        json!({"type": "remote", "url": mcp_url(url, org), "enabled": true});
                    if token.is_some() {
                        e["headers"] =
                            json!({"Authorization": format!("Bearer {{env:{}}}", token_var())});
                    }
                    e
                }
            };
            let changed = set_server(&path, "mcp", (!remove).then_some(entry), dry, false)?;
            done(path, changed, &format!("MCP server {verb}"));
        }
        Harness::Amp => {
            let path = xdg_config(&home).join("amp/plugins").join(PLUGIN_FILE);
            let changed = put_plugin(harness, &path, remove, dry)?;
            done(path, changed, &format!("plugin {verb}"));
            let path = xdg_config(&home).join("amp/settings.json");
            let entry = match signed_in {
                true => stdio.clone(),
                false => with_bearer(json!({"url": mcp_url(url, org)}), token.as_deref()),
            };
            let changed = set_server(
                &path,
                "amp.mcpServers",
                (!remove).then_some(entry),
                dry,
                token.is_some(),
            )?;
            done(path, changed, &format!("MCP server {verb}"));
        }
        Harness::Hermes => {
            let mcp = match signed_in {
                true => McpEntry::Stdio,
                false => McpEntry::Url(mcp_url(url, org), token.clone()),
            };
            for (path, changed, what) in hermes(&home, url, org, mcp, remove, dry)? {
                done(path, changed, &what);
            }
        }
        Harness::Cline => {
            let mcp = match signed_in {
                true => McpEntry::Stdio,
                false => McpEntry::Url(mcp_url(url, org), token.clone()),
            };
            for (path, changed, what) in cline(&home, url, org, mcp, remove, dry)? {
                done(path, changed, &what);
            }
        }
        Harness::Codex => {
            let path = home.join(".codex/hooks.json");
            let mut doc = read_json(&path)?;
            set_hooks(&mut doc, &add, &path)?;
            let changed = write(&path, &format!("{:#}\n", doc), dry, false)?;
            done(path, changed, &format!("hooks {verb}"));
            // MCP: a [mcp_servers.ampup-memory] table, its token read from AMPUP_MEMORY_TOKEN by Codex itself.
            let path = home.join(".codex/config.toml");
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
            };
            let mut toml: toml_edit::DocumentMut = text
                .parse()
                .with_context(|| format!("{} is not TOML", path.display()))?;
            if !toml.contains_key("mcp_servers") {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                toml["mcp_servers"] = toml_edit::Item::Table(t);
            }
            let servers = toml["mcp_servers"]
                .as_table_mut()
                .context("mcp_servers in config.toml is not a table")?;
            for name in SERVER_NAMES {
                servers.remove(name);
            }
            if !remove {
                let mut t = toml_edit::Table::new();
                if signed_in {
                    t["command"] = toml_edit::value(exe());
                    let mut args = toml_edit::Array::new();
                    args.push("mcp");
                    t["args"] = toml_edit::value(args);
                } else {
                    t["url"] = toml_edit::value(mcp_url(url, org));
                    t["bearer_token_env_var"] = toml_edit::value(token_var().as_ref());
                }
                servers.insert(MCP_SERVER, toml_edit::Item::Table(t));
            }
            let changed = write(&path, &toml.to_string(), dry, false)?;
            done(path, changed, &format!("MCP server {verb}"));
        }
        Harness::Gemini => {
            let path = home.join(".gemini/settings.json");
            let mut doc = read_json(&path)?;
            set_hooks(&mut doc, &add, &path)?;
            let servers = object_at(&mut doc, "mcpServers", &path)?;
            remove_servers(servers);
            if !remove {
                let mut entry = match signed_in {
                    true => stdio.clone(),
                    false => json!({"httpUrl": mcp_url(url, org)}),
                };
                if let Some(t) = &token {
                    entry["headers"] = json!({"Authorization": format!("Bearer {t}")});
                }
                servers.insert(MCP_SERVER.into(), entry);
            }
            prune(&mut doc, "mcpServers");
            let changed = write(
                &path,
                &format!("{:#}\n", doc),
                dry,
                token.is_some() && !remove,
            )?;
            done(path, changed, &format!("hooks and MCP server {verb}"));
        }
        Harness::Cursor => {
            let path = home.join(".cursor/hooks.json");
            let mut doc = read_json(&path)?;
            if doc.get("version").is_none() {
                doc["version"] = json!(1);
            }
            set_hooks(&mut doc, &add, &path)?;
            let changed = write(&path, &format!("{:#}\n", doc), dry, false)?;
            done(path, changed, &format!("hooks {verb}"));
            let path = home.join(".cursor/mcp.json");
            let mut doc = read_json(&path)?;
            let servers = object_at(&mut doc, "mcpServers", &path)?;
            remove_servers(servers);
            if !remove {
                servers.insert(
                    MCP_SERVER.into(),
                    match signed_in {
                        true => stdio.clone(),
                        false => json!({"url": mcp_url(url, org),
                                        "headers": {"Authorization": format!("Bearer ${{env:{}}}", token_var())}}),
                    },
                );
            }
            prune(&mut doc, "mcpServers");
            let changed = write(&path, &format!("{:#}\n", doc), dry, false)?;
            done(path, changed, &format!("MCP server {verb}"));
        }
        Harness::Copilot => {
            // Its own file, so nothing else of the user's is touched. An older install's, by memstore's name, goes
            // either way: Copilot would run both.
            let old = home.join(".copilot/hooks/memstore.json");
            if old.exists() {
                if !dry {
                    std::fs::remove_file(&old)
                        .with_context(|| format!("remove {}", old.display()))?;
                }
                done(old, true, "hooks removed (an older install's)");
            }
            let path = home.join(".copilot/hooks/ampup-memory.json");
            match remove {
                true if path.exists() => {
                    if !dry {
                        std::fs::remove_file(&path)
                            .with_context(|| format!("remove {}", path.display()))?;
                    }
                    done(path, true, "hooks removed");
                }
                true => {}
                false => {
                    let mut doc = json!({"version": 1});
                    set_hooks(&mut doc, &add, &path)?;
                    let changed = write(&path, &format!("{:#}\n", doc), dry, false)?;
                    done(path, changed, "hooks added");
                }
            }
            let path = home.join(".copilot/mcp-config.json");
            let mut doc = read_json(&path)?;
            let servers = object_at(&mut doc, "mcpServers", &path)?;
            remove_servers(servers);
            if !remove {
                let mut entry = match signed_in {
                    true => {
                        json!({"type": "local", "command": exe(), "args": ["mcp"], "tools": ["*"]})
                    }
                    false => json!({"type": "http", "url": mcp_url(url, org), "tools": ["*"]}),
                };
                if let Some(t) = &token {
                    entry["headers"] = json!({"Authorization": format!("Bearer {t}")});
                }
                servers.insert(MCP_SERVER.into(), entry);
            }
            prune(&mut doc, "mcpServers");
            let changed = write(
                &path,
                &format!("{:#}\n", doc),
                dry,
                token.is_some() && !remove,
            )?;
            done(path, changed, &format!("MCP server {verb}"));
        }
    }
    Ok(out)
}

/// `entry` with `token` as its bearer, written in: for an agent whose MCP config can't read it from the
/// environment. Without one, the agent signs in itself (OAuth) where it can.
fn with_bearer(mut entry: Value, token: Option<&str>) -> Value {
    if let Some(t) = token {
        entry["headers"] = json!({"Authorization": format!("Bearer {t}")});
    }
    entry
}

/// Sets (or, with no `entry`, takes out) the MCP server's entry in the JSON config at `path`, under `key`
/// (`mcpServers`, OpenCode's `mcp`, Amp's `amp.mcpServers`): every name it had, new and old, taken out first. The
/// rest of the file is kept. Whether it changed.
fn set_server(
    path: &Path,
    key: &str,
    entry: Option<Value>,
    dry: bool,
    secret: bool,
) -> Result<bool> {
    let mut doc = read_json(path)?;
    let servers = object_at(&mut doc, key, path)?;
    remove_servers(servers);
    let adding = entry.is_some();
    if let Some(e) = entry {
        servers.insert(MCP_SERVER.into(), e);
    }
    prune(&mut doc, key);
    write(path, &format!("{:#}\n", doc), dry, secret && adding)
}

/// Removes a file this installer wrote; whether there was one.
fn remove_file(path: &Path, dry: bool) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    if !dry {
        std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(true)
}

/// The JSON config `<name>.json` in `dir`, or the `<name>.jsonc` that is there instead (read as JSON: one with
/// comments is refused, never rewritten without them).
fn json_config(dir: &Path, name: &str) -> PathBuf {
    let plain = dir.join(format!("{name}.json"));
    let commented = dir.join(format!("{name}.jsonc"));
    match !plain.exists() && commented.exists() {
        true => commented,
        false => plain,
    }
}

/// The plugin file an agent that takes plugins loads: what `hooks install` writes into its plugin directory.
const PLUGIN_FILE: &str = "ampup-memory.ts";

/// The plugins this build carries (`plugins/integrations/`), embedded when it was built: None where it had none.
mod plugins {
    include!(concat!(env!("OUT_DIR"), "/plugins.rs"));
}

/// The plugin for `harness`. It runs the client it finds itself: `AMPUP_MEMORY_CLIENT`, else
/// ~/.local/bin/ampup-memory-client (where install.sh puts it), else the PATH's.
fn plugin_text(harness: Harness) -> Result<&'static str> {
    let text = match harness {
        Harness::OpenCode => plugins::OPENCODE,
        Harness::Kilo => plugins::KILO.or(plugins::OPENCODE),
        Harness::Amp => plugins::AMP,
        _ => None,
    };
    let text = text.with_context(|| {
        format!(
            "this build of {} carries no {} plugin: install a newer release (install.sh), then run this again",
            crate::naming::CLIENT_BIN,
            harness.label()
        )
    })?;
    Ok(text)
}

/// Writes (or, with `remove`, removes) the plugin at `path`. Whether it changed.
fn put_plugin(harness: Harness, path: &Path, remove: bool, dry: bool) -> Result<bool> {
    match remove {
        true => remove_file(path, dry),
        false => write(path, plugin_text(harness)?, dry, false),
    }
}

/// The MCP server's entry in each agent's config.
const MCP_SERVER: &str = crate::naming::MCP_SERVER;
/// Its entry's names, new and old: an install replaces an older one's entry, an uninstall removes either.
const SERVER_NAMES: [&str; 3] = [
    crate::naming::MCP_SERVER,
    crate::naming::OLD_MCP_SERVERS[0],
    crate::naming::OLD_MCP_SERVERS[1],
];

fn remove_servers(servers: &mut serde_json::Map<String, Value>) {
    for name in SERVER_NAMES {
        servers.remove(name);
    }
}

/// The variable an agent reads the token from: `AMPUP_MEMORY_TOKEN`, or `MEMSTORE_TOKEN` where that is what this
/// machine sets (the agent reads it itself, so it must be the name its environment has).
fn token_var() -> std::borrow::Cow<'static, str> {
    const TOKEN: &str = "AMPUP_MEMORY_TOKEN";
    match crate::naming::set_by_old_name(TOKEN) {
        true => crate::naming::old_env_name(TOKEN),
        false => TOKEN.into(),
    }
}

/// Whether installing for `harness` writes `AMPUP_MEMORY_TOKEN` itself into a file (its MCP config can't read it
/// from the environment).
pub fn writes_token(harness: Harness) -> bool {
    matches!(
        harness,
        Harness::Gemini
            | Harness::Copilot
            | Harness::Droid
            | Harness::Qwen
            | Harness::Hermes
            | Harness::Cline
            | Harness::Amp
    )
}

/// `hooks install|uninstall --harness <name> [--url] [--org] [--dry-run]`, as `bin` (`ampup-memory` or
/// `ampup-memory-client`): applies it and says what changed.
pub fn cli(
    bin: &str,
    action: &str,
    harness: &str,
    url: Option<&str>,
    org: Option<&str>,
    dry: bool,
) -> Result<()> {
    let h = Harness::parse(harness)?;
    let remove = match action {
        "install" => false,
        "uninstall" => true,
        other => bail!("{bin} hooks install|uninstall, not {other}"),
    };
    // No server named: the hooks and the MCP server follow the context `join` makes (before or after this), as
    // `setup` installs them; a server named (`--url`, or AMPUP_MEMORY_URL) is written in.
    let named = url.is_some() || std::env::var("AMPUP_MEMORY_URL").is_ok_and(|u| !u.is_empty());
    let mcp = match named {
        true => Mcp::Http,
        false => Mcp::SignedIn,
    };
    if org.is_some() && !named {
        println!(
            "note: --org names the org with --url; without one the hooks and MCP server use the org `{bin} join` signed in to"
        );
    }
    let changes = apply_with(h, url, org, mcp, remove, dry)?;
    if dry
        && !remove
        && let Some(block) = hooks_block(h, url.filter(|_| named), org.filter(|_| named))
    {
        println!("the hooks, as they would be written (for an org-wide config):\n{block}");
    }
    let lead = if dry { "would change" } else { "changed" };
    if changes.is_empty() {
        println!("{}: nothing to change", h.name());
    }
    for c in &changes {
        match c.what.starts_with("not installed") {
            true => println!("note: {}: {}", c.path.display(), c.what),
            false => println!("{lead} {}: {}", c.path.display(), c.what),
        }
    }
    let token = std::env::var("AMPUP_MEMORY_TOKEN").is_ok_and(|t| !t.is_empty());
    if !remove && writes_token(h) && token {
        println!(
            "note: {} can't read AMPUP_MEMORY_TOKEN from the environment, so its MCP entry holds the token",
            h.name()
        );
    }
    if !remove && named && !token && signs_in_itself(h) {
        println!(
            "note: {} signs in to the MCP server itself (OAuth) the first time it uses it{}",
            h.label(),
            match h {
                Harness::Codex => ": `codex mcp login ampup-memory`",
                Harness::Devin => ": `devin mcp login ampup-memory`",
                _ => "",
            }
        );
    }
    if !remove && !dry && !changes.is_empty() {
        println!(
            "restart {} to load them; `{bin} doctor` checks the server and how the last hook went",
            h.label()
        );
    }
    Ok(())
}

/// The hooks `harness` gets, as they are written into its config: its hooks block (JSON; YAML for Hermes; the
/// scripts for Cline). None for an agent whose hooks are a plugin.
fn hooks_block(harness: Harness, url: Option<&str>, org: Option<&str>) -> Option<String> {
    let add = hooks_for(harness, url, org);
    let events = || {
        let mut m = serde_json::Map::new();
        for (event, entry) in &add {
            m.entry(event.to_string())
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .expect("a list")
                .push(entry.clone());
        }
        Value::Object(m)
    };
    match harness {
        Harness::Droid => Some(format!("{:#}", events())),
        Harness::Kiro => {
            let hooks: Vec<Value> = add.iter().map(|(_, e)| e.clone()).collect();
            Some(format!("{:#}", json!({"version": "v1", "hooks": hooks})))
        }
        Harness::Hermes => {
            let mut text = String::new();
            for (event, ours) in HERMES_EVENTS {
                let item = format!(
                    "{{command: {}, timeout: 30}}",
                    yaml_quoted(&command(Harness::Hermes, ours, url, org))
                );
                text = yaml::set_list(&text, "hooks", event, &[item], |_| false).ok()?;
            }
            Some(text.trim_end().to_string())
        }
        Harness::Cline => Some(
            CLINE_EVENTS
                .iter()
                .map(|(name, ours)| format!("{name}: {}", command(Harness::Cline, ours, url, org)))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ if add.is_empty() => None,
        _ => Some(format!("{:#}", json!({"hooks": events()}))),
    }
}

/// Whether the agent's MCP entry, holding no token, has the agent sign in with OAuth (the server's dynamic
/// client registration) rather than send one from the environment.
fn signs_in_itself(harness: Harness) -> bool {
    matches!(
        harness,
        Harness::Droid
            | Harness::Qwen
            | Harness::Devin
            | Harness::Kiro
            | Harness::Cline
            | Harness::Hermes
            | Harness::OpenCode
            | Harness::Amp
            | Harness::Kilo
    )
}

/// Where Claude Code finds the AmpUp Memory plugin: the public repository, whose root is its marketplace.
const MARKETPLACE: &str = "ampup-memory";
const MARKETPLACE_REPO: &str = "A79-ai/ampup-memory";
const PLUGIN: &str = "ampup-memory@ampup-memory";
/// The plugin's name before the rename, and its marketplace's.
const OLD_MARKETPLACE: &str = "memstore";
const OLD_PLUGIN: &str = "memstore@memstore";

/// Declares the AmpUp Memory plugin in Claude Code's user settings (`~/.claude/settings.json`): its marketplace in
/// `extraKnownMarketplaces` and the plugin in `enabledPlugins`. At its next start Claude Code clones the
/// marketplace (a public repository) and installs the plugin, with no prompt. With `remove`, takes both out.
/// Either way the plugin under its old name (`memstore@memstore`) comes out of the file too, so a session isn't
/// recorded twice. Everything else in the file is kept, and it's backed up first.
pub fn claude_code(remove: bool, dry: bool) -> Result<Vec<Change>> {
    let path = home()?.join(".claude/settings.json");
    let mut doc = read_json(&path)?;
    let markets = object_at(&mut doc, "extraKnownMarketplaces", &path)?;
    markets.remove(MARKETPLACE);
    markets.remove(OLD_MARKETPLACE);
    if !remove {
        markets.insert(
            MARKETPLACE.into(),
            json!({"source": {"source": "github", "repo": MARKETPLACE_REPO}}),
        );
    }
    prune(&mut doc, "extraKnownMarketplaces");
    let plugins = object_at(&mut doc, "enabledPlugins", &path)?;
    plugins.remove(PLUGIN);
    plugins.remove(OLD_PLUGIN);
    if !remove {
        plugins.insert(PLUGIN.into(), json!(true));
    }
    prune(&mut doc, "enabledPlugins");
    let changed = write(&path, &format!("{:#}\n", doc), dry, false)?;
    Ok(match changed {
        true => vec![Change {
            path,
            what: format!(
                "the plugin {}",
                if remove {
                    "removed"
                } else {
                    "declared: Claude Code installs it at its next start"
                }
            ),
        }],
        false => vec![],
    })
}

/// The coding agents set up on this machine: those whose user config directory exists (Claude Code also when
/// `claude` is on the PATH).
pub fn detected() -> Vec<Harness> {
    let Ok(home) = home() else {
        return vec![];
    };
    let on_path = |cmd: &str| {
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
    };
    let config = xdg_config(&home);
    [
        (Harness::ClaudeCode, home.join(".claude")),
        (Harness::Codex, home.join(".codex")),
        (Harness::Gemini, home.join(".gemini")),
        (Harness::Cursor, home.join(".cursor")),
        (Harness::Copilot, home.join(".copilot")),
        (Harness::Droid, home.join(".factory")),
        (Harness::Qwen, home.join(".qwen")),
        (Harness::Hermes, hermes_home(&home)),
        (Harness::Kiro, home.join(".kiro")),
        (Harness::Devin, home.join(".config/devin")),
        (Harness::Cline, home.join(".cline")),
        (Harness::OpenCode, config.join("opencode")),
        (Harness::Amp, config.join("amp")),
        (Harness::Kilo, config.join("kilo")),
    ]
    .into_iter()
    .filter(|(h, dir)| dir.is_dir() || (*h == Harness::ClaudeCode && on_path("claude")))
    // An agent whose plugin this build doesn't carry is left for a newer release.
    .filter(|(h, _)| {
        !matches!(h, Harness::OpenCode | Harness::Amp | Harness::Kilo) || plugin_text(*h).is_ok()
    })
    .map(|(h, _)| h)
    .collect()
}

/// The MCP entry an agent gets: this binary over stdio (`setup`'s, signed in as the CLI is), or the server's
/// endpoint, with the token where one was set.
enum McpEntry {
    Stdio,
    Url(String, Option<String>),
}

/// Where OpenCode, Kilo Code and Amp keep their config: `XDG_CONFIG_HOME` (when absolute), else `~/.config`.
fn xdg_config(home: &Path) -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(d) if d.is_absolute() => d,
        _ => home.join(".config"),
    }
}

/// Hermes' home: `HERMES_HOME`, else `~/.hermes`.
fn hermes_home(home: &Path) -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .filter(|h| !h.is_empty())
        .map_or_else(|| home.join(".hermes"), PathBuf::from)
}

/// Hermes: shell hooks under `hooks:` in `config.yaml`, each approved in `shell-hooks-allowlist.json` (else a
/// run without a terminal skips them), and the MCP server under `mcp_servers:`. The YAML is edited as text, a
/// line an entry, so the user's comments and layout stay; a `hooks:` or `mcp_servers:` it can't read as a block
/// is refused, never rewritten.
fn hermes(
    home: &Path,
    url: Option<&str>,
    org: Option<&str>,
    mcp: McpEntry,
    remove: bool,
    dry: bool,
) -> Result<Vec<(PathBuf, bool, String)>> {
    let dir = hermes_home(home);
    let verb = if remove { "removed" } else { "added" };
    let events = HERMES_EVENTS;
    let hooks: Vec<(&str, String)> = match remove {
        true => vec![],
        false => events
            .iter()
            .map(|(e, ours)| (*e, command(Harness::Hermes, ours, url, org)))
            .collect(),
    };
    let path = dir.join("config.yaml");
    let before = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut text = before.clone();
    for (event, _) in events {
        let cmd: Vec<String> = hooks
            .iter()
            .filter(|(e, _)| *e == event)
            .map(|(_, c)| format!("{{command: {}, timeout: 30}}", yaml_quoted(c)))
            .collect();
        text = yaml::set_list(&text, "hooks", event, &cmd, |item| {
            yaml::flow_command(item).is_some_and(|c| ours_command(&c))
        })
        .with_context(|| format!("{}: hooks", path.display()))?;
    }
    let entry = match (&mcp, remove) {
        (_, true) => None,
        (McpEntry::Stdio, false) => {
            Some(format!("{{command: {}, args: [mcp]}}", yaml_quoted(&exe())))
        }
        (McpEntry::Url(u, token), false) => Some(match token {
            Some(t) => format!(
                "{{url: {}, headers: {{Authorization: {}}}}}",
                yaml_quoted(u),
                yaml_quoted(&format!("Bearer {t}"))
            ),
            None => format!("{{url: {}, auth: oauth}}", yaml_quoted(u)),
        }),
    };
    text = yaml::set_entry(
        &text,
        "mcp_servers",
        &SERVER_NAMES,
        MCP_SERVER,
        entry.as_deref(),
    )
    .with_context(|| format!("{}: mcp_servers", path.display()))?;
    let secret = matches!(&mcp, McpEntry::Url(_, Some(_))) && !remove;
    let changed = match text == before {
        true => false,
        false => write(&path, &text, dry, secret)?,
    };
    let mut out = vec![(path, changed, format!("hooks and MCP server {verb}"))];
    // Approved, so they run in a session with no terminal to ask in (and none is asked in one with).
    let path = dir.join("shell-hooks-allowlist.json");
    let mut doc = read_json(&path)?;
    if !doc["approvals"].is_array() {
        doc["approvals"] = json!([]);
    }
    let list = doc["approvals"].as_array_mut().context("checked a list")?;
    list.retain(|a| !a["command"].as_str().is_some_and(ours_command));
    let now = iso_now();
    for (event, cmd) in &hooks {
        list.push(json!({"event": event, "command": cmd, "approved_at": now,
                         "script_mtime_at_approval": Value::Null}));
    }
    let empty = list.is_empty();
    let changed = match (empty, path.exists()) {
        // Nothing approved and no file: none made.
        (true, false) => false,
        _ => write(&path, &format!("{:#}\n", doc), dry, true)?,
    };
    out.push((path, changed, format!("hooks' approval {verb}")));
    Ok(out)
}

/// Hermes' hook events, and the hook each runs.
const HERMES_EVENTS: [(&str, &str); 5] = [
    ("on_session_start", "session-start"),
    ("pre_llm_call", "prompt"),
    ("post_llm_call", "stop"),
    ("post_compaction", "session-end"),
    ("on_session_finalize", "end"),
];

/// Cline's hook files, and the hook each runs.
const CLINE_EVENTS: [(&str, &str); 7] = [
    ("TaskStart", "session-start"),
    ("TaskResume", "session-start"),
    ("UserPromptSubmit", "prompt"),
    ("TaskComplete", "stop"),
    ("TaskCancel", "session-end"),
    ("PreCompact", "session-end"),
    ("SessionShutdown", "end"),
];

/// `s` as a YAML single-quoted scalar.
fn yaml_quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Now, as ISO 8601 in UTC (`2026-11-20T23:54:30Z`).
fn iso_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 as a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// Editing a YAML config as text, one top-level block mapping (`hooks:`, `mcp_servers:`) at a time: what this
/// installer writes is one line an entry (flow style), so it is found and taken out by its line, and the rest of
/// the file is left byte for byte.
mod yaml {
    use anyhow::{Result, bail};

    fn indent(line: &str) -> usize {
        line.len() - line.trim_start_matches(' ').len()
    }

    /// Whether `line` holds nothing: blank, or only a comment.
    fn blank(line: &str) -> bool {
        let t = line.trim();
        t.is_empty() || t.starts_with('#')
    }

    /// The key a mapping line names (`key:` or `key: value`), its quotes taken off.
    fn key_of(line: &str) -> Option<&str> {
        let t = line.trim_start();
        if t.starts_with('-') || t.starts_with('#') {
            return None;
        }
        let (k, rest) = t.split_once(':')?;
        if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
            return None;
        }
        Some(k.trim().trim_matches(['"', '\'']))
    }

    /// The value after `key:` on its line, comments aside: empty for a block.
    fn inline_value(line: &str) -> &str {
        let (_, rest) = line.split_once(':').unwrap_or_default();
        let rest = rest.trim();
        match rest.starts_with('#') {
            true => "",
            false => rest,
        }
    }

    /// The top-level `top:` block: (its key's line, the line after its last). None where there is none. Refused
    /// where it isn't a block mapping (an inline value other than empty, `{}` or `null`).
    fn block(lines: &[&str], top: &str) -> Result<Option<(usize, usize)>> {
        let Some(at) = lines
            .iter()
            .position(|l| indent(l) == 0 && key_of(l) == Some(top))
        else {
            return Ok(None);
        };
        if !matches!(inline_value(lines[at]), "" | "{}" | "null" | "~") {
            bail!(
                "{top}: is written inline: make it a block (one entry a line), then install again"
            );
        }
        let mut end = at + 1;
        let mut last = at + 1;
        while end < lines.len() {
            let l = lines[end];
            if !blank(l) && indent(l) == 0 {
                break;
            }
            end += 1;
            if !blank(l) {
                last = end;
            }
        }
        Ok(Some((at, last)))
    }

    /// The command in one of our flow entries: `- {command: '…', timeout: 30}`.
    pub fn flow_command(item: &str) -> Option<String> {
        let t = item.trim_start().strip_prefix('-')?.trim_start();
        let t = t
            .strip_prefix('{')?
            .trim_start()
            .strip_prefix("command:")?
            .trim_start();
        let quote = t.chars().next()?;
        match quote {
            '\'' => {
                let mut out = String::new();
                let mut chars = t[1..].chars().peekable();
                while let Some(c) = chars.next() {
                    match (c, chars.peek()) {
                        ('\'', Some('\'')) => {
                            out.push('\'');
                            chars.next();
                        }
                        ('\'', _) => return Some(out),
                        (c, _) => out.push(c),
                    }
                }
                None
            }
            '"' => {
                let end = t[1..].find('"')?;
                Some(t[1..1 + end].to_string())
            }
            _ => None,
        }
    }

    /// `text` with `top:` → `key:`'s list holding `items` (each a flow value) after the user's own, ours (those
    /// `ours` recognises) taken out first. A `key:` or `top:` left empty by taking ours out is taken out too.
    pub fn set_list(
        text: &str,
        top: &str,
        key: &str,
        items: &[String],
        ours: impl Fn(&str) -> bool,
    ) -> Result<String> {
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        let view: Vec<&str> = lines.iter().map(String::as_str).collect();
        let Some((at, end)) = block(&view, top)? else {
            if items.is_empty() {
                return Ok(text.to_string());
            }
            let mut out = text.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("{top}:\n  {key}:\n"));
            for i in items {
                out.push_str(&format!("    - {i}\n"));
            }
            return Ok(out);
        };
        // An empty inline `top: {}` becomes a block.
        if !inline_value(&lines[at]).is_empty() {
            lines[at] = format!("{top}:");
        }
        let child = (at + 1..end)
            .map(|i| &lines[i])
            .find(|l| !blank(l))
            .map_or(2, |l| indent(l));
        let found =
            (at + 1..end).find(|&i| indent(&lines[i]) == child && key_of(&lines[i]) == Some(key));
        match found {
            None => {
                if items.is_empty() {
                    return Ok(text.to_string());
                }
                let mut add = vec![format!("{}{key}:", " ".repeat(child))];
                add.extend(
                    items
                        .iter()
                        .map(|i| format!("{}- {i}", " ".repeat(child + 2))),
                );
                lines.splice(end..end, add);
            }
            Some(k) => {
                if !matches!(inline_value(&lines[k]), "" | "[]" | "null" | "~") {
                    bail!(
                        "{top}.{key} is written inline: make it a list (one entry a line), then install again"
                    );
                }
                // Its entries: the lines after it, until one as indented as it (or less) that isn't a `- `
                // at its own indent.
                let mut e = k + 1;
                while e < end {
                    let l = &lines[e];
                    let i = indent(l);
                    if !blank(l) && (i < child || (i == child && !l.trim_start().starts_with('-')))
                    {
                        break;
                    }
                    e += 1;
                }
                let item_indent = (k + 1..e)
                    .map(|i| &lines[i])
                    .find(|l| l.trim_start().starts_with('-'))
                    .map_or(child + 2, |l| indent(l));
                let mut kept: Vec<String> = lines[k + 1..e]
                    .iter()
                    .filter(|l| !(l.trim_start().starts_with('-') && ours(l)))
                    .cloned()
                    .collect();
                // Ours after the user's last entry.
                let last = kept.iter().rposition(|l| !blank(l)).map_or(0, |p| p + 1);
                let mine: Vec<String> = items
                    .iter()
                    .map(|i| format!("{}- {i}", " ".repeat(item_indent)))
                    .collect();
                kept.splice(last..last, mine);
                let empty = kept.iter().all(|l| blank(l));
                match empty && inline_value(&lines[k]).is_empty() {
                    // Nothing but ours was there: the key goes too.
                    true => {
                        lines.splice(k..e, kept);
                    }
                    false => {
                        if !inline_value(&lines[k]).is_empty() {
                            lines[k] = format!("{}{key}:", " ".repeat(child));
                        }
                        lines.splice(k + 1..e, kept);
                    }
                }
            }
        }
        Ok(drop_empty(lines, top, text))
    }

    /// `text` with `top:` → `key:` set to the flow value `value` (or, with None, taken out), any of `names`
    /// taken out first: each was this installer's, one line.
    pub fn set_entry(
        text: &str,
        top: &str,
        names: &[&str],
        key: &str,
        value: Option<&str>,
    ) -> Result<String> {
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        let view: Vec<&str> = lines.iter().map(String::as_str).collect();
        let Some((at, end)) = block(&view, top)? else {
            let Some(v) = value else {
                return Ok(text.to_string());
            };
            let mut out = text.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("{top}:\n  {key}: {v}\n"));
            return Ok(out);
        };
        if !inline_value(&lines[at]).is_empty() {
            lines[at] = format!("{top}:");
        }
        let child = (at + 1..end)
            .map(|i| &lines[i])
            .find(|l| !blank(l))
            .map_or(2, |l| indent(l));
        let mut end = end;
        let mut i = at + 1;
        while i < end {
            let l = &lines[i];
            if indent(l) == child && key_of(l).is_some_and(|k| names.contains(&k)) {
                // Its value is on its line (ours always is); a block under it goes with it.
                let mut e = i + 1;
                while e < end && (blank(&lines[e]) || indent(&lines[e]) > child) {
                    e += 1;
                }
                if !inline_value(l).starts_with('{') && e > i + 1 {
                    bail!(
                        "{top}.{} was written by hand: remove it, then install again",
                        key_of(l).unwrap_or(key)
                    );
                }
                lines.drain(i..i + 1);
                end -= 1;
                continue;
            }
            i += 1;
        }
        if let Some(v) = value {
            let last = (at + 1..end)
                .rev()
                .find(|&i| !blank(&lines[i]))
                .map_or(at + 1, |i| i + 1);
            lines.insert(last, format!("{}{key}: {v}", " ".repeat(child)));
        }
        Ok(drop_empty(lines, top, text))
    }

    /// `lines` joined, `top:` taken out where nothing is left under it; the trailing newline as `text` had it.
    fn drop_empty(mut lines: Vec<String>, top: &str, text: &str) -> String {
        let view: Vec<&str> = lines.iter().map(String::as_str).collect();
        if let Ok(Some((at, end))) = block(&view, top)
            && (at + 1..end).all(|i| blank(&lines[i]))
        {
            lines.remove(at);
        }
        let mut out = lines.join("\n");
        if (text.ends_with('\n') || text.is_empty()) && !out.is_empty() {
            out.push('\n');
        }
        out
    }
}

/// Cline's hooks: an executable a hook, named for its event, in `~/Documents/Cline/Hooks` (which both the
/// extension and the `cline` CLI read). One file a hook name: a file of the user's by that name is left as it is,
/// and that hook isn't installed (said so). The MCP server goes in `cline_mcp_settings.json`, streamable HTTP.
fn cline(
    home: &Path,
    url: Option<&str>,
    org: Option<&str>,
    mcp: McpEntry,
    remove: bool,
    dry: bool,
) -> Result<Vec<(PathBuf, bool, String)>> {
    let dir = home.join("Documents/Cline/Hooks");
    let mut out = vec![];
    for (name, ours) in CLINE_EVENTS {
        let path = dir.join(name);
        let theirs = std::fs::read_to_string(&path)
            .ok()
            .filter(|t| !t.contains(CLINE_MARK));
        if theirs.is_some() || (path.exists() && std::fs::read_to_string(&path).is_err()) {
            if !remove {
                out.push((
                    path,
                    true,
                    format!("not installed: it is the user's own {name} hook (Cline runs one file a hook): left as it is"),
                ));
            }
            continue;
        }
        if remove {
            let gone = remove_file(&path, dry)?;
            out.push((path, gone, "hook removed".into()));
            continue;
        }
        let script = format!(
            "#!/bin/sh\n{CLINE_MARK} ({} hooks install --harness cline): replaced by the next install, removed by\n\
             # `hooks uninstall --harness cline`.\n{}\n",
            crate::naming::CLIENT_BIN,
            command(Harness::Cline, ours, url, org)
        );
        let same = std::fs::read_to_string(&path).is_ok_and(|t| t == script);
        if !same && !dry {
            create_dir(&dir)?;
            put(&path, &script, 0o700).with_context(|| format!("write {}", path.display()))?;
        }
        out.push((path, !same, "hook added".into()));
    }
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let path = var("CLINE_MCP_SETTINGS_PATH").unwrap_or_else(|| {
        var("CLINE_DATA_DIR")
            .unwrap_or_else(|| home.join(".cline/data"))
            .join("settings/cline_mcp_settings.json")
    });
    let (entry, secret) = match (mcp, remove) {
        (_, true) => (None, false),
        (McpEntry::Stdio, false) => (
            Some(json!({"type": "stdio", "command": exe(), "args": ["mcp"]})),
            false,
        ),
        (McpEntry::Url(u, token), false) => (
            Some(with_bearer(
                json!({"type": "streamableHttp", "url": u}),
                token.as_deref(),
            )),
            token.is_some(),
        ),
    };
    let verb = if remove { "removed" } else { "added" };
    let changed = set_server(&path, "mcpServers", entry, dry, secret)?;
    out.push((path, changed, format!("MCP server {verb}")));
    Ok(out)
}

/// The line that marks a Cline hook file as this installer's.
const CLINE_MARK: &str = "# AmpUp Memory's Cline hook";
