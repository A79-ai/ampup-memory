//! The model-call cache (`MEMSTORE_MODEL_CACHE`): every reply a model gave memstore, kept by the request that asked
//! for it, so a request is paid for once wherever it is asked again: the same store rebuilt, a fresh store of the
//! same inputs (another fold, another arm, another machine sharing the location), a build rerun after a version
//! bump that left the prompts alone. Every call an [`super::Extractor`] makes goes through it: extraction,
//! reconciling, compiling skills and AIP graphs, learning advice (sorting, linking, notes), judging, routing.
//!
//! - **Keyed by the request as sent.** An entry is the SHA-256 of the backend (`openrouter`, `anthropic`, ...),
//!   the model and the request body exactly as it goes to the API (messages, `max_tokens`, `temperature`,
//!   thinking), under `<root>/v1/<backend>-<model>/<first byte>/<hash>.json`. Credentials and the URL are not part
//!   of it, so a request through a metering proxy or with another key is the same request. `v1` names what a key
//!   covers; a change to that starts a new prefix.
//! - **The reply as the API gave it.** An entry is the reply's body, byte for byte, and a hit goes through the same
//!   parsing as a fresh reply. Only a complete answer is kept: a 200 that finished on its own (not cut off at the
//!   token cap, not refused or filtered) and that its caller could use (a caller that would ask again for a reply
//!   it can't read, such as a compile round with no JSON, says what it can use; see
//!   [`super::Extractor::complete_if`]). Errors are never kept, so a retry asks the model again.
//! - **One immutable file per reply.** A write creates the entry's file if it is absent and never rewrites one, so
//!   any number of processes share a location; two that ask the same thing at once both pay, and the first reply
//!   written stays. A read is one object read by path, nothing listed or held in memory: model calls take seconds,
//!   so a few milliseconds of lookup don't show.
//! - **Never in the way.** A failed read or write is logged once and costs a model call, never the call itself.
//!
//! The location is a directory or an object-store URI (`gs://`, `s3://`, `file://`), reached as the embedding cache
//! reaches its own ([`crate::search::embed_cache::operator`]). `off` turns it off. Unset, the commands that build a
//! memory (`compile`, `backfill`, `extract`, `advice`, `aggregate`, `load`) and `serve --local` keep it in
//! `~/.cache/memstore/model-calls` ([`default_to`]), and any other process (a server serving people) keeps none:
//! an answer to a live question isn't replayed unless its operator names a location.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, OnceLock};

use anyhow::Result;
use sha2::{Digest, Sha256};

/// What a key covers (backend, model, the body as sent); a change to that gets a new one.
const RECIPE: &str = "v1";

/// Replies the model calls memstore made got: `hit` if the cache held it (no model was paid), `miss` if the model
/// answered (paid). With no cache every reply is a miss. Served on `/metrics` as `memstore_model_cache_total`.
pub static CALLS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    let c = prometheus::IntCounterVec::new(
        prometheus::Opts::new(
            "memstore_model_cache_total",
            "Model replies memstore got: hit (the model-call cache held it, nothing paid) or miss (the model answered)",
        ),
        &["result"],
    )
    .unwrap();
    // Both series from the start, at 0.
    c.with_label_values(&["hit"]);
    c.with_label_values(&["miss"]);
    c
});

static DEFAULT: OnceLock<String> = OnceLock::new();

/// Where the cache lives when `MEMSTORE_MODEL_CACHE` is unset: what a build's command or `serve --local` sets
/// ([`build_location`]) before its first model call. Takes effect only before then.
pub fn default_to(location: impl Into<String>) {
    let _ = DEFAULT.set(location.into());
}

/// The location builds keep the cache in by default: `~/.cache/memstore/model-calls`, beside the models and
/// indexes memstore keeps there, so every store built on this machine shares it.
pub fn build_location() -> String {
    format!(
        "{}/.cache/memstore/model-calls",
        std::env::var("HOME").unwrap_or_else(|_| ".".into())
    )
}

static CACHE: LazyLock<Option<Cache>> = LazyLock::new(|| {
    let location = match std::env::var("MEMSTORE_MODEL_CACHE") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT.get()?.clone(),
    };
    if location.eq_ignore_ascii_case("off") {
        return None;
    }
    match crate::search::embed_cache::operator(&location) {
        Ok(op) => {
            let location = format!("{}/{RECIPE}", location.trim_end_matches('/'));
            eprintln!("model-call cache: {location}");
            Some(Cache {
                op,
                location,
                warned_read: AtomicBool::new(false),
                warned_write: AtomicBool::new(false),
            })
        }
        Err(e) => {
            eprintln!("model-call cache: none ({e:#}); every model call is paid for");
            None
        }
    }
});

/// Where the process's cache is, or `off`.
pub fn describe() -> String {
    CACHE
        .as_ref()
        .map_or_else(|| "off".into(), |c| c.location.clone())
}

/// (hits, misses) so far in this process.
pub fn counts() -> (u64, u64) {
    (
        CALLS.with_label_values(&["hit"]).get(),
        CALLS.with_label_values(&["miss"]).get(),
    )
}

struct Cache {
    op: opendal::Operator,
    location: String,
    warned_read: AtomicBool,
    warned_write: AtomicBool,
}

/// Where one request's reply is kept.
pub(crate) struct Key(String);

/// The key of a request: `body` exactly as it is sent to `backend`'s API for `model`. None when there is no cache.
pub(crate) fn key(backend: &str, model: &str, body: &str) -> Option<Key> {
    CACHE.as_ref()?;
    let mut h = Sha256::new();
    for part in [RECIPE, backend, model, body] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part.as_bytes());
    }
    let hash = crate::auth::hex(&h.finalize());
    let space: String = format!("{backend}-{model}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    Some(Key(format!("{RECIPE}/{space}/{}/{hash}.json", &hash[..2])))
}

/// The reply kept for `key`, if any.
pub(crate) async fn get(key: &Key) -> Option<String> {
    let c = CACHE.as_ref()?;
    match c.op.read(&key.0).await {
        Ok(b) => match String::from_utf8(b.to_vec()) {
            Ok(s) => Some(s),
            Err(_) => {
                c.warn("read", &anyhow::anyhow!("{} is not text", key.0));
                None
            }
        },
        Err(e) if e.kind() == opendal::ErrorKind::NotFound => None,
        Err(e) => {
            c.warn("read", &anyhow::Error::from(e).context(key.0.clone()));
            None
        }
    }
}

/// Keeps `reply` under `key`, unless a reply is kept there already; `replace`: even then (the one kept is one its
/// caller can no longer use).
pub(crate) async fn put(key: &Key, reply: &str, replace: bool) {
    let Some(c) = CACHE.as_ref() else {
        return;
    };
    if let Err(e) = c.write(&key.0, reply.as_bytes().to_vec(), replace).await {
        c.warn("write", &e);
    }
}

impl Cache {
    fn warn(&self, what: &str, e: &anyhow::Error) {
        let flag = if what == "read" {
            &self.warned_read
        } else {
            &self.warned_write
        };
        if !flag.swap(true, Ordering::Relaxed) {
            eprintln!(
                "model-call cache {}: can't {what} ({e:#}); carrying on without (further {what} failures aren't logged)",
                self.location
            );
        }
    }

    async fn write(&self, path: &str, b: Vec<u8>, replace: bool) -> Result<()> {
        use anyhow::Context;
        let w = self.op.write_with(path, b);
        let r = if !replace && self.op.info().full_capability().write_with_if_not_exists {
            w.if_not_exists(true).await
        } else {
            w.await
        };
        match r {
            Err(e) if e.kind() != opendal::ErrorKind::ConditionNotMatch => {
                Err(e).context(format!("write {path}"))
            }
            _ => Ok(()),
        }
    }
}
