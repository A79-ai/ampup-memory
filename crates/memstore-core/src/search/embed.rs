//! Text embeddings behind one interface: [`Provider`] names the backend, [`embed`] turns texts
//! into unit-length vectors, [`dims`] is their width, and `layout_tag` keeps each backend's
//! vectors in their own index. The backend is read once per process from the environment.
//!
//! Backends:
//! - `local`: a fastembed model run in-process (default `Xenova/bge-small-en-v1.5`, 384 dims).
//!   Any model fastembed ships can be named; its files are fetched on first use into
//!   `MEMSTORE_MODEL_DIR` (default `~/.cache/memstore/models`).
//! - `openai`: OpenAI's embeddings API (default `text-embedding-3-small`, 1,536 dims), or any
//!   server with its `/embeddings` shape.
//! - `openrouter`: OpenRouter's embeddings API, where one key reaches every vendor's embedding
//!   models (`openai/text-embedding-3-small`, `qwen/qwen3-embedding-8b`,
//!   `google/gemini-embedding-001`, …). OpenRouter does the vendor routing, so the engine
//!   carries no vendor adapters of its own.
//!
//! Environment:
//! - `MEMSTORE_EMBED`: `local`, `openai` or `openrouter`. Unset: `openai` when an embeddings key
//!   of its own is set (`MEMSTORE_EMBED_KEY` or `MEMSTORE_EMBED_KEY_FILE`), else `openrouter` when
//!   `OPENROUTER_API_KEY` is (the default, as for extraction: the local model is slow to index
//!   with on a few CPUs), else `openai` with `OPENAI_API_KEY`, else `local`. A deployment's
//!   control plane can name it (`crate::agent`'s `embedding`). `cargo test` and `cargo run` pin
//!   `local` (`.cargo/config.toml`), so the test suite never calls an API, whatever keys the
//!   machine carries.
//! - `MEMSTORE_EMBED_MODEL`: the model name (a fastembed model code, an OpenAI model id, or an
//!   OpenRouter model id with its vendor).
//! - `MEMSTORE_EMBED_DIMS`: vector width, for API models that accept one. Required for an API
//!   model the engine doesn't know; the local model's width comes from the model.
//! - `MEMSTORE_EMBED_URL`: the API base URL (default `https://api.openai.com/v1` for `openai`,
//!   `https://openrouter.ai/api/v1` for `openrouter`; a proxy or any server with the same
//!   `/embeddings` shape works). `MEMSTORE_EMBED_KEY`: the key (or `MEMSTORE_EMBED_KEY_FILE`,
//!   a file holding it, for mounted secrets), else `OPENAI_API_KEY` or `OPENROUTER_API_KEY`.
//!
//! Vectors from different backends, or models, are not comparable, so a non-default choice
//! gets its own index layout and the next `memstore index` builds the facts and passage
//! indexes there from scratch.

use std::sync::LazyLock;

use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt};

pub const DEFAULT_LOCAL_MODEL: &str = "Xenova/bge-small-en-v1.5";
pub const DEFAULT_OPENAI_MODEL: &str = "text-embedding-3-small";
pub const DEFAULT_OPENAI_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_OPENROUTER_MODEL: &str = "openai/text-embedding-3-small";
pub const DEFAULT_OPENROUTER_URL: &str = "https://openrouter.ai/api/v1";
/// OpenRouter's attribution headers.
const OPENROUTER_REFERER: &str = "https://github.com/A79-ai/ampup-memory";
const OPENROUTER_TITLE: &str = "memstore";
/// A request holds at most this many inputs and this many characters in all (the API's
/// limits are 2,048 inputs and about 300k tokens; characters stand in for tokens).
pub const MAX_INPUTS: usize = 2048;
pub const MAX_CHARS: usize = 300_000;
/// Requests in flight at once during an index build.
const API_CONCURRENCY: usize = 4;
const API_ATTEMPTS: u32 = 5;
/// One input is split into pieces of at most this many bytes (about 6k tokens, under the
/// 8,191-token limit of the OpenAI models), and the pieces' vectors are averaged.
pub const MAX_INPUT_BYTES: usize = 24_000;
/// The smallest pieces an input is cut into when the API says a piece is too long ([`embed_api`]).
const MIN_INPUT_BYTES: usize = 3_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Provider {
    /// A fastembed model run in-process. `code` is its name as fastembed lists it.
    Local {
        model: fastembed::EmbeddingModel,
        code: String,
        dims: i32,
    },
    /// OpenAI's `/embeddings`, or a server with its shape. The key is read from the environment
    /// at call time (`MEMSTORE_EMBED_KEY`, else `OPENAI_API_KEY`), never held here.
    OpenAI {
        model: String,
        dims: i32,
        base_url: String,
    },
    /// OpenRouter's `/embeddings`. The key is read from the environment at call time
    /// (`MEMSTORE_EMBED_KEY`, else `OPENROUTER_API_KEY`), never held here.
    OpenRouter {
        model: String,
        dims: i32,
        base_url: String,
    },
}

impl Provider {
    /// The provider named by environment-like pairs (see the module doc).
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Provider> {
        let var = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let own_key =
            var("MEMSTORE_EMBED_KEY").is_some() || var("MEMSTORE_EMBED_KEY_FILE").is_some();
        let choice = match var("MEMSTORE_EMBED").map(|c| c.to_ascii_lowercase()) {
            Some(c) => c,
            None if own_key => "openai".into(),
            None if var("OPENROUTER_API_KEY").is_some() => "openrouter".into(),
            None if var("OPENAI_API_KEY").is_some() => "openai".into(),
            None => "local".into(),
        };
        let model = var("MEMSTORE_EMBED_MODEL");
        let dims = match var("MEMSTORE_EMBED_DIMS") {
            Some(d) => Some(d.parse::<i32>().ok().filter(|d| *d > 0).with_context(|| {
                format!("MEMSTORE_EMBED_DIMS must be a positive integer, not {d:?}")
            })?),
            None => None,
        };
        match choice.as_str() {
            "local" => {
                let code = model.unwrap_or_else(|| DEFAULT_LOCAL_MODEL.into());
                let info = local_model(&code)?;
                anyhow::ensure!(
                    dims.is_none_or(|d| d as usize == info.dim),
                    "MEMSTORE_EMBED_DIMS does not apply to the local model {code} ({} dims)",
                    info.dim
                );
                Ok(Provider::Local {
                    model: info.model.clone(),
                    code: info.model_code.clone(),
                    dims: info.dim as i32,
                })
            }
            api @ ("openai" | "openrouter") => {
                let (key_var, default_model, default_url) = if api == "openai" {
                    ("OPENAI_API_KEY", DEFAULT_OPENAI_MODEL, DEFAULT_OPENAI_URL)
                } else {
                    (
                        "OPENROUTER_API_KEY",
                        DEFAULT_OPENROUTER_MODEL,
                        DEFAULT_OPENROUTER_URL,
                    )
                };
                anyhow::ensure!(
                    own_key || var(key_var).is_some(),
                    "MEMSTORE_EMBED={api} needs MEMSTORE_EMBED_KEY, MEMSTORE_EMBED_KEY_FILE or {key_var}"
                );
                let model = model.unwrap_or_else(|| default_model.into());
                let dims = dims.or_else(|| known_dims(&model)).with_context(|| {
                    format!("set MEMSTORE_EMBED_DIMS: the engine doesn't know the width of {model}")
                })?;
                let base_url = var("MEMSTORE_EMBED_URL")
                    .map(|u| u.trim_end_matches('/').to_string())
                    .unwrap_or_else(|| default_url.into());
                Ok(if api == "openai" {
                    Provider::OpenAI {
                        model,
                        dims,
                        base_url,
                    }
                } else {
                    Provider::OpenRouter {
                        model,
                        dims,
                        base_url,
                    }
                })
            }
            other => {
                anyhow::bail!("MEMSTORE_EMBED must be local, openai or openrouter, not {other:?}")
            }
        }
    }

    pub fn dims(&self) -> i32 {
        match self {
            Provider::Local { dims, .. }
            | Provider::OpenAI { dims, .. }
            | Provider::OpenRouter { dims, .. } => *dims,
        }
    }

    /// Appended to the index layout name, so each backend and model's vectors live in their
    /// own index. Empty for the default local model, which keeps existing indexes valid.
    pub fn layout_tag(&self) -> String {
        let (kind, model, dims) = match self {
            Provider::Local { code, dims, .. } if code == DEFAULT_LOCAL_MODEL => {
                return String::new();
            }
            Provider::Local { code, dims, .. } => ("local", code, dims),
            Provider::OpenAI { model, dims, .. } => ("openai", model, dims),
            Provider::OpenRouter { model, dims, .. } => ("openrouter", model, dims),
        };
        let model: String = model
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        format!("-{kind}-{model}-{dims}")
    }

    /// How the provider shows in logs and `memstore embed`.
    pub fn describe(&self) -> String {
        match self {
            Provider::Local { code, dims, .. } => format!("local {code} ({dims} dims)"),
            Provider::OpenAI {
                model,
                dims,
                base_url,
            } => format!("openai {model} ({dims} dims) at {base_url}"),
            Provider::OpenRouter {
                model,
                dims,
                base_url,
            } => format!("openrouter {model} ({dims} dims) at {base_url}"),
        }
    }
}

/// The fastembed model named `code`: its listed model code (`Xenova/bge-small-en-v1.5`), that
/// code without its organisation (`bge-small-en-v1.5`), or the enum's own name
/// (`BGESmallENV15Q`), case-insensitively. The first listing wins where codes repeat.
fn local_model(code: &str) -> Result<fastembed::ModelInfo<fastembed::EmbeddingModel>> {
    let models = fastembed::TextEmbedding::list_supported_models();
    models
        .iter()
        .find(|m| {
            m.model_code.eq_ignore_ascii_case(code)
                || m.model_code
                    .rsplit('/')
                    .next()
                    .is_some_and(|c| c.eq_ignore_ascii_case(code))
                || format!("{:?}", m.model).eq_ignore_ascii_case(code)
        })
        .cloned()
        .with_context(|| {
            let codes: Vec<&str> = models.iter().map(|m| m.model_code.as_str()).collect();
            format!(
                "unknown local embedding model {code:?}; fastembed ships: {}",
                codes.join(", ")
            )
        })
}

/// The model's name without its vendor (`openai/text-embedding-3-small`).
fn base_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// The documented width of the API models the engine knows.
fn known_dims(model: &str) -> Option<i32> {
    Some(match base_model(model) {
        "text-embedding-3-small" | "text-embedding-ada-002" => 1536,
        "text-embedding-3-large" | "gemini-embedding-001" => 3072,
        "qwen3-embedding-8b" => 4096,
        "qwen3-embedding-4b" => 2560,
        "qwen3-embedding-0.6b" | "mistral-embed" => 1024,
        _ => return None,
    })
}

static PROVIDER: LazyLock<Provider> = LazyLock::new(|| {
    Provider::from_env(|k| std::env::var(k).ok()).unwrap_or_else(|e| {
        eprintln!("embeddings: {e:#}; using the default local model");
        Provider::from_env(|_| None).expect("the default local model is always listed")
    })
});

/// The process's embedding provider, read from the environment on first use.
pub fn provider() -> &'static Provider {
    &PROVIDER
}

/// Embedding width of the process's provider.
pub fn dims() -> i32 {
    provider().dims()
}

/// Unit-length embeddings of `texts`, in order, from the process's provider.
pub async fn embed(texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(vec![]);
    }
    match provider() {
        p @ Provider::Local { .. } => embed_local(p, texts).await,
        p @ (Provider::OpenAI { .. } | Provider::OpenRouter { .. }) => {
            embed_api(p, &api_key(p)?, texts).await
        }
    }
}

/// The API key: `MEMSTORE_EMBED_KEY`, the contents of the file `MEMSTORE_EMBED_KEY_FILE` names
/// (mounted secrets), or the provider's own variable (`OPENAI_API_KEY`, `OPENROUTER_API_KEY`).
fn api_key(provider: &Provider) -> Result<String> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if let Some(k) = var("MEMSTORE_EMBED_KEY") {
        return Ok(k.trim().to_string());
    }
    if let Some(f) = var("MEMSTORE_EMBED_KEY_FILE") {
        return Ok(std::fs::read_to_string(&f)
            .with_context(|| format!("MEMSTORE_EMBED_KEY_FILE {f}"))?
            .trim()
            .to_string());
    }
    let own = match provider {
        Provider::OpenAI { .. } => "OPENAI_API_KEY",
        _ => "OPENROUTER_API_KEY",
    };
    var(own)
        .map(|k| k.trim().to_string())
        .with_context(|| format!("MEMSTORE_EMBED_KEY, MEMSTORE_EMBED_KEY_FILE or {own}"))
}

/// The local model, loaded once per process, for index updates' batches.
static EMBEDDER: LazyLock<parking_lot::Mutex<Option<fastembed::TextEmbedding>>> =
    LazyLock::new(Default::default);
/// A second copy for searches' own few texts (a query): an index update embedding thousands of passages
/// holds the first for a slice at a time, and a query waiting behind each slice took ~0.8 s.
static QUERY_EMBEDDER: LazyLock<parking_lot::Mutex<Option<fastembed::TextEmbedding>>> =
    LazyLock::new(Default::default);
/// At most this many texts embed on [`QUERY_EMBEDDER`].
const QUERY_TEXTS: usize = 4;

/// Texts embedded per hold of the local model. Index updates embed thousands at a time; holding the
/// model for one slice and handing it on fairly means a search embedding its query waits for at most
/// one slice, not for the whole batch.
const EMBED_SLICE: usize = 32;

async fn embed_local(provider: &Provider, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
    let Provider::Local { model, .. } = provider else {
        anyhow::bail!("not a local provider");
    };
    let model = model.clone();
    let which = if texts.len() <= QUERY_TEXTS {
        &QUERY_EMBEDDER
    } else {
        &EMBEDDER
    };
    // The batch runs on a blocking thread, which nothing can stop from outside: dropping this future (an index
    // update given up, or the runtime shutting down on SIGTERM) only stops waiting for it, and the runtime waits
    // for blocking threads before the process can exit. So the thread looks between slices whether anyone still
    // wants the vectors, and stops if not: a server on a fresh corpus, embedding thousands of passages, exits on
    // SIGTERM within a slice instead of after the whole batch.
    let wanted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let _unwanted_when_dropped = Unwanted(wanted.clone());
    tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>> {
        let mut v = Vec::with_capacity(texts.len());
        for slice in texts.chunks(EMBED_SLICE) {
            if !wanted.load(std::sync::atomic::Ordering::Relaxed) {
                anyhow::bail!("embedding stopped: no one is waiting for it");
            }
            let mut m = which.lock();
            if m.is_none() {
                let dir = std::env::var("MEMSTORE_MODEL_DIR").unwrap_or_else(|_| {
                    format!(
                        "{}/.cache/memstore/models",
                        std::env::var("HOME").unwrap_or_else(|_| ".".into())
                    )
                });
                *m = Some(fastembed::TextEmbedding::try_new(
                    fastembed::TextInitOptions::new(model.clone())
                        .with_cache_dir(dir.into())
                        .with_show_download_progress(false),
                )?);
            }
            v.extend(m.as_mut().unwrap().embed(slice, Some(EMBED_SLICE))?);
            // To whoever is waiting (a search's query, say), not back to this batch's next slice.
            parking_lot::MutexGuard::unlock_fair(m);
        }
        v.iter_mut().for_each(|e| normalize(e));
        Ok(v)
    })
    .await?
}

/// Clears its flag when dropped: the future that waits for a blocking batch holds one.
struct Unwanted(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for Unwanted {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

fn normalize(e: &mut [f32]) {
    let n = e.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    e.iter_mut().for_each(|x| *x /= n);
}

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

/// `text` as API inputs: never empty (the API refuses that), and in pieces of at most
/// `max_bytes` when it is longer than one input may be. The pieces' vectors are averaged back
/// into one (`fold_pieces`), as AmpUp's embedding client does.
fn pieces(text: &str, max_bytes: usize) -> Vec<&str> {
    if text.trim().is_empty() {
        return vec![" "];
    }
    let mut out = vec![];
    let mut rest = text;
    while !rest.is_empty() {
        let cut = floor_char(rest, max_bytes);
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out
}

/// One vector per text from the vectors of its pieces: the mean of the pieces, unit length.
fn fold_pieces(counts: &[usize], mut vecs: Vec<Vec<f32>>) -> Vec<Vec<f32>> {
    let mut out = Vec::with_capacity(counts.len());
    let mut pieces = vecs.drain(..);
    for &n in counts {
        let mut acc = pieces.next().expect("a vector per piece");
        for _ in 1..n {
            let v = pieces.next().expect("a vector per piece");
            acc.iter_mut().zip(&v).for_each(|(a, b)| *a += b);
        }
        normalize(&mut acc);
        out.push(acc);
    }
    out
}

/// Inputs grouped into requests of at most `max_items` inputs and `max_chars` characters.
fn budget_batches(inputs: Vec<String>, max_items: usize, max_chars: usize) -> Vec<Vec<String>> {
    let mut batches: Vec<Vec<String>> = vec![];
    let (mut batch, mut chars) = (vec![], 0);
    for input in inputs {
        if !batch.is_empty() && (batch.len() >= max_items || chars + input.len() >= max_chars) {
            batches.push(std::mem::take(&mut batch));
            chars = 0;
        }
        chars += input.len();
        batch.push(input);
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

/// One POST of `body` to `url`, retried with backoff on rate limits, server errors and
/// transport failures. Returns the JSON reply of a success; any other status is an error.
/// OpenRouter's attribution headers go only to OpenRouter.
async fn post_json(
    url: &str,
    bearer: &str,
    body: &serde_json::Value,
    label: &str,
    openrouter: bool,
) -> Result<serde_json::Value> {
    let mut last = None;
    for attempt in 0..API_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500 << attempt)).await;
        }
        let mut req = HTTP.post(url).bearer_auth(bearer).json(body);
        if openrouter {
            req = req
                .header("HTTP-Referer", OPENROUTER_REFERER)
                .header("X-Title", OPENROUTER_TITLE);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                last = Some(anyhow::Error::from(e).context(format!("{label}: request")));
                continue;
            }
        };
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
        // A reply holds a vector per input, megabytes of JSON for a full request: a body cut off or
        // unreadable on the way is a transport failure, retried like one, not an empty answer.
        let body = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                last = Some(anyhow::Error::from(e).context(format!("{label}: reading the reply")));
                continue;
            }
        };
        let parsed = serde_json::from_slice::<serde_json::Value>(&body);
        if status.is_success() {
            match parsed {
                Ok(out) => return Ok(out),
                Err(e) => {
                    last = Some(anyhow::anyhow!(
                        "{label}: an unreadable reply ({} bytes): {e}",
                        body.len()
                    ));
                    continue;
                }
            }
        }
        let out = parsed.unwrap_or_default();
        let msg = format!("{label}: HTTP {status} {}", out["error"]["message"]);
        anyhow::ensure!(status.as_u16() == 429 || status.is_server_error(), msg);
        if let Some(s) = retry_after {
            tokio::time::sleep(std::time::Duration::from_secs(s.min(60))).await;
        }
        last = Some(anyhow::anyhow!(msg));
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("{label}: gave up")))
}

fn floats(v: &serde_json::Value) -> Vec<f32> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|x| x.as_f64().unwrap_or_default() as f32)
        .collect()
}

/// `texts` through an embeddings API (OpenAI's or OpenRouter's): long texts in pieces, requests
/// within the input and character budgets, a few requests in flight, replies matched by
/// `index`, every vector checked against the layout width. A piece of [`MAX_INPUT_BYTES`] is about 6k
/// tokens of prose, but text that tokenizes densely (diffs, JSON, ids, hashes) passes the model's 8,192
/// tokens in fewer bytes: when the API refuses a piece as too long, the texts are cut into pieces half
/// as long and sent again, down to [`MIN_INPUT_BYTES`]. Texts it took whole are embedded as before.
pub async fn embed_api(
    provider: &Provider,
    key: &str,
    texts: Vec<String>,
) -> Result<Vec<Vec<f32>>> {
    let mut max_bytes = MAX_INPUT_BYTES;
    loop {
        match embed_api_in(provider, key, &texts, max_bytes).await {
            Err(e) if too_long(&e) && max_bytes / 2 >= MIN_INPUT_BYTES => {
                let longest = texts.iter().map(String::len).max().unwrap_or_default();
                eprintln!(
                    "embeddings: a piece of at most {max_bytes} bytes was too long for the model (longest text \
                     {longest} bytes): cutting into pieces of {} bytes",
                    max_bytes / 2
                );
                max_bytes /= 2;
            }
            other => return other,
        }
    }
}

/// Whether the API refused an input as longer than the model takes: OpenRouter's `maximum input length`,
/// OpenAI's `maximum context length`.
fn too_long(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}");
    m.contains("maximum input length") || m.contains("maximum context length")
}

/// [`embed_api`] with texts cut into pieces of at most `max_bytes`.
async fn embed_api_in(
    provider: &Provider,
    key: &str,
    texts: &[String],
    max_bytes: usize,
) -> Result<Vec<Vec<f32>>> {
    let (model, dims, base_url, label, openrouter) = match provider {
        Provider::OpenAI {
            model,
            dims,
            base_url,
        } => (model, dims, base_url, "OpenAI embeddings", false),
        Provider::OpenRouter {
            model,
            dims,
            base_url,
        } => (model, dims, base_url, "OpenRouter embeddings", true),
        Provider::Local { .. } => anyhow::bail!("not an API provider"),
    };
    let url = format!("{base_url}/embeddings");
    let (counts, inputs): (Vec<usize>, Vec<String>) = texts
        .iter()
        .map(|t| {
            let p = pieces(t, max_bytes);
            (p.len(), p.into_iter().map(String::from).collect::<Vec<_>>())
        })
        .fold((vec![], vec![]), |(mut c, mut i), (n, p)| {
            c.push(n);
            i.extend(p);
            (c, i)
        });
    let batches = budget_batches(inputs, MAX_INPUTS, MAX_CHARS);
    let out: Vec<Vec<Vec<f32>>> = futures::stream::iter(batches)
        .map(|batch| {
            let url = url.clone();
            async move {
                let mut body = serde_json::json!({"model": model, "input": batch, "encoding_format": "float"});
                // Only the text-embedding-3 models take a width; the others are always their own.
                if base_model(model).starts_with("text-embedding-3")
                    || Some(*dims) != known_dims(model)
                {
                    body["dimensions"] = (*dims).into();
                }
                let out = post_json(&url, key, &body, label, openrouter).await?;
                let data = out["data"]
                    .as_array()
                    .with_context(|| format!("{label}: no data in the reply"))?;
                // Replies carry each vector's input index; they need not come back in order.
                let mut vecs: Vec<Option<Vec<f32>>> = vec![None; batch.len()];
                for d in data {
                    let i = d["index"]
                        .as_u64()
                        .with_context(|| format!("{label}: a vector without an index"))?
                        as usize;
                    let slot = vecs.get_mut(i).with_context(|| {
                        format!("{label}: index {i} out of range")
                    })?;
                    *slot = Some(floats(&d["embedding"]));
                }
                let vecs = vecs
                    .into_iter()
                    .map(|v| v.with_context(|| format!("{label}: an input without a vector")))
                    .collect::<Result<Vec<_>>>()?;
                for v in &vecs {
                    anyhow::ensure!(
                        v.len() == *dims as usize,
                        "{label}: the model returned {} dims, the index expects {dims}; set MEMSTORE_EMBED_DIMS={} and rebuild the index",
                        v.len(),
                        v.len()
                    );
                }
                Ok::<_, anyhow::Error>(vecs)
            }
        })
        .buffered(API_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(fold_pieces(&counts, out.concat()))
}

/// The largest byte index at or below `max` that is a char boundary of `s`.
fn floor_char(s: &str, max: usize) -> usize {
    let mut i = max.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
