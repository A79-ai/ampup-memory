//! `memstore hooks install --harness <name>`: registers memstore's hooks and MCP server in a coding agent's
//! user-level config, so its sessions are remembered and what the memory knows is recalled into new ones.
//! `memstore hooks uninstall --harness <name>` takes them out again.
//!
//! | harness | hooks file | MCP file | recall |
//! |---|---|---|---|
//! | codex | `~/.codex/hooks.json` | `~/.codex/config.toml` | session start; advice and facts at the first prompt |
//! | gemini | `~/.gemini/settings.json` | the same | session start; advice and facts at the first prompt |
//! | cursor | `~/.cursor/hooks.json` | `~/.cursor/mcp.json` | session start (Cursor's prompt hook can't add context) |
//! | copilot | `~/.copilot/hooks/memstore.json` | `~/.copilot/mcp-config.json` | session start (the same) |
//!
//! Every harness records its sessions as runs when they compact or end, as the Claude Code plugin does; its
//! session-end hook hands the work to a copy of itself, so a harness's short session-end budget never cuts it off.
//!
//! Claude Code installs through its plugin (`/plugin install memstore@memstore`) instead.
//!
//! Everything else in those files is kept: the installer adds its own entries (every hook command it writes runs
//! `memstore hook … --harness <name>`, which is how it finds them again) and backs a file up before its first
//! change (`<file>.memstore-backup`). Installing twice changes nothing. The hooks read `MEMSTORE_URL`,
//! `MEMSTORE_TOKEN`, `MEMSTORE_ORG`, `MEMSTORE_TEAM` and `MEMSTORE_SCOPE` when they run; `--url` writes the URL
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
        .unwrap_or_else(|| "memstore".into())
}

/// The hook command for `event`: [`exe`], every part shell-quoted.
fn command(harness: Harness, event: &str, url: Option<&str>) -> String {
    let q = |s: &str| shlex::try_quote(s).map_or_else(|_| "memstore".into(), |q| q.into_owned());
    let mut c = format!("{} hook {event} --harness {}", q(&exe()), harness.name());
    if let Some(u) = url {
        c = format!("MEMSTORE_URL={} {c}", q(u));
    }
    c
}

/// Whether a hook command is one this installer wrote: `[MEMSTORE_URL=… ]…/memstore hook <event> --harness
/// <name>`, exactly. A user's own hook never matches, whatever it mentions.
fn ours_command(cmd: &str) -> bool {
    let Some(mut words) = shlex::split(cmd) else {
        return false;
    };
    if words
        .first()
        .is_some_and(|w| w.starts_with("MEMSTORE_URL="))
    {
        words.remove(0);
    }
    let events = ["session-start", "prompt", "stop", "session-end", "end"];
    words.len() == 5
        && Path::new(&words[0])
            .file_name()
            .is_some_and(|n| n == "memstore")
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
    let backup = PathBuf::from(format!("{}.memstore-backup", path.display()));
    if let Some(b) = &before
        && !backup.exists()
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
    let tmp = PathBuf::from(format!("{}.memstore-tmp", path.display()));
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
fn hooks_for(harness: Harness, url: Option<&str>) -> Vec<(&'static str, Value)> {
    let c = |event: &str| command(harness, event, url);
    match harness {
        // Claude-shaped: event → [{matcher?, hooks: [{type, command, timeout}]}]; timeouts in seconds.
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
    prune(doc, "hooks");
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

/// The server's MCP endpoint, naming the org and team (`MEMSTORE_ORG`, `MEMSTORE_TEAM`) and scope
/// (`MEMSTORE_SCOPE`) when set, so the org's own servers answer it.
fn mcp_url(url: Option<&str>) -> String {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let base = url
        .map(String::from)
        .or_else(|| var("MEMSTORE_URL"))
        .unwrap_or_else(|| "http://127.0.0.1:8080".into());
    let base = base.trim_end_matches('/');
    let query: Vec<String> = [
        ("org", "MEMSTORE_ORG"),
        ("team", "MEMSTORE_TEAM"),
        ("scope", "MEMSTORE_SCOPE"),
    ]
    .into_iter()
    .filter_map(|(param, k)| var(k).map(|v| format!("{param}={v}")))
    .collect();
    match query.is_empty() {
        true => format!("{base}/mcp"),
        false => format!("{base}/mcp?{}", query.join("&")),
    }
}

/// How the agent reaches the memory's MCP server.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mcp {
    /// Over HTTP, at `--url` (else `MEMSTORE_URL`), with `MEMSTORE_TOKEN` where the harness can't read it from
    /// the environment.
    Http,
    /// `memstore mcp`, a command the agent starts: it signs in as the CLI does (`memstore join`) and renews the
    /// token itself, so no URL or token is written anywhere. What `memstore setup` installs.
    SignedIn,
}

/// Install (or, with `remove`, uninstall) memstore in `harness`'s user config. Returns what changed.
pub fn apply(harness: Harness, url: Option<&str>, remove: bool, dry: bool) -> Result<Vec<Change>> {
    apply_with(harness, url, Mcp::Http, remove, dry)
}

/// [`apply`], reaching the MCP server as `mcp` says. With [`Mcp::SignedIn`] the hooks carry no URL either: they
/// read the CLI's context when they run.
pub fn apply_with(
    harness: Harness,
    url: Option<&str>,
    mcp: Mcp,
    remove: bool,
    dry: bool,
) -> Result<Vec<Change>> {
    let home = home()?;
    let signed_in = mcp == Mcp::SignedIn;
    let url = if signed_in { None } else { url };
    // The stdio entry each harness takes: the command and its arguments.
    let stdio = json!({"command": exe(), "args": ["mcp"]});
    if let Some(u) = url {
        let p = url::Url::parse(u).with_context(|| format!("--url {u} is not a URL"))?;
        if !matches!(p.scheme(), "http" | "https") || p.host_str().is_none() {
            bail!("--url is the memstore server, http(s)://host[:port], not {u}");
        }
    }
    let add = match remove {
        true => vec![],
        false => hooks_for(harness, url),
    };
    let token = std::env::var("MEMSTORE_TOKEN")
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
            "Claude Code installs memstore as a plugin: run /plugin marketplace add {MARKETPLACE_REPO}, then \
             /plugin install memstore@memstore, in Claude Code"
        ),
        Harness::Codex => {
            let path = home.join(".codex/hooks.json");
            let mut doc = read_json(&path)?;
            set_hooks(&mut doc, &add, &path)?;
            let changed = write(&path, &format!("{:#}\n", doc), dry, false)?;
            done(path, changed, &format!("hooks {verb}"));
            // MCP: a [mcp_servers.memstore] table, its token read from MEMSTORE_TOKEN by Codex itself.
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
            servers.remove("memstore");
            if !remove {
                let mut t = toml_edit::Table::new();
                if signed_in {
                    t["command"] = toml_edit::value(exe());
                    let mut args = toml_edit::Array::new();
                    args.push("mcp");
                    t["args"] = toml_edit::value(args);
                } else {
                    t["url"] = toml_edit::value(mcp_url(url));
                    t["bearer_token_env_var"] = toml_edit::value("MEMSTORE_TOKEN");
                }
                servers.insert("memstore", toml_edit::Item::Table(t));
            }
            let changed = write(&path, &toml.to_string(), dry, false)?;
            done(path, changed, &format!("MCP server {verb}"));
        }
        Harness::Gemini => {
            let path = home.join(".gemini/settings.json");
            let mut doc = read_json(&path)?;
            set_hooks(&mut doc, &add, &path)?;
            let servers = object_at(&mut doc, "mcpServers", &path)?;
            servers.remove("memstore");
            if !remove {
                let mut entry = match signed_in {
                    true => stdio.clone(),
                    false => json!({"httpUrl": mcp_url(url)}),
                };
                if let Some(t) = &token {
                    entry["headers"] = json!({"Authorization": format!("Bearer {t}")});
                }
                servers.insert("memstore".into(), entry);
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
            servers.remove("memstore");
            if !remove {
                servers.insert(
                    "memstore".into(),
                    match signed_in {
                        true => stdio.clone(),
                        false => json!({"url": mcp_url(url),
                                        "headers": {"Authorization": "Bearer ${env:MEMSTORE_TOKEN}"}}),
                    },
                );
            }
            prune(&mut doc, "mcpServers");
            let changed = write(&path, &format!("{:#}\n", doc), dry, false)?;
            done(path, changed, &format!("MCP server {verb}"));
        }
        Harness::Copilot => {
            // Its own file, so nothing else of the user's is touched.
            let path = home.join(".copilot/hooks/memstore.json");
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
            servers.remove("memstore");
            if !remove {
                let mut entry = match signed_in {
                    true => {
                        json!({"type": "local", "command": exe(), "args": ["mcp"], "tools": ["*"]})
                    }
                    false => json!({"type": "http", "url": mcp_url(url), "tools": ["*"]}),
                };
                if let Some(t) = &token {
                    entry["headers"] = json!({"Authorization": format!("Bearer {t}")});
                }
                servers.insert("memstore".into(), entry);
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

/// Whether installing for `harness` writes `MEMSTORE_TOKEN` itself into a file (its MCP config can't read it
/// from the environment).
pub fn writes_token(harness: Harness) -> bool {
    matches!(harness, Harness::Gemini | Harness::Copilot)
}

/// Where Claude Code finds the memstore plugin: the public repository each release publishes it to.
const MARKETPLACE: &str = "memstore";
const MARKETPLACE_REPO: &str = "A79-ai/ampup-memory";
const PLUGIN: &str = "memstore@memstore";

/// Declares the memstore plugin in Claude Code's user settings (`~/.claude/settings.json`): its marketplace in
/// `extraKnownMarketplaces` and the plugin in `enabledPlugins`. At its next start Claude Code clones the
/// marketplace (a public repository) and installs the plugin, with no prompt. With `remove`, takes both out.
/// Everything else in the file is kept, and it's backed up first.
pub fn claude_code(remove: bool, dry: bool) -> Result<Vec<Change>> {
    let path = home()?.join(".claude/settings.json");
    let mut doc = read_json(&path)?;
    let markets = object_at(&mut doc, "extraKnownMarketplaces", &path)?;
    markets.remove(MARKETPLACE);
    if !remove {
        markets.insert(
            MARKETPLACE.into(),
            json!({"source": {"source": "github", "repo": MARKETPLACE_REPO}}),
        );
    }
    prune(&mut doc, "extraKnownMarketplaces");
    let plugins = object_at(&mut doc, "enabledPlugins", &path)?;
    plugins.remove(PLUGIN);
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
    [
        (Harness::ClaudeCode, ".claude"),
        (Harness::Codex, ".codex"),
        (Harness::Gemini, ".gemini"),
        (Harness::Cursor, ".cursor"),
        (Harness::Copilot, ".copilot"),
    ]
    .into_iter()
    .filter(|(h, dir)| home.join(dir).is_dir() || (*h == Harness::ClaudeCode && on_path("claude")))
    .map(|(h, _)| h)
    .collect()
}
