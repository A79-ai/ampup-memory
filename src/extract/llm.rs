//! One switch for every model the engine asks, so a laptop runs on one local model (or one key) without
//! naming it for each role. `AMPUP_MEMORY_LLM` picks the backend for every role (extracting, compiling, learning
//! advice, judging, routing, dreaming, summarising) whose own variable (`AMPUP_MEMORY_EXTRACT`,
//! `AMPUP_MEMORY_ADVICE`, ...) names none; a role's own variables always win.
//!
//! - `AMPUP_MEMORY_LLM`: `ollama` (a local Ollama, through its own chat API), any backend `AMPUP_MEMORY_EXTRACT`
//!   takes (`openai` for any OpenAI-compatible server: LM Studio, llama.cpp's server, vLLM), or `off`.
//! - `AMPUP_MEMORY_LLM_MODEL`: one model for every role, the strong ones too (default for `ollama`:
//!   [`OLLAMA_MODEL`]; for another backend, its own defaults per role).
//! - `AMPUP_MEMORY_LLM_URL`: the server's base URL (default for `ollama`: [`OLLAMA_URL`]). Set alone, it means
//!   `openai` at that URL.
//! - `AMPUP_MEMORY_LLM_KEY`: the key it is sent (none by default for `ollama` and a URL of its own).
//! - `AMPUP_MEMORY_LLM_CONTEXT`: the context Ollama is asked to load the model with, in tokens (default
//!   [`OLLAMA_CONTEXT`]).
//!
//! Ollama is asked through its own `/api/chat`, not its OpenAI-compatible `/v1`, because only its own API takes
//! the context size per request (`options.num_ctx`): `/v1` ignores it, and the server's default (4,096 tokens on
//! a machine with little memory) silently cuts the front off a prompt of up to 60,000 characters, the
//! instructions included, with no error to see.
//!
//! `serve --local` keeps a choice made at its first run ([`first_run`]) in `llm.json` in its directory: read as
//! these variables are, under them (a variable set wins), and only by that server.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Where Ollama answers unless `AMPUP_MEMORY_LLM_URL` says otherwise.
pub const OLLAMA_URL: &str = "http://localhost:11434";
/// The model asked through Ollama unless `AMPUP_MEMORY_LLM_MODEL` (or a role's `_MODEL`) names another: small
/// (a 1.9 GB download), quick on a laptop, with a 32k-token context and JSON it keeps to. A larger one
/// (`qwen2.5:7b`, `qwen2.5:14b`) learns better advice where the machine has the memory.
pub const OLLAMA_MODEL: &str = "qwen2.5:3b";
/// The context Ollama loads the model with: a prompt of 60,000 characters is about 15,000 tokens, with room
/// for the reply.
pub const OLLAMA_CONTEXT: u32 = 32_768;

/// The switch: a backend, and what it is reached with.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Llm {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// What `serve --local` read from its `llm.json` (or was told at its first run), for this process.
static SAVED: OnceLock<Llm> = OnceLock::new();

fn var(k: &str) -> Option<String> {
    std::env::var(k)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The switch the environment sets, else the one `serve --local` saved, with `AMPUP_MEMORY_LLM_MODEL` and
/// `_KEY` over the saved ones; None where neither sets one.
pub fn configured() -> Option<Llm> {
    let provider =
        var("AMPUP_MEMORY_LLM").or_else(|| var("AMPUP_MEMORY_LLM_URL").map(|_| "openai".into()));
    let (provider, base) = match provider {
        Some(p) => (p, Llm::default()),
        // "No learning", chosen while there was no key: an OpenRouter key set since turns the default on.
        None => {
            let saved = SAVED.get().cloned()?;
            if saved.provider == "off" && var("OPENROUTER_API_KEY").is_some() {
                return None;
            }
            (saved.provider.clone(), saved)
        }
    };
    Some(Llm {
        provider: provider.to_ascii_lowercase(),
        url: var("AMPUP_MEMORY_LLM_URL").or(base.url),
        model: var("AMPUP_MEMORY_LLM_MODEL").or(base.model),
        key: var("AMPUP_MEMORY_LLM_KEY").or(base.key),
    })
}

/// The context Ollama is asked to load the model with (`AMPUP_MEMORY_LLM_CONTEXT`).
pub fn ollama_context() -> u32 {
    var("AMPUP_MEMORY_LLM_CONTEXT")
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(OLLAMA_CONTEXT)
}

/// An Ollama base URL as given (`http://host:11434`, with or without `/v1` or `/api`), as its own API's base.
pub fn ollama_base(url: &str) -> String {
    let u = url.trim_end_matches('/');
    u.strip_suffix("/v1")
        .or_else(|| u.strip_suffix("/api"))
        .unwrap_or(u)
        .to_string()
}

/// `dir/llm.json`: what `serve --local` chose at its first run.
fn file(dir: &Path) -> PathBuf {
    dir.join("llm.json")
}

/// What `dir/llm.json` holds, if anything.
fn read(dir: &Path) -> Result<Option<Llm>> {
    let f = file(dir);
    match std::fs::read_to_string(&f) {
        Ok(s) => Ok(Some(
            serde_json::from_str(&s).with_context(|| format!("{}", f.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::from(e).context(format!("{}", f.display()))),
    }
}

/// Writes `dir/llm.json`, readable by its owner alone (it may hold a key).
fn write(dir: &Path, llm: &Llm) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let f = file(dir);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut w = o.open(&f).with_context(|| format!("{}", f.display()))?;
    // The mode above is only a new file's: one there already is made the owner's alone too.
    #[cfg(unix)]
    w.set_permissions(
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    )?;
    std::io::Write::write_all(&mut w, serde_json::to_string_pretty(llm)?.as_bytes())?;
    Ok(())
}

/// What `dir/llm.json` holds, if anything (for `service status`: never show its key).
pub fn saved(dir: &Path) -> Result<Option<Llm>> {
    read(dir)
}

/// A vendor's own key variable, for the backends that have one.
fn vendor_key(provider: &str) -> Option<&'static str> {
    match provider {
        "openrouter" | "openrouter-batch" => Some("OPENROUTER_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        _ => None,
    }
}

/// For a server that runs without this environment (`ampup-memory service install`): keeps the model this
/// environment names in `dir/llm.json`, where that server reads it, key included (the file is the owner's
/// alone). `AMPUP_MEMORY_LLM` (or `_URL`) wins, with `_MODEL` and `_KEY` (else the vendor's key variable);
/// else a vendor's key (`OPENROUTER_API_KEY`, then `ANTHROPIC_API_KEY`, then `OPENAI_API_KEY`) is kept where
/// nothing is saved, no model was chosen, or the same vendor was (its key refreshed). A different backend chosen
/// before (a local Ollama) is kept: `AMPUP_MEMORY_LLM` switches it. Says what it did, never the key; None where
/// the environment names no model.
pub fn adopt_env(dir: &Path) -> Result<Option<String>> {
    let saved = read(dir)?;
    let provider =
        var("AMPUP_MEMORY_LLM").or_else(|| var("AMPUP_MEMORY_LLM_URL").map(|_| "openai".into()));
    let want = match provider {
        Some(p) => {
            let p = p.to_ascii_lowercase();
            let key = var("AMPUP_MEMORY_LLM_KEY").or_else(|| vendor_key(&p).and_then(var));
            Llm {
                url: var("AMPUP_MEMORY_LLM_URL"),
                model: var("AMPUP_MEMORY_LLM_MODEL"),
                key: if p == "off" { None } else { key },
                provider: p,
            }
        }
        None => {
            let vendors = ["openrouter", "anthropic", "openai"];
            let has = |p: &str| vendor_key(p).and_then(var);
            let chosen = saved.as_ref().map(|s| s.provider.as_str());
            match chosen {
                // The vendor chosen before: its key refreshed, its model and URL kept.
                Some(p) if vendors.contains(&p) && has(p).is_some() => Llm {
                    key: has(p),
                    ..saved.clone().unwrap_or_default()
                },
                None | Some("off") => match vendors.iter().find(|p| has(p).is_some()) {
                    Some(p) => Llm {
                        provider: p.to_string(),
                        key: has(p),
                        model: var("AMPUP_MEMORY_LLM_MODEL"),
                        url: None,
                    },
                    None => return Ok(None),
                },
                Some(p) => {
                    return Ok(vendors.iter().find(|v| has(v).is_some()).map(|v| {
                        format!(
                            "model: keeping {p}, chosen before ({}), not {} from {}: set AMPUP_MEMORY_LLM={v} to switch",
                            file(dir).display(),
                            v,
                            vendor_key(v).unwrap_or_default()
                        )
                    }));
                }
            }
        }
    };
    let what = describe(&want);
    if saved.as_ref() == Some(&want) {
        return Ok(Some(format!(
            "model: {what}, as saved in {}",
            file(dir).display()
        )));
    }
    write(dir, &want)?;
    Ok(Some(format!(
        "model: {what}, saved in {} (readable by you alone)",
        file(dir).display()
    )))
}

/// A saved choice in words, without its key.
pub fn describe(l: &Llm) -> String {
    if l.provider == "off" {
        return "none (memory is kept and searched, nothing is extracted)".into();
    }
    let mut s = l.provider.clone();
    if let Some(m) = &l.model {
        s += &format!(" {m}");
    }
    if let Some(u) = &l.url {
        s += &format!(" at {u}");
    }
    if l.key.is_some() {
        s += " with a key";
    }
    s
}

/// The models a local Ollama has pulled, or None where it doesn't answer at `base`.
async fn ollama_models(base: &str) -> Option<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok()?;
    let v: serde_json::Value = client
        .get(format!("{}/api/tags", ollama_base(base)))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    Some(
        v["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["name"].as_str().map(String::from))
            .collect(),
    )
}

/// How to turn learning on, for a server that has no model and asks no one.
pub const HOW: &str = "to learn from what it keeps, set OPENROUTER_API_KEY (or AMPUP_MEMORY_LLM=openai with \
OPENAI_API_KEY, or anthropic with ANTHROPIC_API_KEY), or run Ollama and set AMPUP_MEMORY_LLM=ollama (model \
qwen2.5:3b unless AMPUP_MEMORY_LLM_MODEL names another); or run `ampup-memory serve --local` (or, for the background \
service, `ampup-memory service install`) in a terminal to choose";

/// `serve --local`'s first run: reads the choice `dir/llm.json` keeps, and where there is none and nothing else
/// names a model, asks at the terminal: a key to paste, the local Ollama, or no learning. The answer is kept in
/// `dir/llm.json` so it isn't asked again (the variables still win). With no terminal, nothing is asked: the
/// server runs as before, and says how to turn learning on. A key is never shown or logged.
pub async fn first_run(dir: &Path) -> Result<()> {
    let saved = read(dir)?;
    let decided = saved.is_some()
        || var("AMPUP_MEMORY_EXTRACT").is_some()
        || super::Extractor::from_env()?.is_some();
    if let Some(s) = saved {
        let _ = SAVED.set(s);
    }
    if decided {
        check_ollama().await;
        return Ok(());
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        eprintln!(
            "ampup-memory: no model: memory is kept and searched, nothing is extracted; {HOW}"
        );
        return Ok(());
    }
    let base = var("AMPUP_MEMORY_LLM_URL").unwrap_or_else(|| OLLAMA_URL.into());
    let ollama = ollama_models(&base).await;
    eprintln!(
        "\nAmpUp Memory learns facts and advice from what it keeps with a language model, and none is set up.\n  \
         1. Paste an API key (OpenRouter, OpenAI or Anthropic)\n  2. {}\n  3. Continue without learning: memory \
         is kept and searched by keyword and meaning, nothing is extracted",
        match &ollama {
            Some(m) if m.is_empty() => format!(
                "Use Ollama on this machine (it answers at {base} but has no model: `ollama pull {OLLAMA_MODEL}`)"
            ),
            Some(m) => format!("Use Ollama on this machine ({})", m.join(", ")),
            None => format!(
                "Use Ollama on this machine (not answering at {base}: install it from ollama.com, run \
                 `ollama pull {OLLAMA_MODEL}`)"
            ),
        }
    );
    let choice = ask("Choose 1, 2 or 3 [3]: ")?;
    let llm = match choice.as_deref().map(str::trim) {
        Some("1") => {
            let key = ask_hidden("Key (not shown): ")?.trim().to_string();
            let provider = match key.as_str() {
                "" => None,
                k if k.starts_with("sk-or-") => Some("openrouter".to_string()),
                k if k.starts_with("sk-ant-") => Some("anthropic".to_string()),
                k if k.starts_with("sk-") => Some("openai".to_string()),
                _ => ask("Whose key is it: openrouter, openai or anthropic? ")?
                    .map(|a| a.trim().to_ascii_lowercase())
                    .filter(|a| ["openrouter", "openai", "anthropic"].contains(&a.as_str())),
            };
            let Some(provider) = provider else {
                eprintln!("No key saved: continuing without learning (asked again next time)");
                return Ok(());
            };
            Llm {
                provider,
                key: Some(key),
                ..Default::default()
            }
        }
        Some("2") => {
            let models = ollama.clone().unwrap_or_default();
            let model = match models.len() {
                0 => OLLAMA_MODEL.to_string(),
                1 => models[0].clone(),
                _ => {
                    let default = models
                        .iter()
                        .find(|m| m.as_str() == OLLAMA_MODEL)
                        .unwrap_or(&models[0])
                        .clone();
                    let a = ask(&format!("Which model [{default}]: "))?;
                    match a.map(|a| a.trim().to_string()).filter(|a| !a.is_empty()) {
                        Some(a) => a
                            .parse::<usize>()
                            .ok()
                            .and_then(|n| models.get(n.wrapping_sub(1)).cloned())
                            .unwrap_or(a),
                        None => default,
                    }
                }
            };
            if !models.contains(&model) {
                eprintln!(
                    "Ollama has no {model} yet: run `ollama pull {model}`; extraction waits for it meanwhile"
                );
            }
            Llm {
                provider: "ollama".into(),
                url: var("AMPUP_MEMORY_LLM_URL"),
                model: Some(model),
                key: None,
            }
        }
        _ => Llm {
            provider: "off".into(),
            ..Default::default()
        },
    };
    save(dir, llm)
}

/// Keeps `llm` in `dir/llm.json`, and this process uses it.
fn save(dir: &Path, llm: Llm) -> Result<()> {
    write(dir, &llm)?;
    eprintln!(
        "Saved in {} (delete it to be asked again; AMPUP_MEMORY_LLM and the other variables override it)",
        file(dir).display()
    );
    let _ = SAVED.set(llm);
    Ok(())
}

/// Says at start where Ollama is the backend and it doesn't answer, or hasn't the model.
async fn check_ollama() {
    let Some(l) = configured().filter(|l| l.provider == "ollama") else {
        return;
    };
    let base = l.url.clone().unwrap_or_else(|| OLLAMA_URL.into());
    let model = l.model.clone().unwrap_or_else(|| OLLAMA_MODEL.into());
    match ollama_models(&base).await {
        None => eprintln!(
            "ampup-memory: Ollama isn't answering at {base}: extraction waits until it does"
        ),
        Some(m)
            if !m
                .iter()
                .any(|n| *n == model || *n == format!("{model}:latest")) =>
        {
            eprintln!(
                "ampup-memory: Ollama at {base} has no {model}: run `ollama pull {model}`; extraction waits for it"
            )
        }
        Some(_) => eprintln!(
            "ampup-memory: every model call goes to Ollama at {base} ({model}, context {} tokens)",
            ollama_context()
        ),
    }
}

/// A line from the terminal, or None at its end.
fn ask(prompt: &str) -> Result<Option<String>> {
    use std::io::Write;
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
}

/// A line from the terminal that it doesn't echo (a key).
fn ask_hidden(prompt: &str) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: termios is plain data, filled by tcgetattr before it is read.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd is stdin's, a terminal (checked by the caller); t is a valid termios.
        if unsafe { libc::tcgetattr(fd, &mut t) } == 0 {
            let echo = t;
            t.c_lflag &= !libc::ECHO;
            // SAFETY: as above; the terminal's settings are restored below whatever the read returns.
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) };
            let line = ask(prompt);
            // SAFETY: restores what tcgetattr read.
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &echo) };
            eprintln!();
            return Ok(line?.unwrap_or_default());
        }
    }
    Ok(ask(prompt)?.unwrap_or_default())
}
