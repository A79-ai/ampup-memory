//! The embedding cache (`MEMSTORE_EMBED_CACHE`): the vectors index updates computed, kept by the text they embed,
//! so the same text is embedded once wherever it is indexed again: another scope, workspace or branch, a
//! rebuilt index, a fresh stack loading a kept corpus. An index update looks up each text it has no vector
//! for, sends only the misses to the model, and writes their vectors back.
//!
//! - **Keyed by content.** An entry is the vector of one text, keyed by the SHA-256 of the text exactly as it is
//!   sent to the model, under the backend, model and width (`<root>/v1/<backend>-<model>-<dims>/`). A vector
//!   is stored as the model returned it (after the engine's own normalization), as little-endian `f32`s, so a
//!   cached vector is bit for bit the one the model gave the first time. `v1` names how the engine turns a
//!   text into a vector (pieces of a long text averaged, unit length); a change to that gets a new prefix.
//! - **Sharded, immutable segments.** Entries live in 16 shards by the hash's first four bits. A write adds one
//!   segment file per shard it touches (`<shard>/<content hash>-<count>.seg`: a header, the sorted hashes,
//!   then the vectors), never rewrites one. Concurrent writers can't clash: two that write the same entries
//!   write the same file, and duplicates across files are harmless.
//! - **Read lazily.** A shard's segment list and hash tables are read the first time a lookup needs that shard,
//!   then kept in memory (16 bytes of hash and a position per entry; at most [`MAX_ENTRIES`] in all, the least
//!   recently used shards going first). A lookup that misses in a shard listed more than [`RELIST_AFTER`] ago
//!   lists it again, for segments other processes wrote since. Vectors are read with ranged reads, one per run
//!   of nearby hits in a segment, and not kept.
//! - **Never in the way.** A failed read or write is logged once and treated as a miss or skipped: the cache
//!   can cost an index update a model call, never fail it. Searches don't use it (see [`embed_cached`]).
//!
//! The location is a directory or an object-store URI (`gs://`, `s3://`, `file://`), reached with opendal, which
//! memstore's Iceberg storage uses too: GCS with `GCP_ACCESS_TOKEN` when set, else application default credentials
//! (a pod's workload identity), S3 with `AWS_ENDPOINT`, `AWS_REGION` and the AWS credentials the search index reads.
//! `off` turns it off.
//! Unset, `serve --local` keeps it in its directory (`<dir>/embeddings`) and any other process keeps none.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use sha2::{Digest, Sha256};

use super::embed::Provider;

/// How the engine turns a text into a vector; a change to that (pieces, averaging, normalization) gets a new one.
const RECIPE: &str = "v1";
const SHARDS: usize = 16;
const MAGIC: &[u8; 8] = b"MSEMBED1";
/// Magic, width (u32) and entry count (u32).
const HEADER: usize = 16;
/// Entries whose hash tables are held in memory, every shard together (about 40 bytes each).
pub const MAX_ENTRIES: usize = 4_000_000;
/// A lookup that misses in a shard listed longer ago than this lists it again.
pub const RELIST_AFTER: Duration = Duration::from_secs(10);
/// Hits this many entries apart or fewer in a segment are read together (the entries between read too).
const GAP: u32 = 32;
/// Reads or writes in flight per shard, and shards at once.
const REQUESTS: usize = 16;
const SHARDS_AT_ONCE: usize = 4;

type Hash = [u8; 32];
/// Vectors found, each with the index of its key.
type Found = Vec<(usize, Vec<f32>)>;
/// Nearby hits in one segment, read together: its path, its entry count, and each hit's position and key index.
type Run = (String, u32, Vec<(u32, usize)>);

/// Texts index updates needed a vector for: `hit` if the cache held it, `miss` if the model computed it (and it
/// was written to the cache). With no cache every text is a miss. Served on `/metrics` as
/// `memstore_embed_cache_total`.
pub static LOOKUPS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    let c = prometheus::IntCounterVec::new(
        prometheus::Opts::new(
            "memstore_embed_cache_total",
            "Texts index updates needed a vector for: hit (the embedding cache held it) or miss (the model computed it)",
        ),
        &["result"],
    )
    .unwrap();
    // Both series from the start, at 0.
    c.with_label_values(&["hit"]);
    c.with_label_values(&["miss"]);
    c
});

/// (hits, misses) so far in this process.
pub fn counts() -> (u64, u64) {
    (
        LOOKUPS.with_label_values(&["hit"]).get(),
        LOOKUPS.with_label_values(&["miss"]).get(),
    )
}

static DEFAULT: OnceLock<String> = OnceLock::new();

/// Where the cache lives when `MEMSTORE_EMBED_CACHE` is unset (`serve --local`: its directory). Takes effect only
/// before the first embedding.
pub fn default_to(location: impl Into<String>) {
    let _ = DEFAULT.set(location.into());
}

static CACHE: LazyLock<Option<Cache>> = LazyLock::new(|| {
    let location = match std::env::var("MEMSTORE_EMBED_CACHE") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT.get()?.clone(),
    };
    if location.eq_ignore_ascii_case("off") {
        return None;
    }
    match Cache::open(&location, super::embed::provider()) {
        Ok(c) => {
            eprintln!("embedding cache: {}", c.location);
            Some(c)
        }
        Err(e) => {
            eprintln!("embedding cache: none ({e:#}); index updates embed every text they lack");
            None
        }
    }
});

/// Where the process's cache is (with its backend's prefix), or `off`.
pub fn describe() -> String {
    CACHE
        .as_ref()
        .map_or_else(|| "off".into(), |c| c.location.clone())
}

/// Unit-length embeddings of `texts` for an index update: from the cache when it holds them, the rest from the
/// model (and then into the cache). Searches embed with [`super::embed`] instead: a query is one short text
/// (and the last 1024 are kept in memory already), and a lookup in a shard not yet read would add object-store
/// round trips to a search's latency.
pub async fn embed_cached(texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
    let Some(cache) = CACHE.as_ref().filter(|_| !texts.is_empty()) else {
        LOOKUPS
            .with_label_values(&["miss"])
            .inc_by(texts.len() as u64);
        return super::embed(texts).await;
    };
    let keys: Vec<Hash> = texts
        .iter()
        .map(|t| Sha256::digest(t.as_bytes()).into())
        .collect();
    let mut out = cache.get(&keys).await;
    let missing: Vec<usize> = (0..texts.len()).filter(|&i| out[i].is_none()).collect();
    LOOKUPS
        .with_label_values(&["hit"])
        .inc_by((texts.len() - missing.len()) as u64);
    if !missing.is_empty() {
        let fresh = super::embed(missing.iter().map(|&i| texts[i].clone()).collect()).await?;
        LOOKUPS
            .with_label_values(&["miss"])
            .inc_by(missing.len() as u64);
        let keys: Vec<Hash> = missing.iter().map(|&i| keys[i]).collect();
        cache.put(&keys, &fresh).await;
        for (i, v) in missing.into_iter().zip(fresh) {
            out[i] = Some(v);
        }
    }
    Ok(out
        .into_iter()
        .map(|v| v.expect("a vector per text"))
        .collect())
}

/// What one shard holds in memory: its segments and where each entry is.
#[derive(Default)]
struct Shard {
    /// When the segments were listed; None: not read yet (or evicted).
    listed: Option<Instant>,
    /// Each segment's path and entry count.
    segments: Vec<(String, u32)>,
    /// Entry (the first 16 bytes of its hash) to (segment, position).
    index: HashMap<u128, (u32, u32)>,
}

struct Cache {
    op: opendal::Operator,
    /// `v1/<backend>-<model>-<dims>/`, under the operator's root.
    dir: String,
    dims: usize,
    location: String,
    shards: Vec<tokio::sync::Mutex<Shard>>,
    /// Each shard's entries in memory and when it was last used (ms since `started`), read without its lock.
    sizes: Vec<AtomicUsize>,
    used: Vec<AtomicU64>,
    started: Instant,
    warned_read: AtomicBool,
    warned_write: AtomicBool,
}

fn shard_of(h: &Hash) -> usize {
    (h[0] >> 4) as usize
}

fn short(h: &[u8]) -> u128 {
    u128::from_le_bytes(h[..16].try_into().unwrap())
}

/// The backend, model and width, as a path segment: `openai-text-embedding-3-small-1536`.
fn space(p: &Provider) -> String {
    let (kind, model) = match p {
        Provider::Local { code, .. } => ("local", code),
        Provider::OpenAI { model, .. } => ("openai", model),
        Provider::OpenRouter { model, .. } => ("openrouter", model),
    };
    format!("{kind}-{model}-{}", p.dims())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// An operator rooted at `location`: `gs://bucket/prefix`, `s3://bucket/prefix`, or a directory (`file://` or a
/// path; `~/` is the home directory), made if missing. The model-call cache (`memstore::extract::cache`) lives the
/// same way.
pub fn operator(location: &str) -> Result<opendal::Operator> {
    opendal::init_default_registry();
    let bucket_and_root = |rest: &str| {
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        vec![
            ("bucket".to_string(), bucket.to_string()),
            ("root".to_string(), format!("/{}", prefix.trim_matches('/'))),
        ]
    };
    let (scheme, config) = if let Some(rest) = location.strip_prefix("gs://") {
        let mut config = bucket_and_root(rest);
        // A token given, as the gcp platform takes one (`gcloud auth print-access-token` on a laptop); else
        // application default credentials or the metadata server.
        if let Ok(t) = std::env::var("GCP_ACCESS_TOKEN") {
            config.push(("token".into(), t));
        }
        ("gcs", config)
    } else if let Some(rest) = location.strip_prefix("s3://") {
        ("s3", bucket_and_root(rest))
    } else {
        let path = location.strip_prefix("file://").unwrap_or(location);
        let path = match path.strip_prefix("~/") {
            Some(rest) => std::path::PathBuf::from(std::env::var("HOME")?).join(rest),
            None => std::path::PathBuf::from(path),
        };
        std::fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
        let root = path.canonicalize()?.display().to_string();
        // Written beside, then renamed into place: a reader never sees half a segment.
        let tmp = format!("{root}/.tmp");
        (
            "fs",
            vec![("root".into(), root), ("atomic_write_dir".into(), tmp)],
        )
    };
    Ok(opendal::Operator::via_iter(scheme, config)
        .with_context(|| format!("object store {location}"))?
        .layer(opendal::layers::RetryLayer::new()))
}

impl Cache {
    fn open(location: &str, provider: &Provider) -> Result<Cache> {
        let space = space(provider);
        Ok(Cache {
            op: operator(location)?,
            dir: format!("{RECIPE}/{space}/"),
            dims: provider.dims() as usize,
            location: format!("{}/{RECIPE}/{space}", location.trim_end_matches('/')),
            shards: (0..SHARDS).map(|_| Default::default()).collect(),
            sizes: (0..SHARDS).map(|_| AtomicUsize::new(0)).collect(),
            used: (0..SHARDS).map(|_| AtomicU64::new(0)).collect(),
            started: Instant::now(),
            warned_read: AtomicBool::new(false),
            warned_write: AtomicBool::new(false),
        })
    }

    fn warn(&self, what: &str, e: &anyhow::Error) {
        let flag = if what == "read" {
            &self.warned_read
        } else {
            &self.warned_write
        };
        if !flag.swap(true, Ordering::Relaxed) {
            eprintln!(
                "embedding cache {}: can't {what} ({e:#}); carrying on without (further {what} failures aren't logged)",
                self.location
            );
        }
    }

    /// The cached vector of each key, where there is one.
    async fn get(&self, keys: &[Hash]) -> Vec<Option<Vec<f32>>> {
        let mut by_shard: Vec<Vec<usize>> = vec![vec![]; SHARDS];
        for (i, k) in keys.iter().enumerate() {
            by_shard[shard_of(k)].push(i);
        }
        let found: Vec<Found> = futures::stream::iter(
            by_shard
                .into_iter()
                .enumerate()
                .filter(|(_, idx)| !idx.is_empty()),
        )
        .map(|(s, idx)| async move {
            self.get_shard(s, &idx, keys).await.unwrap_or_else(|e| {
                self.warn("read", &e);
                vec![]
            })
        })
        .buffer_unordered(SHARDS_AT_ONCE)
        .collect()
        .await;
        let mut out = vec![None; keys.len()];
        for (i, v) in found.into_iter().flatten() {
            out[i] = Some(v);
        }
        self.evict();
        out
    }

    /// The vectors shard `s` holds of `keys[i]` for `i` in `idx`, as (i, vector).
    async fn get_shard(&self, s: usize, idx: &[usize], keys: &[Hash]) -> Result<Found> {
        let mut shard = self.shards[s].lock().await;
        let lacks = |sh: &Shard| {
            idx.iter()
                .any(|&i| !sh.index.contains_key(&short(&keys[i])))
        };
        if shard
            .listed
            .is_none_or(|t| t.elapsed() >= RELIST_AFTER && lacks(&shard))
        {
            self.list(s, &mut shard).await?;
        }
        self.touch(s, shard.index.len());
        // Runs of nearby hits in each segment, one read each.
        let mut by_segment: HashMap<u32, Vec<(u32, usize)>> = HashMap::new();
        for &i in idx {
            if let Some(&(seg, pos)) = shard.index.get(&short(&keys[i])) {
                by_segment.entry(seg).or_default().push((pos, i));
            }
        }
        let mut runs: Vec<Run> = vec![];
        for (seg, mut hits) in by_segment {
            let (path, count) = &shard.segments[seg as usize];
            hits.sort_unstable();
            let mut run: Vec<(u32, usize)> = vec![];
            for h in hits {
                if run.last().is_some_and(|l| h.0 - l.0 > GAP) {
                    runs.push((path.clone(), *count, std::mem::take(&mut run)));
                }
                run.push(h);
            }
            runs.push((path.clone(), *count, run));
        }
        drop(shard);
        let width = self.dims * 4;
        let read: Vec<Result<Found>> = futures::stream::iter(runs)
            .map(|(path, count, run)| async move {
                let base = HEADER + 32 * count as usize;
                let (lo, hi) = (run[0].0 as usize, run[run.len() - 1].0 as usize + 1);
                let b = self
                    .op
                    .read_with(&path)
                    .range((base + lo * width) as u64..(base + hi * width) as u64)
                    .await?
                    .to_bytes();
                anyhow::ensure!(b.len() == (hi - lo) * width, "{path}: a short read");
                Ok(run
                    .into_iter()
                    .map(|(pos, i)| {
                        let at = (pos as usize - lo) * width;
                        (i, floats(&b[at..at + width]))
                    })
                    .collect())
            })
            .buffer_unordered(REQUESTS)
            .collect()
            .await;
        let mut out = vec![];
        for r in read {
            match r {
                Ok(v) => out.extend(v),
                Err(e) => self.warn("read", &e),
            }
        }
        Ok(out)
    }

    /// Reads the hash tables of the shard's segments it doesn't hold yet.
    async fn list(&self, s: usize, shard: &mut Shard) -> Result<()> {
        let started = Instant::now();
        let prefix = format!("{}{s:x}/", self.dir);
        let entries = match self.op.list(&prefix).await {
            Ok(e) => e,
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e).context(format!("list {prefix}")),
        };
        let held: HashSet<&str> = shard.segments.iter().map(|(p, _)| p.as_str()).collect();
        let new: Vec<(String, u32)> = entries
            .iter()
            .filter_map(|e| {
                let count = e.name().strip_suffix(".seg")?.rsplit_once('-')?.1;
                let path = format!("{prefix}{}", e.name());
                (!held.contains(path.as_str())).then_some((path, count.parse().ok()?))
            })
            .collect();
        let tables: Vec<_> = futures::stream::iter(new)
            .map(|(path, count)| async move {
                let r = self
                    .op
                    .read_with(&path)
                    .range(0..(HEADER + 32 * count as usize) as u64)
                    .await;
                (path, count, r)
            })
            .buffer_unordered(REQUESTS)
            .collect()
            .await;
        for (path, count, r) in tables {
            let b = match r {
                Ok(b) => b.to_bytes(),
                Err(e) => {
                    self.warn("read", &anyhow::Error::from(e).context(path));
                    continue;
                }
            };
            let u32_at = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
            if b.len() != HEADER + 32 * count as usize
                || &b[..8] != MAGIC
                || u32_at(8) as usize != self.dims
                || u32_at(12) != count
            {
                self.warn(
                    "read",
                    &anyhow::anyhow!("{path} is not a segment of this cache"),
                );
                continue;
            }
            let seg = shard.segments.len() as u32;
            shard.segments.push((path, count));
            for pos in 0..count {
                let at = HEADER + 32 * pos as usize;
                shard
                    .index
                    .entry(short(&b[at..at + 32]))
                    .or_insert((seg, pos));
            }
        }
        shard.listed = Some(started);
        Ok(())
    }

    /// Writes `vecs` under `keys`: a new segment in each shard they fall in.
    async fn put(&self, keys: &[Hash], vecs: &[Vec<f32>]) {
        let mut by_shard: Vec<Vec<usize>> = vec![vec![]; SHARDS];
        for (i, k) in keys.iter().enumerate() {
            if vecs[i].len() == self.dims {
                by_shard[shard_of(k)].push(i);
            }
        }
        let segments: Vec<(usize, String, Vec<Hash>, Vec<u8>)> = by_shard
            .into_iter()
            .enumerate()
            .filter(|(_, idx)| !idx.is_empty())
            .map(|(s, mut idx)| {
                idx.sort_by_key(|&i| keys[i]);
                idx.dedup_by_key(|i| keys[*i]);
                let mut b = Vec::with_capacity(HEADER + idx.len() * (32 + self.dims * 4));
                b.extend_from_slice(MAGIC);
                b.extend_from_slice(&(self.dims as u32).to_le_bytes());
                b.extend_from_slice(&(idx.len() as u32).to_le_bytes());
                for &i in &idx {
                    b.extend_from_slice(&keys[i]);
                }
                for &i in &idx {
                    for x in &vecs[i] {
                        b.extend_from_slice(&x.to_le_bytes());
                    }
                }
                let id = crate::auth::hex(&Sha256::digest(&b)[..16]);
                let path = format!("{}{s:x}/{id}-{}.seg", self.dir, idx.len());
                (s, path, idx.iter().map(|&i| keys[i]).collect(), b)
            })
            .collect();
        futures::stream::iter(segments)
            .map(|(s, path, hashes, b)| async move {
                if let Err(e) = self.write(&path, b).await {
                    return self.warn("write", &e);
                }
                // A shard already read takes the segment now; one not read yet finds it when it is listed.
                let mut shard = self.shards[s].lock().await;
                if shard.listed.is_some() && !shard.segments.iter().any(|(p, _)| *p == path) {
                    let seg = shard.segments.len() as u32;
                    shard.segments.push((path, hashes.len() as u32));
                    for (pos, h) in hashes.iter().enumerate() {
                        shard.index.entry(short(h)).or_insert((seg, pos as u32));
                    }
                    self.touch(s, shard.index.len());
                }
            })
            .buffer_unordered(REQUESTS)
            .collect::<Vec<()>>()
            .await;
    }

    /// Writes a segment unless it is there already (the same entries, written by another process).
    async fn write(&self, path: &str, b: Vec<u8>) -> Result<()> {
        let w = self.op.write_with(path, b);
        let r = if self.op.info().full_capability().write_with_if_not_exists {
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

    fn touch(&self, s: usize, entries: usize) {
        self.sizes[s].store(entries, Ordering::Relaxed);
        self.used[s].store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Past [`MAX_ENTRIES`] in memory, the least recently used shards are dropped (read again when needed).
    fn evict(&self) {
        let mut total: usize = self.sizes.iter().map(|n| n.load(Ordering::Relaxed)).sum();
        if total <= MAX_ENTRIES {
            return;
        }
        let mut order: Vec<usize> = (0..SHARDS)
            .filter(|&s| self.sizes[s].load(Ordering::Relaxed) > 0)
            .collect();
        order.sort_by_key(|&s| self.used[s].load(Ordering::Relaxed));
        for s in order {
            if total <= MAX_ENTRIES {
                break;
            }
            if let Ok(mut shard) = self.shards[s].try_lock() {
                total -= shard.index.len();
                *shard = Shard::default();
                self.sizes[s].store(0, Ordering::Relaxed);
            }
        }
    }
}

fn floats(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}
