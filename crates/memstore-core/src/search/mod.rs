//! The derived index over facts (spec A-4): one Lance dataset per scope, kept next to the data
//! and rebuildable from the facts table at any time (design rule 3). It carries a BM25
//! inverted index for lexical search, an embedding per fact for semantic search (the two are
//! fused by reciprocal rank), and BTree scalar indexes for fact-key lookups, so none needs the
//! scope's facts in memory.
//!
//! The index covers whole Iceberg data files, keyed by path plus delete files. Index rows are
//! trusted only for keys still in the table's current snapshot; every other current file
//! (newer than the last `update`, rewritten by compaction, or with new row deletes such as an
//! erasure) is scanned directly. So the index can be stale but never serves a deleted row, and
//! freshness never waits on indexing (NFR-3). Every hit is post-filtered like a read.

mod cache;
pub mod embed;
pub mod embed_cache;
pub mod manage;
pub mod passages;
pub mod rows;
pub mod when;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{
    Array, ArrayRef, FixedSizeListArray, Float32Array, Int64Array, RecordBatch,
    RecordBatchIterator, StringArray, cast::AsArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lance::Dataset;
use lance::dataset::{WriteMode, WriteParams};
use lance::index::DatasetIndexExt;
use lance_index::IndexType;
use lance_index::scalar::{
    BuiltinIndexType, FullTextSearchQuery, InvertedIndexParams, ScalarIndexParams,
};

use serde::{Deserialize, Serialize};

use crate::{Fact, Principal, Scope, Store};

/// The keyword-searched fields of a fact, each with its own inverted index, and the BM25F weight it has in a
/// search (`AMPUP_MEMORY_RANK_W_<FIELD>`): what a fact says counts most, then whom it is about, its quote, and the
/// predicate least. Lance needs every weight at least 1, so the weights are these ratios scaled up.
pub const FTS_FIELDS: [(&str, &str, f32); 5] = [
    ("t_value", "VALUE", 1.0),
    ("t_subject", "SUBJECT", 0.6),
    ("t_quote", "QUOTE", 0.5),
    ("t_pred", "PRED", 0.2),
    ("t_date", "DATE", 0.2),
];
/// Embedding width of the process's provider (see `crate::search::embed`).
pub fn dims() -> i32 {
    crate::search::embed::dims()
}
/// Reciprocal rank fusion constant: a fact's score is the sum of 1/(RRF_K + rank) over the
/// keyword and vector rankings.
pub const RRF_K: f32 = 60.0;
/// A hit's keyword and vector scores are scaled by `max(1 / (1 + age in years), RECENCY_FLOOR)`
/// before ranking (see [`Rank`]): recency reorders near-ties and never outranks a clearly better match.
pub const RECENCY_FLOOR: f32 = 0.75;
/// A passage page spreads over kinds of source: each hit from a kind already on the page scores
/// this much less than it would (see [`Rank::diverse`]).
pub const DIVERSITY_DECAY: f32 = 0.9;

/// How hits are ordered beyond how well they match.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rank {
    /// Date recency against this time (µs since the epoch): newer facts (by `valid_from`) and
    /// passages (by when their episode occurred) rank a little higher, by [`recency_bias`].
    /// None ranks by match alone.
    pub as_of: Option<i64>,
    /// Spread a page of passages over kinds of source (`email`, `slack`, `meeting`): once a kind
    /// is on the page, its next hit counts [`DIVERSITY_DECAY`] times as much, and so on. Facts
    /// ignore it.
    pub diverse: bool,
    /// Group fact hits: each hit carries up to this many facts about the same entity from its source
    /// episodes (and the other items of its list, the same entity and predicate) as [`Hit::group`], and a
    /// group is one of the `k` hits. 0 doesn't group. Passages ignore it.
    pub group: usize,
    /// Rank facts as before the current ranking, for a scope whose admins asked (`Settings::classic_ranking`): one keyword
    /// ranking (BM25 over a fact's subject, predicate and value alike, any of the terms) and the nearest ones,
    /// fused by reciprocal rank, each scaled by recency as of `as_of`; a pool of `FETCH_MULTIPLE` rows per hit
    /// asked for; no keyword rescoring of vector-only candidates, no time window, no near-duplicate folding and
    /// no groups. Passages ignore it.
    pub classic: bool,
}

impl Rank {
    /// Recency against now, no diversity: the default for fact search.
    pub fn now() -> Rank {
        Rank {
            as_of: Some(crate::now_micros()),
            diverse: false,
            group: 0,
            classic: false,
        }
    }
    /// Match alone.
    pub fn plain() -> Rank {
        Rank {
            as_of: None,
            diverse: false,
            group: 0,
            classic: false,
        }
    }
}

/// The recency scale of something from `at`, read as of `as_of`: 1 when it is new, unknown
/// (`at <= 0`) or there is no `as_of`; down to [`RECENCY_FLOOR`] (reached at four months) as it ages.
pub fn recency_bias(at: i64, as_of: Option<i64>) -> f32 {
    let Some(as_of) = as_of else { return 1.0 };
    if at <= 0 || at >= as_of {
        return 1.0;
    }
    let years = (as_of - at) as f64 / (365.25 * 86_400.0 * 1e6);
    (1.0 / (1.0 + years)).max(RECENCY_FLOOR as f64) as f32
}
/// Parts with at least this many rows get a vector index; smaller ones are scanned.
pub const VEC_INDEX_ROWS: usize = 20_000;
/// The vector index a part gets: IVF_SQ, about sqrt(rows) partitions, each vector held as one byte a
/// dimension. It replaced IVF_FLAT (four bytes a dimension), whose index of a production deployment's facts (868k rows,
/// 5.3 GB) outgrew the index cache, so each search read it again from object storage: 7–28 s.
pub fn vec_index(rows: usize) -> lance::index::vector::VectorIndexParams {
    lance::index::vector::VectorIndexParams::with_ivf_sq_params(
        lance_linalg::distance::MetricType::L2,
        lance_index::vector::ivf::IvfBuildParams::new(((rows as f64).sqrt() as usize).max(1)),
        Default::default(),
    )
}
/// What Lance names the index [`vec_index`] builds; a part whose vector index is another kind is rebuilt.
pub const VEC_INDEX_KIND: &str = "IVF_SQ";
/// IVF partitions a vector search probes (`AMPUP_MEMORY_INDEX_NPROBES`): all of them. Measured on a copy of a production
/// 868k facts with 38 real asks and 200 facts as queries: 40 ms, and 0.97 of the exact top 15 (0.99 of the top
/// 300); probing 320 of 931 partitions took 15 ms but kept 0.935 of an ask's top 15.
const NPROBES: usize = 1 << 20;
/// When set (`AMPUP_MEMORY_INDEX_REFINE`), a vector search ranks this many times the rows asked for by their
/// one-byte vectors, then those again by their full vectors: exact top 15 (1.00), but each search then reads
/// that many rows from object storage (about 1 s for 600 from GCS), so it is off.
const REFINE: u32 = 0;

/// Sets how a vector search on an indexed part probes and refines (see [`vec_index`]).
pub fn probe(scan: &mut lance::dataset::scanner::Scanner) {
    scan.minimum_nprobes(index_knob("NPROBES", NPROBES));
    let refine = index_knob("REFINE", REFINE as usize) as u32;
    if refine > 0 {
        scan.refine(refine);
    }
}
/// Most unindexed facts one search embeds (see `search_with`).
pub const TAIL_EMBED_MAX: usize = 500;
/// Vectors of unindexed facts and passages by text, so repeated searches don't re-embed the same
/// tail. Cleared when it passes TAIL_CACHE_MAX entries (about 1.5 KB each).
static TAIL_CACHE: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Vec<f32>>>> =
    std::sync::LazyLock::new(Default::default);
const TAIL_CACHE_MAX: usize = 50_000;
/// Held by the search embedding tail text the cache lacks, so searches that arrive meanwhile wait
/// for its vectors instead of each embedding the same text.
static TAIL_EMBEDDING: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(Default::default);

/// Unit-length embeddings of unindexed text (`texts`, in order): from the tail cache, embedding
/// only what it lacks, once however many searches ask at the same time.
pub async fn embed_tail(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    // What's cached now, copied: a search may clear the cache while another embeds.
    let cached = |texts: &[String]| -> (HashMap<String, Vec<f32>>, Vec<String>) {
        let cache = TAIL_CACHE.lock().unwrap();
        let have: HashMap<String, Vec<f32>> = texts
            .iter()
            .filter_map(|t| cache.get(t).map(|v| (t.clone(), v.clone())))
            .collect();
        let missing = texts
            .iter()
            .filter(|t| !have.contains_key(*t))
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        (have, missing)
    };
    let (mut have, missing) = cached(texts);
    if !missing.is_empty() {
        let _turn = TAIL_EMBEDDING.lock().await;
        // The search ahead may have just embedded them.
        let (now, missing) = cached(texts);
        have = now;
        let fresh: HashMap<String, Vec<f32>> = missing
            .iter()
            .cloned()
            .zip(embed(missing.clone()).await?)
            .collect();
        let mut cache = TAIL_CACHE.lock().unwrap();
        if cache.len() + fresh.len() > TAIL_CACHE_MAX {
            cache.clear();
        }
        cache.extend(fresh.iter().map(|(t, v)| (t.clone(), v.clone())));
        have.extend(fresh);
    }
    Ok(texts.iter().map(|t| have[t].clone()).collect())
}

/// Embeddings of recent search texts, so the next page of a search, or the same question asked again,
/// doesn't embed it again. Cleared when it passes QUERY_CACHE_MAX texts.
static QUERY_CACHE: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Vec<f32>>>> =
    std::sync::LazyLock::new(Default::default);
const QUERY_CACHE_MAX: usize = 1024;

/// Search texts being embedded now: a search's facts and its source text are searched at once, and both
/// embed the query, which is computed once.
#[allow(clippy::type_complexity)]
static EMBEDDING: std::sync::LazyLock<
    std::sync::Mutex<HashMap<String, Arc<tokio::sync::OnceCell<Vec<f32>>>>>,
> = std::sync::LazyLock::new(Default::default);

/// The unit-length embedding of one text (a search's), from the cache of recent ones when it is there.
pub async fn embed_text(text: &str) -> Result<Vec<f32>> {
    if let Some(v) = QUERY_CACHE.lock().unwrap().get(text) {
        return Ok(v.clone());
    }
    let cell = EMBEDDING
        .lock()
        .unwrap()
        .entry(text.to_string())
        .or_default()
        .clone();
    let v = cell
        .get_or_try_init(|| async {
            let v = embed(vec![text.to_string()]).await?.remove(0);
            let mut cache = QUERY_CACHE.lock().unwrap();
            if cache.len() >= QUERY_CACHE_MAX {
                cache.clear();
            }
            cache.insert(text.to_string(), v.clone());
            anyhow::Ok(v)
        })
        .await
        .cloned();
    // Done (or failed, and the next search tries again): it is in the cache now.
    EMBEDDING.lock().unwrap().remove(text);
    v
}

/// Unit-length embeddings of `texts`, from the process's provider (`crate::search::embed`).
pub async fn embed(texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
    crate::search::embed::embed(texts).await
}

pub fn vec_field() -> Field {
    Field::new(
        "vec",
        DataType::FixedSizeList(
            Arc::new(Field::new("item", DataType::Float32, true)),
            dims(),
        ),
        false,
    )
}
/// Separates the parts of a key column; not expected inside entity, predicate or value.
const SEP: char = '\u{1f}';

/// A fact key, as stored in the `ekey` / `vkey` columns.
pub enum Key<'a> {
    /// (entity, predicate): "what is X's role".
    Entity(&'a str, &'a str),
    /// (predicate, value): "who has team = T".
    Value(&'a str, &'a str),
}

impl Key<'_> {
    fn column(&self) -> &'static str {
        match self {
            Key::Entity(..) => "ekey",
            Key::Value(..) => "vkey",
        }
    }
    fn text(&self) -> String {
        match self {
            Key::Entity(a, b) | Key::Value(a, b) => format!("{a}{SEP}{b}"),
        }
    }
    fn matches(&self, f: &Fact) -> bool {
        match self {
            Key::Entity(e, p) => f.entity == *e && f.predicate == *p,
            Key::Value(p, v) => f.predicate == *p && f.value == *v,
        }
    }
}

/// Default index location: `AMPUP_MEMORY_INDEX_URI` (a local path, `gs://…` or `s3://…`), else
/// `~/.cache/memstore/index`. A store can override it (see `Store::with_index_base`).
pub fn default_index_base() -> String {
    std::env::var("AMPUP_MEMORY_INDEX_URI").unwrap_or_else(|_| {
        format!(
            "{}/.cache/memstore/index",
            std::env::var("HOME").unwrap_or_else(|_| ".".into())
        )
    })
}

/// The facts index's directory name under the scope's index location.
pub fn dir_name() -> String {
    // v8: open/closed parts with the fact's quote, embedding, typed columns (filterable), unit,
    // citation, time basis and tags, plus a files marker. v9: a keyword index per field (value, subject,
    // quote, predicate; see FTS_FIELDS) instead of one over the joined text. v10: the fact's date as a
    // keyword field too (`2023-03-21 march 2023`). Older layouts are ignored (the
    // next `update` rebuilds the index from the facts table).
    // The default local model keeps the original path, so existing indexes stay valid; any other
    // embedding provider or model gets its own (see `crate::search::embed`).
    format!("{LAYOUT}{}", crate::search::embed::provider().layout_tag())
}

const LAYOUT: &str = "v10";

fn base_uri(base: &str, scope: &Scope) -> String {
    format!(
        "{}/{}/{}",
        base.trim_end_matches('/'),
        scope.namespace(),
        dir_name()
    )
}

/// Where the scope's facts index lives in the store's index location.
pub fn dir_uri(store: &Store, scope: &Scope) -> String {
    base_uri(store.index_base(), scope)
}

/// What an index is built with: its layout, embedding model, chunking (passages) and keyword (BM25)
/// settings. Each configuration has a directory of its own under the scope's index location
/// ([`IndexConfig::dir`]), so a change builds a second index beside the first and never edits it; the
/// first stays readable, by a store configured as it was, until it is dropped (`memstore index drop`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexConfig {
    /// `facts` or `passages`.
    pub kind: String,
    pub layout: String,
    /// The embedding provider and model, with its width.
    pub embedding: String,
    pub dims: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_overlap: Option<usize>,
    /// The keyword index's tokenizer settings (Lance's inverted index, BM25-ranked), as JSON.
    pub bm25: String,
    /// The directory it lives in.
    pub dir: String,
}

/// The keyword index's settings, as JSON.
pub fn bm25_settings() -> String {
    serde_json::to_string(&fts_params(true)).unwrap_or_default()
}

/// How a fact field's keyword index tokenizes: lower case, stemmed, ASCII-folded, and stop words kept, so a
/// phrase matches across them; positions for the fields phrases are matched in (value and quote). Every field
/// tokenizes alike, as a combined-fields (BM25F) query needs.
fn fts_params(positions: bool) -> InvertedIndexParams {
    InvertedIndexParams::default()
        .remove_stop_words(false)
        .with_position(positions)
}

/// Text as the keyword index reads it: a number's thousands separators dropped (`2,106` is `2106`), so the
/// query and the fact spell it alike.
pub fn normalize_text(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let digit = |i: usize| c.get(i).is_some_and(char::is_ascii_digit);
    let mut out = String::with_capacity(s.len());
    for (i, ch) in c.iter().enumerate() {
        // `1,234` and `12,345,678`: a comma after a digit with exactly three digits after it.
        if *ch == ',' && i > 0 && digit(i - 1) && (1..=3).all(|j| digit(i + j)) && !digit(i + 4) {
            continue;
        }
        out.push(*ch);
    }
    out
}

/// A fact's date as words, for keyword search: `2023-03-21 march 2023`. Empty when it has none.
fn date_text(valid_from: i64) -> String {
    match chrono::DateTime::from_timestamp_micros(valid_from).filter(|_| valid_from > 0) {
        Some(d) => format!(
            "{} {} {}",
            d.format("%Y-%m-%d"),
            d.format("%B").to_string().to_lowercase(),
            d.format("%Y")
        ),
        None => String::new(),
    }
}

/// A fact's subject as words: its entity id with the type prefix and `_` read as spaces
/// (`project:helio_start` is `project helio start`).
fn subject_text(entity: &str) -> String {
    normalize_text(&entity.replace([':', '_'], " "))
}

/// What the facts index is built with.
pub fn config() -> IndexConfig {
    IndexConfig {
        kind: "facts".into(),
        layout: LAYOUT.into(),
        embedding: crate::search::embed::provider().describe(),
        dims: dims(),
        chunk_bytes: None,
        chunk_overlap: None,
        bm25: bm25_settings(),
        dir: dir_name(),
    }
}

/// One update of an index, as its manifest (`manifest.lance` in the index's directory) records it: the
/// ledger heads it covers, each part's dataset version after it, and the configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    /// When it was recorded (µs since the epoch).
    pub at: i64,
    /// Per branch, the heads read before the update read the table's files: every commit reachable from
    /// them has its rows in the index. (Rows of commits still in flight then may be there too; reads show
    /// only committed rows a reader's branch or snapshot reaches.)
    pub heads: BTreeMap<String, Vec<String>>,
    pub versions: BTreeMap<String, u64>,
    pub config: IndexConfig,
}

/// The index versions a pinned read uses (a workspace snapshot's), per kind of index: the update it was
/// taken with, or none when the snapshot had no index of that kind covering at most its commits, so every
/// row comes from the tables.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IndexPin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<Coverage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passages: Option<Coverage>,
}

fn manifest_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![Field::new("row", DataType::Utf8, false)]))
}

/// Every update recorded in the manifest of the index in `dir`, oldest first; none without a manifest.
pub async fn coverage(dir: &str) -> Result<Vec<Coverage>> {
    let ds = match crate::search::open_ds(&format!("{dir}/manifest.lance")).await {
        Ok(d) => d,
        Err(lance::Error::DatasetNotFound { .. }) => return Ok(vec![]),
        Err(e) => return Err(e).context("open index manifest"),
    };
    let batches: Vec<RecordBatch> = ds.scan().try_into_stream().await?.try_collect().await?;
    let mut out: Vec<Coverage> = vec![];
    for b in &batches {
        for row in crate::strings(b, "row") {
            out.push(serde_json::from_str(&row)?);
        }
    }
    out.sort_by_key(|c| c.at);
    Ok(out)
}

/// Record an update in the manifest of the index in `dir`, unless the newest record already says the same.
pub async fn record_coverage(
    dir: &str,
    heads: BTreeMap<String, Vec<String>>,
    versions: BTreeMap<String, u64>,
    config: IndexConfig,
) -> Result<()> {
    // An index of nothing, of a scope with no commits yet: nothing to record (and no directory to make).
    if heads.is_empty() && versions.is_empty() {
        return Ok(());
    }
    if coverage(dir)
        .await?
        .last()
        .is_some_and(|c| c.heads == heads && c.versions == versions && c.config == config)
    {
        return Ok(());
    }
    let c = Coverage {
        at: crate::now_micros(),
        heads,
        versions,
        config,
    };
    let row = RecordBatch::try_new(
        manifest_schema(),
        vec![Arc::new(StringArray::from(vec![serde_json::to_string(
            &c,
        )?]))],
    )?;
    let uri = format!("{dir}/manifest.lance");
    let reader = RecordBatchIterator::new([Ok(row)], manifest_schema());
    match crate::search::open_ds(&uri).await {
        Ok(mut d) => d.append(reader, None).await?,
        Err(lance::Error::DatasetNotFound { .. }) => {
            let params = WriteParams {
                mode: WriteMode::Create,
                session: Some(session()),
                ..Default::default()
            };
            Dataset::write(reader, uri.as_str(), Some(params)).await?;
        }
        Err(e) => return Err(e).context("open index manifest"),
    }
    Ok(())
}

/// The newest update in `records` that covers nothing a snapshot doesn't reach: every head it records is in
/// `reach`, the commits reachable from the snapshot's heads.
pub fn newest_within(records: &[Coverage], reach: &HashSet<String>) -> Option<Coverage> {
    records
        .iter()
        .rev()
        .find(|c| c.heads.values().flatten().all(|h| reach.contains(h)))
        .cloned()
}

/// Keep the dataset versions `c` names from being cleaned up, by tagging each with `tag` (a snapshot id).
pub async fn tag_versions(dir: &str, c: &Coverage, tag: &str) -> Result<()> {
    for (part, v) in &c.versions {
        let ds = crate::search::open_ds(&format!("{dir}/{part}.lance")).await?;
        if let Err(e) = ds.tags().create(tag, *v).await
            && !e.to_string().contains("already exists")
        {
            return Err(e).with_context(|| format!("tag {dir}/{part}.lance version {v}"));
        }
    }
    Ok(())
}

/// A part of an index as a pinned read sees it: the dataset at the version `c` names.
pub async fn checkout(dir: &str, c: &Coverage, part: &str) -> Result<Option<Dataset>> {
    let Some(v) = c.versions.get(part) else {
        return Ok(None);
    };
    let ds = crate::search::open_ds(&format!("{dir}/{part}.lance"))
        .await
        .with_context(|| format!("open {dir}/{part}.lance"))?;
    Ok(Some(ds.checkout_version(*v).await.with_context(|| {
        format!("{dir}/{part}.lance has no version {v} any more")
    })?))
}

/// Refuse a pinned index built with another configuration than this store reads with (another embedding
/// model's vectors can't be searched with this one's).
pub fn check_config(c: &Coverage, now: &IndexConfig) -> Result<()> {
    anyhow::ensure!(
        c.config.dir == now.dir,
        "the snapshot's {} index was built as {} and this server reads {}: serve it with that configuration",
        now.kind,
        c.config.dir,
        now.dir
    );
    Ok(())
}

/// A scope's index is two Lance datasets of fact rows, split by whether the row's validity
/// window is still open, plus `files`, the list of data-file keys fully indexed. "Current"
/// queries search only `open`, so a hub entity's hundreds of superseded versions (in `closed`)
/// can neither crowd out its current fact nor force a full-table prefilter. Beside them, an
/// update in progress records how far it got (`staged`, see [`Progress`]); reads never look at it.
#[derive(Clone, Copy, PartialEq)]
enum Part {
    Open,
    Closed,
    Files,
}

const PARTS: [Part; 3] = [Part::Open, Part::Closed, Part::Files];

impl Part {
    fn uri(self, base: &str, scope: &Scope) -> String {
        let name = match self {
            Part::Open => "open",
            Part::Closed => "closed",
            Part::Files => "files",
        };
        format!("{}/{name}.lance", base_uri(base, scope))
    }
}

/// The URI prefix holding a scope's index in a store's index location.
pub fn index_uri(store: &Store, scope: &Scope) -> String {
    base_uri(store.index_base(), scope)
}

fn lance_schema() -> Arc<Schema> {
    let s = |n: &str| Field::new(n, DataType::Utf8, false);
    Arc::new(Schema::new(vec![
        // The Iceberg data file's key (path plus delete files) these rows came from.
        s("data_file"),
        s("commit_id"),
        s("fact_id"),
        s("entity"),
        s("predicate"),
        s("value"),
        Field::new("valid_from", DataType::Int64, true),
        Field::new("valid_to", DataType::Int64, true),
        s("sources"),
        s("text"),
        s("ekey"),
        s("vkey"),
        Field::new("quote", DataType::Utf8, true),
        vec_field(),
        // Typed columns (null in untyped scopes), each with a scalar index for filters.
        Field::new("entity_type", DataType::Utf8, true),
        Field::new("value_type", DataType::Utf8, true),
        Field::new("value_num", DataType::Float64, true),
        Field::new("value_ts", DataType::Int64, true),
        Field::new("value_ref", DataType::Utf8, true),
        // Where the fact comes from and how its time is known.
        Field::new("unit", DataType::Utf8, true),
        Field::new("cite", DataType::Utf8, true),
        Field::new("time_basis", DataType::Utf8, true),
        Field::new("tags", DataType::Utf8, true),
        // The keyword-searched fields (FTS_FIELDS).
        s("t_value"),
        s("t_subject"),
        s("t_quote"),
        s("t_pred"),
        s("t_date"),
    ]))
}

pub fn files_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![Field::new(
        "data_file",
        DataType::Utf8,
        false,
    )]))
}

/// The text a fact is searched by.
fn fact_text(entity: &str, predicate: &str, value: &str) -> String {
    format!("{entity} {} {value}", predicate.replace('_', " "))
}

/// Facts table rows of one Iceberg data file, as Lance rows, and how many of their texts had a vector to reuse.
/// `reuse` holds vectors already computed for fact texts; only other texts are embedded (or found in the
/// embedding cache, `embed_cache`).
async fn to_lance(data_file: &str, b: &RecordBatch, reuse: &Reuse) -> Result<(RecordBatch, usize)> {
    let col = |n: &str| crate::strings(b, n);
    let (ent, pred, val) = (col("entity"), col("predicate"), col("value"));
    let n = b.num_rows();
    let text: Vec<String> = (0..n)
        .map(|i| fact_text(&ent[i], &pred[i], &val[i]))
        .collect();
    let t_value: Vec<String> = val.iter().map(|v| normalize_text(v)).collect();
    let t_subject: Vec<String> = ent.iter().map(|e| subject_text(e)).collect();
    let t_pred: Vec<String> = pred.iter().map(|p| p.replace('_', " ")).collect();
    let ekey: Vec<String> = (0..n)
        .map(|i| Key::Entity(&ent[i], &pred[i]).text())
        .collect();
    let vkey: Vec<String> = (0..n)
        .map(|i| Key::Value(&pred[i], &val[i]).text())
        .collect();
    let known = reuse.vectors(&text).await;
    let missing: Vec<String> = text
        .iter()
        .filter(|t| !known.contains_key(*t))
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let fresh: HashMap<String, Vec<f32>> = missing
        .iter()
        .cloned()
        .zip(embed_cache::embed_cached(missing.clone()).await?)
        .collect();
    let vecs: Vec<Vec<f32>> = text
        .iter()
        .map(|t| known.get(t).or_else(|| fresh.get(t)).unwrap().clone())
        .collect();
    let vec: ArrayRef = Arc::new(FixedSizeListArray::try_new(
        Arc::new(Field::new("item", DataType::Float32, true)),
        dims(),
        Arc::new(Float32Array::from(vecs.concat())),
        None,
    )?);
    let ts = |n: &str| -> ArrayRef { Arc::new(Int64Array::from(crate::timestamps(b, n))) };
    let st = |v: Vec<String>| -> ArrayRef { Arc::new(StringArray::from(v)) };
    let batch = RecordBatch::try_new(
        lance_schema(),
        vec![
            st(vec![data_file.to_string(); b.num_rows()]),
            st(col("commit_id")),
            st(col("fact_id")),
            st(ent),
            st(pred),
            st(val),
            ts("valid_from"),
            ts("valid_to"),
            st(col("source_episodes")),
            st(text),
            st(ekey),
            st(vkey),
            Arc::new(StringArray::from(crate::optional_strings(b, "quote"))),
            vec,
            Arc::new(StringArray::from(crate::optional_strings(b, "entity_type"))),
            Arc::new(StringArray::from(crate::optional_strings(b, "value_type"))),
            Arc::new(arrow_array::Float64Array::from(crate::optional_f64(
                b,
                "value_num",
            ))),
            Arc::new(Int64Array::from(crate::optional_micros(b, "value_ts"))),
            Arc::new(StringArray::from(crate::optional_strings(b, "value_ref"))),
            Arc::new(StringArray::from(crate::optional_strings(b, "unit"))),
            Arc::new(StringArray::from(crate::optional_strings(b, "cite"))),
            Arc::new(StringArray::from(crate::optional_strings(b, "time_basis"))),
            Arc::new(StringArray::from(crate::optional_strings(b, "tags"))),
            st(t_value),
            st(t_subject),
            st(crate::optional_strings(b, "quote")
                .into_iter()
                .map(|q| normalize_text(&q.unwrap_or_default()))
                .collect()),
            st(t_pred),
            st(crate::timestamps(b, "valid_from")
                .into_iter()
                .map(|t| date_text(t.unwrap_or(0)))
                .collect()),
        ],
    )?;
    Ok((batch, known.len()))
}

async fn open_part(base: &str, scope: &Scope, part: Part) -> Result<Option<Dataset>> {
    crate::cell::check_namespace(&scope.namespace())?;
    match crate::search::open_ds(&part.uri(base, scope)).await {
        Ok(d) => Ok(Some(d)),
        Err(lance::Error::DatasetNotFound { .. }) => Ok(None),
        Err(e) => Err(e).context("open search index"),
    }
}

/// The index's datasets and the file keys it fully holds. The handles live for the whole
/// process so their sessions keep index caches; after the staleness window only the latest
/// version of `files` is checked, and everything is re-opened only when an `update` wrote.
struct OpenIndex {
    checked: std::time::Instant,
    files_version: Option<u64>,
    open: Option<Dataset>,
    closed: Option<Dataset>,
    held: Arc<HashSet<String>>,
    /// Hashes of the (entity, predicate) keys with more than one row in `open` and `closed` (see
    /// [`OpenIndex::multi_keys`]), read on first use.
    multi: Arc<tokio::sync::OnceCell<Arc<HashSet<u64>>>>,
}

static OPEN: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Arc<OpenIndex>>>> =
    std::sync::LazyLock::new(Default::default);

impl OpenIndex {
    /// No index: every row is read from the table.
    fn none() -> Self {
        OpenIndex {
            checked: std::time::Instant::now(),
            files_version: None,
            open: None,
            closed: None,
            held: Arc::default(),
            multi: Arc::default(),
        }
    }

    /// Of the table's `current` files, those whose index rows a read uses: the ones `files` marks. Rows of
    /// a file an update has written but not marked yet (it was interrupted, or is still running) aren't
    /// served; the file is read directly, with the rest of the tail.
    fn trusted(&self, mut current: HashSet<String>) -> HashSet<String> {
        current.retain(|k| self.held.contains(k));
        current
    }

    /// Hashes of the keys (`ekey`) with more than one row in the index: of the keys it holds, the only ones
    /// a newer version can supersede. A key with one row and none unindexed has one version. Read once per
    /// index version; a hash collision only marks a key that has one row as having more.
    async fn multi_keys(&self) -> Result<Arc<HashSet<u64>>> {
        let keys = self
            .multi
            .get_or_try_init(|| async {
                let (mut seen, mut multi) = (HashSet::new(), HashSet::new());
                for ds in [&self.open, &self.closed].into_iter().flatten() {
                    let batches: Vec<RecordBatch> = ds
                        .scan()
                        .project(&["ekey"])?
                        .try_into_stream()
                        .await?
                        .try_collect()
                        .await?;
                    for b in &batches {
                        let a = b.column(0).as_string::<i32>();
                        for i in 0..a.len() {
                            let h = key_hash(a.value(i));
                            if !seen.insert(h) {
                                multi.insert(h);
                            }
                        }
                    }
                }
                anyhow::Ok(Arc::new(multi))
            })
            .await?;
        Ok(keys.clone())
    }
}

fn key_hash(ekey: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ekey.hash(&mut h);
    h.finish()
}

/// The dataset version of each part of the scope's facts index that reads use now; empty without an index.
pub async fn current_versions(store: &Store, scope: &Scope) -> Result<BTreeMap<String, u64>> {
    let o = open_cached(store, scope).await?;
    let mut out = BTreeMap::new();
    if let Some(v) = o.files_version {
        out.insert("files".to_string(), v);
    }
    for (name, d) in [("open", &o.open), ("closed", &o.closed)] {
        if let Some(d) = d {
            out.insert(name.to_string(), d.version().version);
        }
    }
    Ok(out)
}

/// The index a read uses: the current one, or for a pinned read the versions its snapshot recorded (or
/// none, when it recorded none).
async fn index_for(store: &Store, scope: &Scope, pin: Option<&IndexPin>) -> Result<Arc<OpenIndex>> {
    let Some(pin) = pin else {
        return open_cached(store, scope).await;
    };
    let Some(c) = &pin.facts else {
        return Ok(Arc::new(OpenIndex::none()));
    };
    check_config(c, &config())?;
    let dir = base_uri(store.index_base(), scope);
    // Pinned versions never change: opened once per process.
    let key = format!("{dir}/@{}", serde_json::to_string(&c.versions)?);
    if let Some(o) = OPEN.lock().unwrap().get(&key).cloned() {
        return Ok(o);
    }
    let files = checkout(&dir, c, "files").await?;
    let held = match &files {
        Some(f) => covered(f).await?,
        None => HashSet::new(),
    };
    let o = Arc::new(OpenIndex {
        checked: std::time::Instant::now(),
        files_version: files.as_ref().map(|f| f.version().version),
        open: checkout(&dir, c, "open").await?,
        closed: checkout(&dir, c, "closed").await?,
        held: Arc::new(held),
        multi: Arc::default(),
    });
    OPEN.lock().unwrap().insert(key, o.clone());
    Ok(o)
}

async fn load_index(base: &str, scope: &Scope) -> Result<OpenIndex> {
    let (open, closed, files) = tokio::try_join!(
        open_part(base, scope, Part::Open),
        open_part(base, scope, Part::Closed),
        open_part(base, scope, Part::Files)
    )?;
    let held = match &files {
        Some(f) => covered(f).await?,
        None => HashSet::new(),
    };
    // `files` is written last, so without it the first build is still under way: `open` may exist with
    // no full-text index yet. Nothing is trusted from the parts until it lands; the facts are read directly.
    let (open, closed) = match files {
        Some(_) => (open, closed),
        None => (None, None),
    };
    let t0 = std::time::Instant::now();
    futures::future::join_all(open.iter().chain(&closed).map(warm_keywords)).await;
    if t0.elapsed() > std::time::Duration::from_secs(1) {
        eprintln!(
            "memstore: keyword index of {} loaded in {} ms before reads use it",
            scope.namespace(),
            t0.elapsed().as_millis()
        );
    }
    Ok(OpenIndex {
        checked: std::time::Instant::now(),
        files_version: files.as_ref().map(|f| f.version().version),
        open,
        closed,
        held: Arc::new(held),
        multi: Arc::default(),
    })
}

/// Load a part's keyword postings into the index cache, as one read of every file at once
/// (`AMPUP_MEMORY_INDEX_PREWARM=0` leaves them to the searches). A keyword search loads the postings it needs a
/// term, field and file at a time, so on a part an update or merge just rewrote a long query waited for one
/// object-storage round trip after another (a production deployment's facts: 10 to 30 s); loaded here, while reads still use the
/// index they have, it takes 20 to 40 ms. Parts already loaded return at once. Best effort: what doesn't load is
/// loaded by the searches that need it.
async fn warm_keywords(ds: &Dataset) {
    let names: Vec<&str> = FTS_FIELDS.iter().map(|(col, _, _)| *col).collect();
    warm_keyword_indexes(ds, &names).await;
}

/// [`warm_keywords`] for the keyword indexes `names` of `ds` (a passage index has one, `passages_text`).
pub(crate) async fn warm_keyword_indexes(ds: &Dataset, names: &[&str]) {
    use lance_index::traits::{FtsPrewarmMode, FtsPrewarmOptions, PrewarmOptions};
    if index_knob("PREWARM", 1) == 0 {
        return;
    }
    let options = PrewarmOptions::Fts(
        FtsPrewarmOptions::new()
            .with_position(knob("PHRASE", 0.0) > 0.0)
            .with_mode(FtsPrewarmMode::BestEffort),
    );
    let warmed = futures::future::join_all(
        names
            .iter()
            .map(|name| ds.prewarm_index_with_options(name, &options)),
    )
    .await;
    for (name, r) in names.iter().zip(warmed) {
        if let Err(e) = r {
            eprintln!(
                "memstore: keyword index {name} of {} left to load on use: {e}",
                ds.uri()
            );
        }
    }
}

/// The scope's current index, opened once per process. Past the store's staleness window the next read
/// still gets it at once and starts a check for a newer one in the background (one at a time per index),
/// as cached tables are refreshed: opening `files` is a few object-store round trips, which every read
/// would otherwise wait for. A reader waits only for the first open, or once the copy is older than the
/// hard staleness limit, and then for the check under way ([`recheck`] runs one at a time). This
/// process's own updates drop the copy, so it reads them at once.
async fn open_cached(store: &Store, scope: &Scope) -> Result<Arc<OpenIndex>> {
    let base = store.index_base();
    let uri = base_uri(base, scope);
    let cached = OPEN.lock().unwrap().get(&uri).cloned();
    if let Some(o) = &cached {
        let age = o.checked.elapsed();
        if age < store.max_staleness() {
            return Ok(o.clone());
        }
        if RECHECKING.lock().unwrap().insert(uri.clone()) {
            let (base, scope, o) = (base.to_string(), scope.clone(), o.clone());
            tokio::spawn(async move {
                if let Err(e) = recheck(&base, &scope, Some(o)).await {
                    eprintln!(
                        "memstore: checking the search index of {scope} failed (searching the one open): {e:#}"
                    );
                }
                RECHECKING.lock().unwrap().remove(&base_uri(&base, &scope));
            });
        }
        if age < store.hard_staleness() {
            return Ok(o.clone());
        }
    }
    recheck(base, scope, cached).await
}

/// Indexes whose re-check is under way (see [`open_cached`]).
static RECHECKING: std::sync::LazyLock<std::sync::Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

/// One re-check of an index at a time (see [`recheck`]).
static TURNS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    std::sync::LazyLock::new(Default::default);

/// Bumped whenever open indexes are dropped (an update wrote, an index was deleted): a re-check that began
/// before doesn't put back what it read then.
static DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn drop_open(drop: impl Fn(&str) -> bool) {
    let mut open = OPEN.lock().unwrap();
    DROPPED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    open.retain(|k, _| !drop(k));
}

/// The scope's index as it is now: `cached`'s handles when `files` hasn't changed since, else opened again.
/// One at a time per index: one that waited for its turn takes what the one before it opened, if that one
/// finished after it asked, rather than open the same version again. Opening a new version loads its keyword
/// postings first ([`warm_keywords`]), and readers past the hard staleness limit each ran a check of their
/// own: on a production deployment one day, with agent sessions searching it ~20 times a minute, 70 such loads overlapped,
/// took a median 68 s and up to 20 minutes, and every search of the org waited on one until its caller gave
/// up.
async fn recheck(
    base: &str,
    scope: &Scope,
    cached: Option<Arc<OpenIndex>>,
) -> Result<Arc<OpenIndex>> {
    let uri = base_uri(base, scope);
    let asked = std::time::Instant::now();
    let turn = TURNS
        .lock()
        .unwrap()
        .entry(uri.clone())
        .or_default()
        .clone();
    let _turn = turn.lock().await;
    let current = OPEN.lock().unwrap().get(&uri).cloned();
    if let Some(o) = &current
        && o.checked >= asked
    {
        return Ok(o.clone());
    }
    let cached = current.or(cached);
    let dropped = DROPPED.load(std::sync::atomic::Ordering::SeqCst);
    let unchanged = match &cached {
        Some(o) => match open_part(base, scope, Part::Files).await? {
            Some(f) => Some(f.version().version) == o.files_version,
            None => o.files_version.is_none(),
        },
        None => false,
    };
    let fresh = Arc::new(match (cached, unchanged) {
        (Some(o), true) => OpenIndex {
            checked: std::time::Instant::now(),
            files_version: o.files_version,
            open: o.open.clone(),
            closed: o.closed.clone(),
            held: o.held.clone(),
            multi: o.multi.clone(),
        },
        _ => load_index(base, scope).await?,
    });
    let mut open = OPEN.lock().unwrap();
    if DROPPED.load(std::sync::atomic::Ordering::SeqCst) == dropped {
        open.insert(uri, fresh.clone());
    }
    Ok(fresh)
}

/// File keys a dataset holds rows for.
pub async fn covered(ds: &Dataset) -> Result<HashSet<String>> {
    let batches: Vec<RecordBatch> = ds
        .scan()
        .project(&["data_file"])?
        .try_into_stream()
        .await?
        .try_collect()
        .await?;
    Ok(batches
        .iter()
        .flat_map(|b| {
            let a = b.column(0).as_string::<i32>();
            (0..a.len())
                .map(|i| a.value(i).to_string())
                .collect::<Vec<_>>()
        })
        .collect())
}

pub fn in_list(keys: &[String]) -> String {
    let list = keys
        .iter()
        .map(|k| format!("'{}'", k.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    format!("data_file IN ({list})")
}

/// An index setting from `AMPUP_MEMORY_INDEX_<name>`, else `default`.
/// The one Lance session every index dataset in this process opens with, so its index and metadata caches
/// are bounded once: `AMPUP_MEMORY_INDEX_CACHE_BYTES` (default 1 GiB) and `AMPUP_MEMORY_INDEX_METADATA_BYTES`
/// (default 256 MiB). Opened on its own, each dataset got a session of its own, of up to 6 GiB and 1 GiB:
/// with every scope's index held open, a server's memory had no bound, and an 8 GiB pod was
/// OOM-killed once a production deployment's indexes grew. The index cache keeps half its room for what was just read
/// ([`cache`]).
pub(crate) fn session() -> Arc<lance::session::Session> {
    SESSION.read().unwrap().clone()
}

static SESSION: std::sync::LazyLock<std::sync::RwLock<Arc<lance::session::Session>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(new_session()));

fn new_session() -> Arc<lance::session::Session> {
    let (cache, metadata) = session_budget();
    let hot = index_knob("CACHE_HOT", 50).min(100) as f64 / 100.0;
    Arc::new(lance::session::Session::with_index_cache_backend(
        Arc::new(cache::IndexCache::new(cache, hot)),
        metadata,
        Default::default(),
    ))
}

/// The shared session's bounds (`AMPUP_MEMORY_INDEX_CACHE_BYTES`, `AMPUP_MEMORY_INDEX_METADATA_BYTES`): 1 GiB of index
/// cache and 256 MiB of metadata, or a tenth and a fortieth of the container's memory if those are less
/// (`crate::metrics::sized`).
pub fn session_budget() -> (usize, usize) {
    (
        index_knob("CACHE_BYTES", crate::metrics::sized(1 << 30, 0.1)),
        index_knob("METADATA_BYTES", crate::metrics::sized(256 << 20, 0.025)),
    )
}

/// The vectors an index update may reuse: those of the rows it is about to clear (files rewritten, re-added or
/// gone), by text, read from the index as it was before the clear. A rewrite mostly carries the same texts, and
/// embedding is the slow part of an update. Only where each text's row is is held (a hash of the text and the
/// row's id, under 40 bytes a row); each chunk of the update takes the vectors it needs as it is written.
/// Holding the vectors themselves OOM-killed the 8 GiB server, and capping them at 1 GiB had a production
/// facts index, 432k rows, embed 285k of them again after each nightly rewrite: 20 minutes of model calls, the
/// index's turn held throughout.
pub(crate) struct Reuse {
    /// The parts as they were before the clear: their rows stay readable there for the whole update (old
    /// versions go only once an update is done, `drop_old_versions`).
    parts: Vec<Dataset>,
    /// Per text (the first 16 bytes of its SHA-256), its part and row id.
    at: HashMap<[u8; 16], (usize, u64)>,
}

impl Reuse {
    /// Where the texts of `parts`' rows of the files `clear` are, before they are cleared.
    pub(crate) async fn of(parts: Vec<Dataset>, clear: &[String]) -> Result<Reuse> {
        let mut at = HashMap::new();
        if !clear.is_empty() {
            for (p, d) in parts.iter().enumerate() {
                for keys in clear.chunks(500) {
                    let mut scan = d.scan();
                    scan.filter(&in_list(keys))?
                        .project(&["text"])?
                        .with_row_id();
                    let mut batches = scan.try_into_stream().await?;
                    while let Some(b) = batches.try_next().await? {
                        let t = b
                            .column_by_name("text")
                            .context("no text")?
                            .as_string::<i32>();
                        let ids = b
                            .column_by_name("_rowid")
                            .context("no _rowid")?
                            .as_primitive::<arrow_array::types::UInt64Type>();
                        for i in 0..b.num_rows() {
                            at.entry(text_hash(t.value(i))).or_insert((p, ids.value(i)));
                        }
                    }
                }
            }
        }
        Ok(Reuse { parts, at })
    }

    /// The vectors it holds of `texts`, by text. A read that fails leaves its texts out (they are embedded): reuse
    /// saves model calls, it never fails an update.
    pub(crate) async fn vectors<'a>(
        &self,
        texts: impl IntoIterator<Item = &'a String>,
    ) -> HashMap<String, Vec<f32>> {
        let mut wanted: Vec<Vec<(u64, String)>> = vec![vec![]; self.parts.len()];
        let mut seen = HashSet::new();
        for t in texts {
            if seen.insert(t)
                && let Some(&(p, id)) = self.at.get(&text_hash(t))
            {
                wanted[p].push((id, t.clone()));
            }
        }
        // In row order, a run of nearby rows a take, several takes at once: a take of rows spread over the part
        // waits on one object-store read after another (5,000 of a production deployment's from GCS: 13 s alone, 4 s as 16 at once).
        let mut takes: Vec<(Dataset, Vec<(u64, String)>)> = vec![];
        for (d, mut rows) in self.parts.iter().zip(wanted) {
            rows.sort_unstable_by_key(|r| r.0);
            while !rows.is_empty() {
                let rest = rows.split_off(rows.len().min(512));
                takes.push((d.clone(), std::mem::replace(&mut rows, rest)));
            }
        }
        use futures::StreamExt;
        let taken: Vec<_> = futures::stream::iter(takes)
            .map(|(d, rows)| async move {
                let ids: Vec<u64> = rows.iter().map(|r| r.0).collect();
                let got = d
                    .take_rows(
                        &ids,
                        lance::dataset::ProjectionRequest::from_columns(
                            ["text", "vec"],
                            d.schema(),
                        ),
                    )
                    .await;
                (d, rows, got)
            })
            .buffer_unordered(16)
            .collect()
            .await;
        let mut out = HashMap::new();
        for (d, rows, got) in taken {
            let b = match got {
                Ok(b) if b.num_rows() == rows.len() => b,
                Ok(b) => {
                    eprintln!(
                        "memstore: {}: took {} rows for {} known vectors; embedding those texts",
                        d.uri(),
                        b.num_rows(),
                        rows.len()
                    );
                    continue;
                }
                Err(e) => {
                    eprintln!(
                        "memstore: {}: known vectors not read ({e}); embedding those texts",
                        d.uri()
                    );
                    continue;
                }
            };
            let (t, v) = (
                b.column(0).as_string::<i32>(),
                b.column(1).as_fixed_size_list(),
            );
            for (i, (_, text)) in rows.iter().enumerate() {
                // A hash shared by two texts: the other's vector is no use.
                if t.value(i) == text.as_str() {
                    let row = v.value(i);
                    let f = row.as_primitive::<arrow_array::types::Float32Type>();
                    out.insert(text.clone(), f.values().to_vec());
                }
            }
        }
        out
    }
}

fn text_hash(text: &str) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())[..16].try_into().unwrap()
}

/// The most bytes of embeddings searched without a vector index that reads keep (`AMPUP_MEMORY_INDEX_FLAT_BYTES`,
/// see `rows::nearest_flat`): 1 GiB, or a tenth of the container's memory if that is less.
pub fn flat_budget() -> usize {
    static BUDGET: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| index_knob("FLAT_BYTES", crate::metrics::sized(1 << 30, 0.1)));
    *BUDGET
}

/// `params` with the memory a full keyword-index build may use bounded to a tenth of the container's memory.
/// Lance otherwise lets each build worker hold 2 GiB before it writes a partition. A build-time setting only:
/// the index's stored settings, and with them its directory, don't change.
pub(crate) fn fts_build(params: InvertedIndexParams) -> InvertedIndexParams {
    match crate::metrics::memory_limit() {
        Some(limit) => params.memory_limit_mb((limit / 10) >> 20),
        None => params,
    }
}

/// Replace the shared session with an empty one: an index deleted for good (forgotten memory among it) must
/// leave memory with it, and one rebuilt at the same path restarts its versions, which the old caches would
/// answer for. Lance has no cache clear; datasets opened since get the new session.
fn new_caches() {
    *SESSION.write().unwrap() = new_session();
}

/// What the search side holds in memory, for the server's memory log: the shared Lance session's caches,
/// open fact and passage indexes, decoded index rows, flat-scanned embeddings and query vectors.
pub fn memory_report() -> String {
    let (passage_open, passage_rows) = passages::held();
    let (flat_parts, flat_bytes) = rows::flat_held();
    format!(
        "lance caches {} MB, open indexes {} fact + {passage_open} passage, index rows {} fact + {} keyed + \
         {passage_rows} passage, flat vectors {flat_parts} parts {} MB, query vectors {}",
        session().size_bytes() >> 20,
        OPEN.lock().unwrap().len(),
        FACT_ROWS.len(),
        KEYED_ROWS.len(),
        flat_bytes >> 20,
        TAIL_CACHE.lock().unwrap().len(),
    )
}

/// Open the index dataset at `uri` in the shared [`session`]. Every index read and write opens through here
/// (a write first, to append or find there is none), so another cell's index is refused here
/// ([`crate::cell::check_uri`]) and never read or written.
pub(crate) async fn open_ds(uri: &str) -> lance::Result<Dataset> {
    crate::cell::check_uri(uri).map_err(|e| lance::Error::invalid_input_source(Box::new(e)))?;
    lance::dataset::builder::DatasetBuilder::from_uri(uri)
        .with_session(session())
        .load()
        .await
}

pub(crate) fn index_knob(name: &str, default: usize) -> usize {
    std::env::var(format!("AMPUP_MEMORY_INDEX_{name}"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Whether an index write failed for a reason trying again can clear: a storage request that broke off or
/// timed out. Neither Lance nor object_store retries an upload part whose connection fails with an error
/// they can't classify (a TLS record error mid-PUT, `BadRecordMac`, is one), so one such part used to fail
/// a whole update. A missing object or a refused one is not transient. A commit another writer's preempted
/// (a Lance "retryable commit conflict": that commit changed the same files or indexes first) is, run again
/// from the newest version ([`at_newest`]): a server updates a scope's index one update at a time, but
/// `memstore index` by hand can still commit beside it, and the nightly reindex lost to one was left to the
/// next refresh.
fn transient(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::{AlreadyExists, InvalidInput, NotFound, PermissionDenied};
    let mut io = false;
    for c in e.chain() {
        if let Some(l) = c.downcast_ref::<lance::Error>() {
            if matches!(l, lance::Error::RetryableCommitConflict { .. }) {
                return true;
            }
            io |= matches!(l, lance::Error::IO { .. } | lance::Error::Timeout { .. });
        }
        if let Some(i) = c.downcast_ref::<std::io::Error>() {
            if matches!(
                i.kind(),
                NotFound | PermissionDenied | AlreadyExists | InvalidInput
            ) {
                return false;
            }
            io = true;
        }
    }
    io
}

/// Run an index write (`what`, for the log) again while it fails transiently ([`transient`]), up to
/// `AMPUP_MEMORY_INDEX_RETRIES` (default 5) more times, waiting 2 s, then twice as long each time up to a minute.
/// Each write it runs commits whole or not at all (a Lance commit), so running one again is safe.
pub(crate) async fn retrying<T, F, Fut>(what: &str, mut write: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let retries = index_knob("RETRIES", 5);
    let mut wait = std::time::Duration::from_secs(2);
    let mut attempt = 0;
    loop {
        match write().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < retries && transient(&e) => {
                attempt += 1;
                eprintln!(
                    "memstore: {what} failed ({}); trying again in {}s ({attempt} of {retries})",
                    e.root_cause(),
                    wait.as_secs()
                );
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(std::time::Duration::from_secs(60));
            }
            Err(e) => return Err(e),
        }
    }
}

/// `ds` checked out at its newest version, where a write [`retrying`] runs starts: after another writer's
/// commit preempted the last try, the version that try read is no longer one a commit can follow.
pub(crate) async fn at_newest(mut ds: Dataset) -> Result<Dataset> {
    ds.checkout_latest().await?;
    Ok(ds)
}

/// Append `batches` to the Lance dataset at `uri`, creating it if there is none, trying again on a
/// transient failure ([`retrying`]). An append whose commit landed though its reply was lost isn't made
/// twice: a dataset that moved past the version the attempt started from took it.
pub(crate) async fn append_rows(
    uri: &str,
    batches: Vec<RecordBatch>,
    schema: Arc<Schema>,
) -> Result<Option<Dataset>> {
    let open = || async {
        match crate::search::open_ds(uri).await {
            Ok(d) => Ok(Some(d)),
            Err(lance::Error::DatasetNotFound { .. }) => Ok(None),
            Err(e) => Err(anyhow::Error::from(e).context(format!("open {uri}"))),
        }
    };
    if batches.iter().all(|b| b.num_rows() == 0) {
        return open().await;
    }
    // The version the first attempt started from.
    let before = std::sync::Mutex::new(None::<Option<u64>>);
    retrying(&format!("append to {uri}"), || {
        let (batches, schema, before) = (batches.clone(), schema.clone(), &before);
        async move {
            let ds = open().await?;
            let version = ds.as_ref().map(|d| d.version().version);
            if *before.lock().unwrap().get_or_insert(version) != version {
                return Ok(ds);
            }
            let reader = RecordBatchIterator::new(batches.into_iter().map(Ok), schema);
            Ok(Some(match ds {
                Some(mut d) => {
                    d.append(reader, None).await?;
                    d
                }
                None => {
                    let params = WriteParams {
                        mode: WriteMode::Create,
                        session: Some(session()),
                        ..Default::default()
                    };
                    Dataset::write(reader, uri, Some(params)).await?
                }
            }))
        }
    })
    .await
}

/// Build (or rebuild) the index `name` on `cols` of `ds`, trying again on a transient failure; an index
/// that already covers every fragment (built by an update that was interrupted after it) is kept.
pub(crate) async fn build_index(
    ds: &mut Dataset,
    cols: &[&str],
    index_type: IndexType,
    name: &str,
    params: &dyn lance_index::IndexParams,
) -> Result<()> {
    let indices = ds.load_indices().await?;
    let mine: Vec<_> = indices.iter().filter(|i| i.name == name).collect();
    let covered = |id: u64| {
        u32::try_from(id).is_ok_and(|id| {
            mine.iter()
                .any(|i| i.fragment_bitmap.as_ref().is_some_and(|b| b.contains(id)))
        })
    };
    // A vector index of another kind than `vec_index` builds is rebuilt, not extended.
    let stale = !mine.is_empty()
        && matches!(index_type, IndexType::Vector)
        && !ds
            .index_statistics(name)
            .await
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .is_some_and(|s| s["index_type"] == VEC_INDEX_KIND);
    if !stale && !mine.is_empty() && ds.fragments().iter().all(|f| covered(f.id)) {
        return Ok(());
    }
    let uri = ds.uri().to_string();
    if stale {
        eprintln!("memstore: rebuilding index {name} of {uri} as {VEC_INDEX_KIND}");
    } else if !mine.is_empty() {
        // Index only the new rows, into a delta beside the existing index. A rebuild trains on every row:
        // for a production deployment's passages (1.2 GiB of vectors) that OOM-killed the 8 GiB server each update.
        // Deltas accumulate, one per update, until `tidy` merges them.
        return retrying(&format!("index {name} of {uri} (new rows)"), || {
            let ds = ds.clone();
            async move {
                let mut ds = at_newest(ds).await?;
                ds.optimize_indices(
                    &lance_index::optimize::OptimizeOptions::append()
                        .index_names(vec![name.into()]),
                )
                .await?;
                Ok(ds)
            }
        })
        .await
        .map(|d| *ds = d);
    }
    retrying(&format!("index {name} of {uri}"), || {
        let ds = ds.clone();
        async move {
            let mut ds = at_newest(ds).await?;
            ds.create_index(cols, index_type, Some(name.into()), params, true)
                .await?;
            Ok(ds)
        }
    })
    .await
    .map(|d| *ds = d)
}

/// How much [`tidy`] may rewrite: an update's, only what the updates since the last night added; the nightly
/// window's, everything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tidy {
    Update,
    Night,
}

/// Keeps an index part quick to search as updates add to it. Every update appends a few small files and a
/// delta to each of its indexes, and a search reads all of them (a production deployment's passages: 20 deltas over 247
/// files, 15 s a search). Past `AMPUP_MEMORY_INDEX_MAX_FRAGMENTS` files (32) they are compacted into few, and an
/// index past `AMPUP_MEMORY_INDEX_MAX_DELTAS` deltas (4) has its deltas merged. A merge keeps the vector index's
/// partitions (no training, unlike a rebuild) and compaction runs one task at a time, so neither needs the
/// memory a rebuild does.
///
/// An update's tidy leaves each index's oldest segment, which the last night's merged everything into, and
/// the files it covers alone: it merges the newer deltas into one beside it and compacts only files no oldest
/// segment covers. Merging the oldest segment too rewrote the whole index every few updates (a production deployment's vector
/// index of 405k facts: 1.2 GB, of its passages 1.4 GB, for a dozen new rows), and compaction then took every
/// file the merged index covered, rows and indexes rewritten again (8 GB of passages). The nightly window's
/// tidy ([`tidy_night`]) merges every index into one and compacts all its files once there are too many.
/// Returns (indexes merged, whether files were compacted).
pub(crate) async fn tidy(ds: &mut Dataset, how: Tidy) -> Result<(usize, bool)> {
    let uri = ds.uri().to_string();
    let most_files = index_knob("MAX_FRAGMENTS", 32);
    let mut compacted = false;
    let indices = ds.load_indices().await?;
    // Each index's oldest segment (segments are listed oldest first, and a merge takes the newest).
    let mut oldest = HashMap::new();
    for i in indices.iter() {
        oldest.entry(i.name.as_str()).or_insert(i);
    }
    let settled: HashSet<u32> = match how {
        Tidy::Update => oldest
            .values()
            .filter_map(|i| i.fragment_bitmap.as_ref())
            .flat_map(|b| b.iter())
            .collect(),
        Tidy::Night => HashSet::new(),
    };
    let fragments = ds.fragments().len();
    let loose = ds
        .fragments()
        .iter()
        .filter(|f| !settled.contains(&(f.id as u32)))
        .count();
    // At night, files with many deleted rows are compacted too (a file a fifth gone is still read whole).
    let deleted = how == Tidy::Night
        && ds.fragments().iter().any(|f| {
            let gone = f
                .deletion_file
                .as_ref()
                .and_then(|d| d.num_deleted_rows)
                .unwrap_or(0);
            f.physical_rows.is_some_and(|n| n > 0 && gone * 10 > n)
        });
    // Of an index's `n` segments, the newest this many are merged.
    let take = |n: usize| match how {
        Tidy::Update => n.saturating_sub(1),
        Tidy::Night => n,
    };
    // At night the merges come first: compaction can then join files every index covers alike.
    let mut merged = 0;
    if how == Tidy::Night {
        for (name, n) in segments(ds).await?.into_iter().filter(|(_, n)| *n > 1) {
            merge(ds, &uri, &name, n, take(n)).await?;
            merged += 1;
        }
    }
    if loose > most_files || (deleted && fragments > 1) {
        let t0 = std::time::Instant::now();
        let excluded: Vec<u32> = settled.iter().copied().collect();
        retrying(&format!("compact {uri}"), || {
            let ds = ds.clone();
            let excluded_fragment_ids = excluded.clone();
            async move {
                let mut ds = at_newest(ds).await?;
                let options = lance::dataset::optimize::CompactionOptions {
                    num_threads: Some(1),
                    excluded_fragment_ids,
                    ..Default::default()
                };
                lance::dataset::optimize::compact_files(&mut ds, options, None).await?;
                Ok(ds)
            }
        })
        .await
        .map(|d| *ds = d)?;
        compacted = true;
        eprintln!(
            "memstore: compacted {uri}: {fragments} files to {} in {} ms ({} settled files left as they were)",
            ds.fragments().len(),
            t0.elapsed().as_millis(),
            settled.len()
        );
    }
    if how == Tidy::Update {
        let most = index_knob("MAX_DELTAS", 4);
        for (name, n) in segments(ds).await?.into_iter().filter(|(_, n)| *n > most) {
            merge(ds, &uri, &name, n, take(n)).await?;
            merged += 1;
        }
    }
    Ok((merged, compacted))
}

/// The nightly window's tidy of a scope's facts index: rows written twice dropped, every index merged into
/// one and the files compacted once there are too many ([`Tidy::Night`]). Searches move to it as they do to
/// an update's work, with the next change to the files the index holds. Left alone while an interrupted
/// update's rows wait (the next update rolls back to its last recorded chunk). Returns (indexes merged,
/// parts compacted, rows dropped).
pub async fn tidy_night(store: &Store, scope: &Scope) -> Result<(usize, usize, usize)> {
    store.check_fence()?;
    let base = store.index_base();
    let (mut merged, mut compacted, mut dropped) = (0, 0, 0);
    if !Progress::load(&base_uri(base, scope))
        .await?
        .files()
        .is_empty()
    {
        return Ok((merged, compacted, dropped));
    }
    for part in [Part::Open, Part::Closed] {
        let Some(mut ds) = open_part(base, scope, part).await? else {
            continue;
        };
        dropped += drop_duplicates(&mut ds, FACT_KEY).await?;
        let (m, c) = tidy(&mut ds, Tidy::Night).await?;
        merged += m;
        compacted += usize::from(c);
    }
    if merged + compacted + dropped > 0 {
        drop_open(|k| k == base_uri(base, scope));
    }
    Ok((merged, compacted, dropped))
}

/// How many segments each index of `ds` has.
async fn segments(ds: &Dataset) -> Result<BTreeMap<String, usize>> {
    let mut segments: BTreeMap<String, usize> = BTreeMap::new();
    for i in ds.load_indices().await?.iter() {
        *segments.entry(i.name.clone()).or_default() += 1;
    }
    Ok(segments)
}

/// Merge the newest `take` of index `name`'s `n` segments into one.
async fn merge(ds: &mut Dataset, uri: &str, name: &str, n: usize, take: usize) -> Result<()> {
    let t0 = std::time::Instant::now();
    retrying(&format!("merge index {name} of {uri}"), || {
        let ds = ds.clone();
        let name = name.to_string();
        async move {
            let mut ds = at_newest(ds).await?;
            ds.optimize_indices(
                &lance_index::optimize::OptimizeOptions::merge(take).index_names(vec![name]),
            )
            .await?;
            Ok(ds)
        }
    })
    .await
    .map(|d| *ds = d)?;
    eprintln!(
        "memstore: merged index {name} of {uri}: {take} of {n} segments in {} ms",
        t0.elapsed().as_millis()
    );
    Ok(())
}

/// The columns that tell one fact row of the index from another, and one passage row.
pub(crate) const FACT_KEY: &[&str] = &["data_file", "commit_id", "fact_id"];
pub(crate) const PASSAGE_KEY: &[&str] = &["data_file", "episode_id", "chunk"];

/// Drops every row of `ds` but the first that shares `key` with another: a copy written twice. Two updates
/// of one index that once ran at once both wrote the files new to them, and a production facts index
/// held 743k rows for 413k facts, every posting list nearly twice as long and a search's candidates spent on
/// copies. A passage update also once wrote an episode's passages for each of its rows in a file (an episode
/// written again is another row), and the nightly rewrite puts every row in one file: one production reindex wrote
/// 157k copies a night. Read once, the keys hashed (16 bytes a row), and the copies deleted by row id in
/// ranges. Returns how many went.
pub(crate) async fn drop_duplicates(ds: &mut Dataset, key: &[&str]) -> Result<usize> {
    use sha2::{Digest, Sha256};
    let mut scan = ds.scan();
    scan.with_row_id().project(key)?;
    let mut batches = scan.try_into_stream().await?;
    // Per key, the lowest row id seen; every other row of it is a copy.
    let mut first: HashMap<[u8; 16], u64> = HashMap::new();
    let mut copies: Vec<u64> = vec![];
    while let Some(b) = batches.try_next().await? {
        let ids = b
            .column_by_name("_rowid")
            .context("no _rowid")?
            .as_primitive::<arrow_array::types::UInt64Type>();
        let cols: Vec<&ArrayRef> = key
            .iter()
            .map(|k| b.column_by_name(k).context("no key column"))
            .collect::<Result<_>>()?;
        for i in 0..b.num_rows() {
            let mut h = Sha256::new();
            for c in &cols {
                match c.data_type() {
                    DataType::Int32 => h.update(
                        c.as_primitive::<arrow_array::types::Int32Type>()
                            .value(i)
                            .to_le_bytes(),
                    ),
                    _ => h.update(c.as_string::<i32>().value(i).as_bytes()),
                }
                h.update([0u8]);
            }
            let k: [u8; 16] = h.finalize()[..16].try_into()?;
            let id = ids.value(i);
            match first.entry(k) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(id);
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    copies.push(id.max(*e.get()));
                    if id < *e.get() {
                        e.insert(id);
                    }
                }
            }
        }
    }
    if copies.is_empty() {
        return Ok(0);
    }
    copies.sort_unstable();
    let mut ranges: Vec<(u64, u64)> = vec![];
    for id in &copies {
        match ranges.last_mut() {
            Some((_, end)) if *end + 1 == *id => *end = *id,
            _ => ranges.push((*id, *id)),
        }
    }
    let uri = ds.uri().to_string();
    for part in ranges.chunks(256) {
        let filter = part
            .iter()
            .map(|(a, b)| format!("(_rowid >= {a} AND _rowid <= {b})"))
            .collect::<Vec<_>>()
            .join(" OR ");
        // Row ids are where the rows are in the version read, so no retry from a newer one: a write beside
        // this one (none on a server, which updates an index one update at a time) leaves the copies to the
        // next check.
        ds.delete(&filter)
            .await
            .with_context(|| format!("drop copies in {uri}"))?;
    }
    eprintln!(
        "memstore: dropped {} rows of {uri} written twice ({} rows left)",
        copies.len(),
        ds.count_rows(None).await?
    );
    Ok(copies.len())
}

/// Deletes the versions of an index part that stopped being current more than
/// `AMPUP_MEMORY_INDEX_KEEP_VERSIONS_SECS` ago (an hour), and the files only they used: every update leaves one
/// behind, with the index files a compaction or merge replaced (a production facts index: 942 versions, 94
/// GB of index files over 2 GB of rows). A reader is pinned to the version of `part` its index manifest (in
/// `dir`) names, and reopens as the manifest moves on: every version a record of the last hour names, or the
/// one before them did, is kept, with every version made since the oldest of them. A snapshot's versions are
/// tagged (`tag_versions`) and kept whatever their age.
pub(crate) async fn drop_old_versions(ds: &Dataset, dir: &str, part: &str) {
    let keep = chrono::Duration::seconds(index_knob("KEEP_VERSIONS_SECS", 3_600) as i64);
    let cutoff = (chrono::Utc::now() - keep).timestamp_micros();
    let records = match coverage(dir).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("memstore: old versions of {} kept: {e:#}", ds.uri());
            return;
        }
    };
    // The records readers may still be pinned to: those of the last hour, and the one current before them.
    let then = records
        .iter()
        .filter(|r| r.at < cutoff)
        .max_by_key(|r| r.at)
        .map(|r| r.at);
    let Some(oldest_read) = records
        .iter()
        .filter(|r| r.at >= cutoff || Some(r.at) == then)
        .filter_map(|r| r.versions.get(part).copied())
        .min()
    else {
        return;
    };
    let policy = lance::dataset::cleanup::CleanupPolicy {
        before_version: Some(oldest_read),
        // Tagged (a snapshot's) versions are kept and the rest go, rather than the whole cleanup refused.
        error_if_tagged_old_versions: false,
        ..Default::default()
    };
    match ds.cleanup_with_policy(policy).await {
        Ok(s) if s.old_versions > 0 => eprintln!(
            "memstore: dropped {} old versions of {} ({} MB)",
            s.old_versions,
            ds.uri(),
            s.bytes_removed >> 20
        ),
        Ok(_) => {}
        // Left for the next update: what failed to go is still unused.
        Err(e) => eprintln!("memstore: old versions of {} not dropped: {e:#}", ds.uri()),
    }
}

/// [`drop_old_versions`] after an update that had nothing to write, at most once an hour per part: an index
/// no longer updated would otherwise keep what it left behind before old versions were dropped (a
/// production facts index: 942 versions, 94 GB), and listing a part's files every round costs as much as an idle
/// update.
pub(crate) async fn drop_old_versions_idle(ds: &Dataset, dir: &str, part: &str) {
    static LAST: std::sync::LazyLock<std::sync::Mutex<HashMap<String, std::time::Instant>>> =
        std::sync::LazyLock::new(Default::default);
    let every = std::time::Duration::from_secs(index_knob("KEEP_VERSIONS_SECS", 3_600) as u64);
    {
        let mut last = LAST.lock().unwrap();
        if last.get(ds.uri()).is_some_and(|t| t.elapsed() < every) {
            return;
        }
        last.insert(ds.uri().to_string(), std::time::Instant::now());
    }
    drop_old_versions(ds, dir, part).await;
}

async fn append_part(
    base: &str,
    scope: &Scope,
    part: Part,
    batches: Vec<RecordBatch>,
    schema: Arc<Schema>,
) -> Result<Option<Dataset>> {
    append_rows(&part.uri(base, scope), batches, schema).await
}

/// How far an index update got (`staged.lance`, beside the index's parts): per data file not marked yet,
/// how many of its rows are written (in the order the table reads them), and each part's version once they
/// were. An update writes a chunk of rows at a time ([`chunk_rows`]), commits it, then records it here, so
/// one that is interrupted (a failed upload, a killed process, a pod rolled out) is picked up by the next
/// after its last recorded chunk instead of reading and embedding everything again; with every row written,
/// the next goes straight to the indexes. Emptied once the update marks its files.
pub(crate) struct Progress {
    uri: String,
    written: HashMap<String, usize>,
    versions: BTreeMap<String, u64>,
}

fn progress_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("data_file", DataType::Utf8, false),
        Field::new("rows", DataType::Int64, false),
        // Each part's version after the chunk, as JSON.
        Field::new("versions", DataType::Utf8, false),
    ]))
}

/// Rows a chunk of an update writes: `AMPUP_MEMORY_INDEX_CHUNK_ROWS`, default 5,000.
pub(crate) fn chunk_rows() -> usize {
    index_knob("CHUNK_ROWS", 5_000).max(1)
}

impl Progress {
    /// The progress recorded in the index directory `dir`; none without a `staged.lance`.
    pub(crate) async fn load(dir: &str) -> Result<Progress> {
        let mut p = Progress {
            uri: format!("{dir}/staged.lance"),
            written: HashMap::new(),
            versions: BTreeMap::new(),
        };
        let ds = match crate::search::open_ds(&p.uri).await {
            Ok(d) => d,
            Err(lance::Error::DatasetNotFound { .. }) => return Ok(p),
            Err(e) => return Err(e).with_context(|| format!("open {}", p.uri)),
        };
        let batches: Vec<RecordBatch> = ds.scan().try_into_stream().await?.try_collect().await?;
        for b in &batches {
            let (files, versions) = (
                crate::strings(b, "data_file"),
                crate::strings(b, "versions"),
            );
            let rows = b
                .column_by_name("rows")
                .context("staged.lance has no rows column")?
                .as_primitive::<arrow_array::types::Int64Type>();
            for i in 0..b.num_rows() {
                let n = p.written.entry(files[i].clone()).or_default();
                *n = (*n).max(rows.value(i) as usize);
                for (part, v) in serde_json::from_str::<BTreeMap<String, u64>>(&versions[i])? {
                    let at = p.versions.entry(part).or_default();
                    *at = (*at).max(v);
                }
            }
        }
        Ok(p)
    }

    /// Rows of `file` written by recorded chunks.
    pub(crate) fn written(&self, file: &str) -> usize {
        self.written.get(file).copied().unwrap_or(0)
    }

    /// Files with rows written by recorded chunks.
    pub(crate) fn files(&self) -> HashSet<String> {
        self.written.keys().cloned().collect()
    }

    /// Undo a chunk that was committed to some of the parts (`(name, uri)`) but not recorded: each part that
    /// gained fragments since the last recorded chunk goes back to the version recorded with it (made only
    /// of whole chunks, so of each file's first `written` rows). Without a recorded chunk there is nothing to
    /// undo: the update clears every file it is about to write.
    pub(crate) async fn roll_back(&self, parts: &[(&str, String)]) -> Result<()> {
        if self.written.is_empty() {
            return Ok(());
        }
        let newest = |d: &Dataset| d.fragments().iter().map(|f| f.id).max();
        for (name, uri) in parts {
            let Ok(ds) = crate::search::open_ds(uri).await else {
                continue;
            };
            let at = match self.versions.get(*name) {
                Some(v) => Some(ds.checkout_version(*v).await?),
                None => None,
            };
            if newest(&ds) <= at.as_ref().and_then(newest) {
                continue;
            }
            eprintln!("memstore: {uri}: undoing a chunk an interrupted update didn't record");
            retrying(&format!("roll back {uri}"), || {
                let (mut ds, at) = (ds.clone(), at.clone());
                async move {
                    match at {
                        // Made by that chunk: nothing in it was recorded.
                        None => {
                            ds.delete("true").await?;
                        }
                        Some(mut at) => at.restore().await?,
                    }
                    Ok(())
                }
            })
            .await?;
        }
        Ok(())
    }

    /// Record a committed chunk: `through` is how many rows of each of its files are written now, and
    /// `parts` the parts it wrote, whose versions are recorded with it.
    pub(crate) async fn record(
        &mut self,
        through: &[(String, usize)],
        parts: &[(&str, String)],
    ) -> Result<()> {
        for (name, uri) in parts {
            if let Ok(d) = crate::search::open_ds(uri).await {
                self.versions.insert(name.to_string(), d.version().version);
            }
        }
        let versions = serde_json::to_string(&self.versions)?;
        let row = RecordBatch::try_new(
            progress_schema(),
            vec![
                Arc::new(StringArray::from_iter_values(through.iter().map(|t| &t.0))),
                Arc::new(Int64Array::from_iter_values(
                    through.iter().map(|t| t.1 as i64),
                )),
                Arc::new(StringArray::from_iter_values(
                    through.iter().map(|_| &versions),
                )),
            ],
        )?;
        append_rows(&self.uri, vec![row], progress_schema()).await?;
        for (file, n) in through {
            self.written.insert(file.clone(), *n);
        }
        Ok(())
    }

    /// Empty it, once the update has marked its files. Left behind (the update stopped in between), it
    /// names only marked files, which the next update ignores.
    pub(crate) async fn clear(&self) -> Result<()> {
        if let Ok(d) = crate::search::open_ds(&self.uri).await {
            retrying(&format!("clear {}", self.uri), || {
                let d = d.clone();
                async move { Ok(at_newest(d).await?.delete("true").await?) }
            })
            .await?;
        }
        Ok(())
    }
}

/// Split files' rows (`(file, batches)`, skipping each one's first `written(file)` rows) into chunks of at
/// most `rows` rows, in order: each chunk a list of (file, some of its rows).
pub(crate) fn chunks<'a>(
    files: impl IntoIterator<Item = (&'a String, &'a [RecordBatch])>,
    written: impl Fn(&str) -> usize,
    rows: usize,
) -> Vec<Vec<(String, RecordBatch)>> {
    let (mut out, mut chunk, mut n) = (vec![], vec![], 0);
    for (file, batches) in files {
        let mut skip = written(file);
        for b in batches {
            let mut at = skip.min(b.num_rows());
            skip -= at;
            while at < b.num_rows() {
                let take = (rows - n).min(b.num_rows() - at);
                chunk.push((file.clone(), b.slice(at, take)));
                (at, n) = (at + take, n + take);
                if n == rows {
                    out.push(std::mem::take(&mut chunk));
                    n = 0;
                }
            }
        }
    }
    if !chunk.is_empty() {
        out.push(chunk);
    }
    out
}

/// How many rows of each file a chunk brings each file's count to, given the rows `written` before it.
pub(crate) fn through(
    chunk: &[(String, RecordBatch)],
    written: impl Fn(&str) -> usize,
) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = vec![];
    for (file, b) in chunk {
        match out.last_mut() {
            Some((f, n)) if f == file => *n += b.num_rows(),
            _ => out.push((file.clone(), written(file) + b.num_rows())),
        }
    }
    out
}

/// Bring a scope's index up to date: drop rows of files no longer current, write the facts
/// table's unindexed data files into `open`/`closed` a chunk at a time ([`Progress`]), rebuild the
/// text and key indexes, then mark the files in `files` (the commit point of an index update).
/// Reads trust rows only of files `files` marks, so rows of an update in progress, or of one that
/// was interrupted, are never served. Returns (rows written, files marked).
pub async fn update(store: &Store, scope: &Scope) -> Result<(usize, usize)> {
    // A fenced store (a server's background work) stops between steps once it may no longer write.
    store.check_fence()?;
    crate::workspace::check_index(store, scope).await?;
    // The heads before the files are read: every commit reachable from them has its rows in those files.
    let heads = store.branch_heads(scope).await?;
    let base = store.index_base();
    let held = match open_part(base, scope, Part::Files).await? {
        Some(f) => covered(&f).await?,
        None => HashSet::new(),
    };
    let (current, files) = store.fact_files(scope, &held).await?;
    let parts =
        [("open", Part::Open), ("closed", Part::Closed)].map(|(n, p)| (n, p.uri(base, scope)));
    let mut progress = Progress::load(&base_uri(base, scope)).await?;
    progress.roll_back(&parts).await?;
    let resumed: usize = files.iter().map(|(k, _)| progress.written(k)).sum();
    if resumed > 0 {
        eprintln!(
            "memstore: index of {}: resuming an interrupted update, {resumed} rows already written",
            scope.namespace()
        );
    }
    // Rows of files that are gone or gained deletes must leave the index. Rows of files about
    // to be added are cleared too, unless a recorded chunk wrote them, so an update that was
    // interrupted before it recorded any is redone without duplicates.
    let mut clear: Vec<String> = held
        .iter()
        .chain(&progress.files())
        .filter(|k| !current.contains(*k))
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let stale = clear.len();
    clear.extend(
        files
            .iter()
            .filter(|(k, _)| progress.written(k) == 0)
            .map(|(k, _)| k.clone()),
    );
    // Where the vectors of the rows about to be cleared are (`Reuse`): a rewritten or re-added file mostly
    // carries the same facts. Only needed when there are files to write.
    let mut before = vec![];
    if !files.is_empty() {
        for part in [Part::Open, Part::Closed] {
            before.extend(open_part(base, scope, part).await?);
        }
    }
    let reuse = Reuse::of(before, &clear).await?;
    if !clear.is_empty() {
        store.check_fence()?;
        for part in PARTS {
            if let Some(d) = open_part(base, scope, part).await? {
                retrying(&format!("clear {}", part.uri(base, scope)), || {
                    let d = d.clone();
                    let filter = in_list(&clear);
                    async move { Ok(at_newest(d).await?.delete(&filter).await?) }
                })
                .await?;
            }
        }
    }
    // Rows written twice (two updates that once ran at once both wrote a file): the index holds more rows
    // than the table's metadata counts for the files it covers and the rows an interrupted update wrote,
    // and the copies go. The counts are free each round; only a surplus reads the keys.
    if let Some(rows) = store.fact_file_rows(scope).await? {
        let bound: Option<u64> = held
            .iter()
            .filter(|k| current.contains(*k))
            .map(|k| rows.get(k).copied())
            .sum::<Option<u64>>()
            .map(|n| n + resumed as u64);
        let mut indexed = 0;
        for part in [Part::Open, Part::Closed] {
            if let Some(d) = open_part(base, scope, part).await? {
                indexed += d.count_rows(None).await? as u64;
            }
        }
        // A surplus a look found no copies in (the table's counts can be short) isn't looked into again for a
        // day: the keys of a large index take a while to read.
        static NONE_FOUND: std::sync::LazyLock<
            std::sync::Mutex<HashMap<String, std::time::Instant>>,
        > = std::sync::LazyLock::new(Default::default);
        let dir = base_uri(base, scope);
        let looked = NONE_FOUND
            .lock()
            .unwrap()
            .get(&dir)
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(86_400));
        if let Some(bound) = bound
            && indexed > bound
            && !looked
        {
            eprintln!(
                "memstore: index of {}: {indexed} rows for at most {bound} facts, looking for rows written twice",
                scope.namespace()
            );
            store.check_fence()?;
            let mut dropped = 0;
            for part in [Part::Open, Part::Closed] {
                if let Some(mut d) = open_part(base, scope, part).await? {
                    dropped += drop_duplicates(&mut d, FACT_KEY).await?;
                }
            }
            if dropped == 0 {
                NONE_FOUND
                    .lock()
                    .unwrap()
                    .insert(dir.clone(), std::time::Instant::now());
            } else {
                drop_open(|k| k == dir);
            }
        }
    }
    if files.is_empty() && stale == 0 {
        record_facts_coverage(store, scope, heads).await?;
        let dir = base_uri(base, scope);
        for (part, name) in [
            (Part::Open, "open"),
            (Part::Closed, "closed"),
            (Part::Files, "files"),
        ] {
            if let Some(ds) = open_part(base, scope, part).await? {
                drop_old_versions_idle(&ds, &dir, name).await;
            }
        }
        return Ok((0, 0));
    }
    let total: usize = files
        .iter()
        .flat_map(|(_, b)| b.iter())
        .map(|b| b.num_rows())
        .sum();
    let (mut n_rows, mut reused) = (0, 0);
    for chunk in chunks(
        files.iter().map(|(k, b)| (k, b.as_slice())),
        |k| progress.written(k),
        chunk_rows(),
    ) {
        // Split on the stored `valid_to` alone. A closure (`fact_closures`) or a newer version ends a
        // fact only for readers who can see that evidence, and current reads skip `closed`, so moving
        // such a row there would hide it from everyone else too. Those rows stay in `open`, and reads
        // apply closures and supersession per reader.
        let (mut open_rows, mut closed_rows) = (vec![], vec![]);
        for (file, b) in &chunk {
            let (l, n) = to_lance(file, b, &reuse).await?;
            reused += n;
            let is_open = arrow_array::BooleanArray::from_iter(
                (0..l.num_rows()).map(|i| Some(l.column(7).is_null(i))),
            );
            let not_open = arrow_arith::boolean::not(&is_open)?;
            open_rows.push(arrow_select::filter::filter_record_batch(&l, &is_open)?);
            closed_rows.push(arrow_select::filter::filter_record_batch(&l, &not_open)?);
            n_rows += b.num_rows();
        }
        store.check_fence()?;
        append_part(base, scope, Part::Open, open_rows, lance_schema()).await?;
        append_part(base, scope, Part::Closed, closed_rows, lance_schema()).await?;
        let through = through(&chunk, |k| progress.written(k));
        progress.record(&through, &parts).await?;
        eprintln!(
            "memstore: index of {}: wrote {} of {total} rows ({reused} with the vectors they had)",
            scope.namespace(),
            resumed + n_rows
        );
    }
    store.check_fence()?;
    let open = open_part(base, scope, Part::Open).await?;
    let closed = open_part(base, scope, Part::Closed).await?;
    for (ds, time_cols) in [(open, false), (closed, true)] {
        let Some(mut ds) = ds else { continue };
        // New rows go into delta indexes beside the existing ones (`build_index`); a scope with no index yet
        // gets a full build.
        for (col, _, _) in FTS_FIELDS {
            build_index(
                &mut ds,
                &[col],
                IndexType::Inverted,
                col,
                &fts_build(fts_params(matches!(col, "t_value" | "t_quote"))),
            )
            .await?;
        }
        // Small parts are searched by a flat scan of their embeddings; past VEC_INDEX_ROWS that
        // scan dominates search time, so the part gets a vector index (see `vec_index`).
        let rows = ds.count_rows(None).await?;
        if rows >= VEC_INDEX_ROWS {
            build_index(
                &mut ds,
                &["vec"],
                IndexType::Vector,
                "vec",
                &vec_index(rows),
            )
            .await?;
        }
        // Key lookups and filters: BTree for ids, numbers and times, Bitmap for the few
        // distinct types and predicates.
        let mut btree = vec![
            "fact_id",
            "ekey",
            "vkey",
            "entity",
            "value_num",
            "value_ts",
            "value_ref",
            "valid_from",
        ];
        if time_cols {
            btree.push("valid_to");
        }
        for (cols, kind) in [
            (btree, BuiltinIndexType::BTree),
            (vec!["entity_type", "predicate"], BuiltinIndexType::Bitmap),
        ] {
            let index_type = match &kind {
                BuiltinIndexType::Bitmap => IndexType::Bitmap,
                _ => IndexType::BTree,
            };
            for col in cols {
                build_index(
                    &mut ds,
                    &[col],
                    index_type,
                    col,
                    &ScalarIndexParams::for_builtin(kind.clone()),
                )
                .await?;
            }
        }
        tidy(&mut ds, Tidy::Update).await?;
    }
    let marks = RecordBatch::try_new(
        files_schema(),
        vec![Arc::new(StringArray::from_iter_values(
            files.iter().map(|(k, _)| k.as_str()),
        ))],
    )?;
    append_part(base, scope, Part::Files, vec![marks], files_schema()).await?;
    progress.clear().await?;
    let dir = base_uri(base, scope);
    drop_open(|k| k == dir);
    record_facts_coverage(store, scope, heads).await?;
    for (part, name) in [
        (Part::Open, "open"),
        (Part::Closed, "closed"),
        (Part::Files, "files"),
    ] {
        if let Some(ds) = open_part(base, scope, part).await? {
            drop_old_versions(&ds, &dir, name).await;
        }
    }
    Ok((n_rows, files.len()))
}

/// Record in the facts index's manifest that it covers `heads`, with its parts' versions.
async fn record_facts_coverage(
    store: &Store,
    scope: &Scope,
    heads: BTreeMap<String, Vec<String>>,
) -> Result<()> {
    let base = store.index_base();
    let mut versions = BTreeMap::new();
    for (part, name) in [
        (Part::Open, "open"),
        (Part::Closed, "closed"),
        (Part::Files, "files"),
    ] {
        if let Some(d) = open_part(base, scope, part).await? {
            versions.insert(name.to_string(), d.version().version);
        }
    }
    record_coverage(&base_uri(base, scope), heads, versions, config()).await
}

/// A fact column a filter can test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Col {
    Entity,
    EntityType,
    Predicate,
    Value,
    ValueNum,
    ValueTs,
    ValueRef,
    ValidFrom,
    ValidTo,
    /// The (entity, predicate) key column (see [`Key::Entity`]), BTree-indexed.
    EKey,
    /// The fact's tags as `,a,b,`: a tag is matched whole with `Contains(Tags, ",tag,")`.
    Tags,
    /// The fact's id, BTree-indexed.
    FactId,
    /// The episodes a fact cites as `a,b` (not indexed): an episode is matched with `Contains(Sources, id)`.
    Sources,
}

impl Col {
    fn sql(self) -> &'static str {
        match self {
            Col::Entity => "`entity`",
            Col::EntityType => "`entity_type`",
            Col::Predicate => "`predicate`",
            Col::Value => "`value`",
            Col::ValueNum => "`value_num`",
            Col::ValueTs => "`value_ts`",
            Col::ValueRef => "`value_ref`",
            Col::ValidFrom => "`valid_from`",
            Col::ValidTo => "`valid_to`",
            Col::EKey => "`ekey`",
            Col::Tags => "`tags`",
            Col::FactId => "`fact_id`",
            Col::Sources => "`sources`",
        }
    }

    fn of(self, f: &Fact) -> Option<Lit> {
        match self {
            Col::Entity => Some(Lit::Str(f.entity.clone())),
            Col::EntityType => f.entity_type.clone().map(Lit::Str),
            Col::Predicate => Some(Lit::Str(f.predicate.clone())),
            Col::Value => Some(Lit::Str(f.value.clone())),
            Col::ValueNum => f.value_num.map(Lit::Num),
            Col::ValueTs => f.value_ts.map(Lit::Int),
            Col::ValueRef => f.value_ref.clone().map(Lit::Str),
            Col::ValidFrom => Some(Lit::Int(f.valid_from)),
            Col::ValidTo => f.valid_to.map(Lit::Int),
            Col::EKey => Some(Lit::Str(Key::Entity(&f.entity, &f.predicate).text())),
            Col::Tags => crate::tags_column(&f.tags).map(Lit::Str),
            Col::FactId => Some(Lit::Str(f.fact_id.clone())),
            Col::Sources => Some(Lit::Str(f.source_episodes.join(","))),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Lit {
    Str(String),
    Num(f64),
    Int(i64),
}

impl Lit {
    fn sql(&self) -> String {
        match self {
            Lit::Str(s) => format!("'{}'", s.replace('\'', "''")),
            Lit::Num(n) => format!("{n:?}"),
            Lit::Int(i) => i.to_string(),
        }
    }

    fn cmp(&self, other: &Lit) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Lit::Str(a), Lit::Str(b)) => Some(a.cmp(b)),
            (Lit::Num(a), Lit::Num(b)) => a.partial_cmp(b),
            (Lit::Int(a), Lit::Int(b)) => Some(a.cmp(b)),
            (Lit::Num(a), Lit::Int(b)) => a.partial_cmp(&(*b as f64)),
            (Lit::Int(a), Lit::Num(b)) => (*a as f64).partial_cmp(b),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A predicate over fact rows. The index evaluates it as a Lance SQL prefilter (served by the
/// scalar indexes); unindexed rows, and every index row again, are checked in memory by
/// [`Filter::matches`], with SQL's three-valued logic, so both paths agree and the SQL only has
/// to be no stricter than the in-memory check.
#[derive(Clone, Debug, PartialEq)]
pub enum Filter {
    Eq(Col, Lit),
    In(Col, Vec<Lit>),
    Cmp(Col, Op, Lit),
    Null(Col),
    NotNull(Col),
    /// Case-insensitive substring of a text column.
    Contains(Col, String),
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),
    /// The fact cites one of these episodes (an episode type's, say). Checked in memory only: its SQL
    /// is `true`, so the index prefilters nothing for it and the fetch loop grows until enough match.
    Cites(Cited),
}

/// The episodes a [`Filter::Cites`] names, under a label for what they are (`slack_thread`). The label is the
/// filter's identity: it prints as the label alone, so a search's cursor still names the same search after
/// an episode of the type arrives, and printing it doesn't spell out every id.
#[derive(Clone, PartialEq)]
pub struct Cited {
    pub label: String,
    pub ids: std::sync::Arc<std::collections::BTreeSet<String>>,
}

impl std::fmt::Debug for Cited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cited({:?})", self.label)
    }
}

impl Filter {
    pub fn and(filters: impl IntoIterator<Item = Filter>) -> Filter {
        let v: Vec<Filter> = filters.into_iter().collect();
        if v.len() == 1 {
            v.into_iter().next().unwrap()
        } else {
            Filter::And(v)
        }
    }

    pub fn to_sql(&self) -> String {
        match self {
            Filter::Eq(c, l) => format!("{} = {}", c.sql(), l.sql()),
            Filter::In(_, ls) if ls.is_empty() => "false".into(),
            Filter::In(c, ls) => format!(
                "{} IN ({})",
                c.sql(),
                ls.iter().map(Lit::sql).collect::<Vec<_>>().join(", ")
            ),
            Filter::Cmp(c, op, l) => {
                let op = match op {
                    Op::Ne => "<>",
                    Op::Lt => "<",
                    Op::Le => "<=",
                    Op::Gt => ">",
                    Op::Ge => ">=",
                };
                format!("{} {op} {}", c.sql(), l.sql())
            }
            Filter::Null(c) => format!("{} IS NULL", c.sql()),
            Filter::NotNull(c) => format!("{} IS NOT NULL", c.sql()),
            // LIKE wildcards in the needle widen the pattern; the in-memory check is exact.
            Filter::Contains(c, s) => format!(
                "lower({}) LIKE '%{}%'",
                c.sql(),
                s.to_lowercase()
                    .replace(['%', '_'], "%")
                    .replace('\'', "''")
            ),
            Filter::And(fs) if fs.is_empty() => "true".into(),
            Filter::Or(fs) if fs.is_empty() => "false".into(),
            Filter::And(fs) => fs
                .iter()
                .map(|f| format!("({})", f.to_sql()))
                .collect::<Vec<_>>()
                .join(" AND "),
            Filter::Or(fs) => fs
                .iter()
                .map(|f| format!("({})", f.to_sql()))
                .collect::<Vec<_>>()
                .join(" OR "),
            // NOT of a check SQL can't make would be stricter than the in-memory one: prefilter nothing.
            Filter::Not(f) if !f.sql_exact() => "true".into(),
            Filter::Not(f) => format!("NOT ({})", f.to_sql()),
            Filter::Cites(_) => "true".into(),
        }
    }

    /// Whether [`Filter::to_sql`] says exactly what the in-memory check does (not only something looser).
    fn sql_exact(&self) -> bool {
        match self {
            // LIKE treats `_` and `%` as wildcards, and SQL's lower() needn't agree with Rust's past ASCII.
            Filter::Cites(_) | Filter::Contains(..) => false,
            Filter::And(fs) | Filter::Or(fs) => fs.iter().all(Filter::sql_exact),
            Filter::Not(f) => f.sql_exact(),
            _ => true,
        }
    }

    pub fn matches(&self, f: &Fact) -> bool {
        self.matches_with(&|c| c.of(f))
    }

    /// Whether a row matches, its columns given by `col` (none for a null one), as [`Filter::matches`]
    /// checks a fact.
    pub fn matches_with(&self, col: &dyn Fn(Col) -> Option<Lit>) -> bool {
        self.eval(col) == Some(true)
    }

    /// SQL semantics: a comparison with a null column is unknown (None), and NOT unknown is too.
    fn eval(&self, col: &dyn Fn(Col) -> Option<Lit>) -> Option<bool> {
        use std::cmp::Ordering::*;
        match self {
            Filter::Eq(c, l) => Some(col(*c)?.cmp(l)? == Equal),
            Filter::In(c, ls) => {
                let v = col(*c)?;
                Some(ls.iter().any(|l| v.cmp(l) == Some(Equal)))
            }
            Filter::Cmp(c, op, l) => {
                let o = col(*c)?.cmp(l)?;
                Some(match op {
                    Op::Ne => o != Equal,
                    Op::Lt => o == Less,
                    Op::Le => o != Greater,
                    Op::Gt => o == Greater,
                    Op::Ge => o != Less,
                })
            }
            Filter::Null(c) => Some(col(*c).is_none()),
            Filter::NotNull(c) => Some(col(*c).is_some()),
            Filter::Contains(c, s) => match col(*c)? {
                Lit::Str(v) => Some(v.to_lowercase().contains(&s.to_lowercase())),
                _ => None,
            },
            Filter::And(fs) => {
                let mut unknown = false;
                for x in fs {
                    match x.eval(col) {
                        Some(false) => return Some(false),
                        None => unknown = true,
                        Some(true) => {}
                    }
                }
                (!unknown).then_some(true)
            }
            Filter::Or(fs) => {
                let mut unknown = false;
                for x in fs {
                    match x.eval(col) {
                        Some(true) => return Some(true),
                        None => unknown = true,
                        Some(false) => {}
                    }
                }
                (!unknown).then_some(false)
            }
            Filter::Not(x) => x.eval(col).map(|b| !b),
            Filter::Cites(c) => match col(Col::Sources)? {
                Lit::Str(v) => Some(v.split(',').any(|e| c.ids.contains(e))),
                _ => None,
            },
        }
    }
}

/// Columns read back from the index for a fact (everything but the embedding).
const FACT_COLUMNS: &[&str] = &[
    "data_file",
    "commit_id",
    "fact_id",
    "entity",
    "predicate",
    "value",
    "valid_from",
    "valid_to",
    "sources",
    "quote",
    "entity_type",
    "value_type",
    "value_num",
    "value_ts",
    "value_ref",
    "unit",
    "cite",
    "time_basis",
    "tags",
];

/// A fact row of an index part, decoded: its data file, commit, the fact and its embedding.
struct FactRow {
    file: String,
    commit: String,
    fact: Fact,
    vec: Option<Vec<f32>>,
}

struct PreparedRow {
    // Keep the allocation alive: a later row must never reuse this cache key's address.
    _row: Arc<FactRow>,
    matched: Option<usize>,
    keyword_distance: Option<Option<f32>>,
}

/// One ranking pass's preparation, shared across its lists. Fact IDs alone are not row identities:
/// different physical rows with the same ID may carry different commits or contents.
#[derive(Default)]
struct PreparedCandidates(HashMap<usize, PreparedRow>);

impl PreparedCandidates {
    fn for_list(
        &mut self,
        row: &Arc<FactRow>,
        query: &[f32],
        keyword: bool,
        distance: Option<f32>,
        prepare: impl FnOnce(&Fact, &str) -> Option<usize>,
    ) -> Option<(usize, Option<f32>)> {
        let cached = self
            .0
            .entry(Arc::as_ptr(row) as usize)
            .or_insert_with(|| PreparedRow {
                _row: row.clone(),
                matched: prepare(&row.fact, &row.commit),
                keyword_distance: None,
            });
        let matched = cached.matched?;
        let distance = if keyword {
            *cached.keyword_distance.get_or_insert_with(|| {
                row.vec.as_ref().map(|e| {
                    e.iter()
                        .zip(query.iter())
                        .map(|(a, b)| (a - b) * (a - b))
                        .sum()
                })
            })
        } else {
            distance
        };
        Some((matched, distance))
    }
}

#[cfg(test)]
mod candidate_tests;

/// Fact rows read from index parts (see [`rows`]): about 4 KB each with the embedding (on a production deployment), so at
/// most a few hundred MB. `AMPUP_MEMORY_INDEX_FACT_ROWS` overrides.
const FACT_ROWS_MAX: usize = 100_000;

static FACT_ROWS: std::sync::LazyLock<rows::RowCache<FactRow>> =
    std::sync::LazyLock::new(|| rows::RowCache::new(index_knob("FACT_ROWS", FACT_ROWS_MAX)));

fn decode_rows(b: &RecordBatch) -> Result<Vec<FactRow>> {
    let file = crate::strings(b, "data_file");
    let vecs = b
        .column_by_name("vec")
        .map(|c| c.as_fixed_size_list().clone());
    Ok(crate::decode_facts(b, "sources")
        .into_iter()
        .enumerate()
        .map(|(i, (commit, fact))| FactRow {
            file: file[i].clone(),
            commit,
            fact,
            vec: vecs.as_ref().map(|vs| {
                vs.value(i)
                    .as_primitive::<arrow_array::types::Float32Type>()
                    .values()
                    .to_vec()
            }),
        })
        .collect())
}

/// Fact rows read by an entity's, a key's or some facts' ids ([`keyed`] scans, [`key_lookup`]), without the
/// embedding: a search's groups read every fact of their entities, thousands for a big deal, and the embedding,
/// which only ranking uses, is most of a row (a few KB of about 4). About 1 KB each, so at most a few hundred MB.
/// `AMPUP_MEMORY_INDEX_KEYED_ROWS` overrides.
const KEYED_ROWS_MAX: usize = 200_000;

static KEYED_ROWS: std::sync::LazyLock<rows::RowCache<FactRow>> =
    std::sync::LazyLock::new(|| rows::RowCache::new(index_knob("KEYED_ROWS", KEYED_ROWS_MAX)));

/// The rows `ids` of index part `ds` without their embeddings (see [`KEYED_ROWS`]), by id.
async fn keyed_rows(ds: &Dataset, ids: Vec<u64>) -> Result<HashMap<u64, Arc<FactRow>>> {
    let rows = KEYED_ROWS.get(ds, &ids, FACT_COLUMNS, decode_rows).await?;
    Ok(ids.into_iter().zip(rows).collect())
}

/// The rows `ids` of index part `ds`, by id: from the row cache, the missing ones in one read.
async fn fact_rows(ds: &Dataset, ids: Vec<u64>) -> Result<HashMap<u64, Arc<FactRow>>> {
    Ok(fact_rows_counted(ds, ids).await?.0)
}

/// [`fact_rows`], with how many rows it took from the part's files (the rest came from the row cache).
async fn fact_rows_counted(
    ds: &Dataset,
    ids: Vec<u64>,
) -> Result<(HashMap<u64, Arc<FactRow>>, usize)> {
    let columns: Vec<&str> = FACT_COLUMNS.iter().copied().chain(["vec"]).collect();
    let (rows, taken) = FACT_ROWS
        .get_counted(ds, &ids, &columns, decode_rows)
        .await?;
    Ok((ids.into_iter().zip(rows).collect(), taken))
}

/// Whether a scan with `filter` reads few rows, which its scalar indexes find: an entity's, a key's, or
/// some facts' by id. Its rows come from the row cache; any other scan reads its rows directly.
fn keyed(filter: &Filter) -> bool {
    match filter {
        Filter::Eq(Col::Entity | Col::EKey | Col::FactId, _)
        | Filter::In(Col::Entity | Col::EKey | Col::FactId, _) => true,
        Filter::And(fs) => fs.iter().any(keyed),
        _ => false,
    }
}

/// Every fact row matching `filter`, as (commit id, fact), before visibility filtering: from the
/// index (the filter as a prefilter, served by its scalar indexes) for files it holds that are
/// still current, and from the unindexed tail in memory. With `history`, superseded rows (the
/// closed part) are read too.
pub async fn scan(
    store: &Store,
    scope: &Scope,
    filter: &Filter,
    history: bool,
) -> Result<Vec<(String, Fact)>> {
    let index = open_cached(store, scope).await?;
    let (current, tail) = store.fact_files(scope, &index.held).await?;
    let current = index.trusted(current);
    let sql = filter.to_sql();
    let mut out = vec![];
    let parts = [Some(&index.open), history.then_some(&index.closed)];
    for ds in parts.into_iter().flatten().flatten() {
        if keyed(filter) {
            let mut scan = ds.scan();
            scan.filter(&sql)?;
            let ids: Vec<u64> = rows::ids(scan)
                .await?
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let rows = keyed_rows(ds, ids.clone()).await?;
            for id in &ids {
                let r = &rows[id];
                if current.contains(&r.file) && filter.matches(&r.fact) {
                    out.push((r.commit.clone(), r.fact.clone()));
                }
            }
            continue;
        }
        let batches: Vec<RecordBatch> = ds
            .scan()
            .filter(&sql)?
            .project(FACT_COLUMNS)?
            .try_into_stream()
            .await?
            .try_collect()
            .await?;
        for b in &batches {
            let file = crate::strings(b, "data_file");
            for (i, row) in crate::decode_facts(b, "sources").into_iter().enumerate() {
                if current.contains(&file[i]) && filter.matches(&row.1) {
                    out.push(row);
                }
            }
        }
    }
    for (_, rows) in &tail {
        for b in rows.iter() {
            out.extend(
                crate::decode_facts(b, "source_episodes")
                    .into_iter()
                    .filter(|(_, f)| (history || f.valid_to.is_none()) && filter.matches(f)),
            );
        }
    }
    Ok(out)
}

/// The facts table's current files as a number: what reads of `scope`'s facts would read now, the index
/// standing in for the files it holds. Something kept from an earlier read holds while this stays the same
/// (and so does the visibility state it was read under).
pub async fn files_stamp(store: &Store, scope: &Scope) -> Result<u64> {
    use std::hash::{Hash, Hasher};
    let index = open_cached(store, scope).await?;
    let (current, _) = store.fact_files(scope, &index.held).await?;
    let mut keys: Vec<&String> = current.iter().collect();
    keys.sort_unstable();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    scope.namespace().hash(&mut h);
    keys.hash(&mut h);
    Ok(h.finish())
}

/// An (entity, predicate) pair: the key a one-valued attribute's versions share.
pub type VersionKey = (String, String);

/// For each key, the newest version `who` can see that began by `t`, as (valid_from, fact_id).
/// That version is the attribute's value at `t` (if it is still valid then); every older version
/// is superseded by it. Visibility is the reader's own: a newer version they can't see doesn't
/// count, so it never hides the one they can. Whether the newer version is still valid at `t`
/// doesn't matter: once a value is replaced, the old one doesn't come back when the new one
/// ends. History is read (superseded rows live in the closed part): when the keys of one predicate
/// are more than a chunk and their entities are all of type `ty`, in one scan of that type's
/// versions of the predicate; the other keys, by the key index, a chunk of keys per scan.
pub async fn newest_versions(
    store: &Store,
    scope: &Scope,
    vis: &crate::Vis,
    who: &Principal,
    keys: &std::collections::BTreeSet<VersionKey>,
    t: i64,
    ty: Option<&str>,
) -> Result<HashMap<VersionKey, (i64, String)>> {
    const CHUNK: usize = 500;
    // Only keys with another version can have a newer one: more than one row in the index, or rows it
    // doesn't hold yet. The rest have one version, which nothing supersedes.
    let index = open_cached(store, scope).await?;
    let multi = index.multi_keys().await?;
    let (_, tail) = store.fact_files(scope, &index.held).await?;
    let mut unindexed = HashSet::new();
    for (_, rows) in &tail {
        for b in rows.iter() {
            let (e, p) = (crate::strings(b, "entity"), crate::strings(b, "predicate"));
            for i in 0..b.num_rows() {
                unindexed.insert(key_hash(&Key::Entity(&e[i], &p[i]).text()));
            }
        }
    }
    let mut by_pred: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (e, p) in keys {
        let h = key_hash(&Key::Entity(e, p).text());
        if multi.contains(&h) || unindexed.contains(&h) {
            by_pred.entry(p.as_str()).or_default().push(e.as_str());
        }
    }
    let mut filters = vec![];
    let mut rest = vec![];
    for (pred, of) in by_pred {
        match ty {
            Some(ty) if of.len() > CHUNK => filters.push(Filter::And(vec![
                Filter::Eq(Col::Predicate, Lit::Str(pred.to_string())),
                of_type(ty),
            ])),
            _ => rest.extend(of.into_iter().map(|e| Key::Entity(e, pred).text())),
        }
    }
    for chunk in rest.chunks(CHUNK) {
        filters.push(Filter::In(
            Col::EKey,
            chunk.iter().map(|k| Lit::Str(k.clone())).collect(),
        ));
    }
    let mut out: HashMap<VersionKey, (i64, String)> = HashMap::new();
    for filter in filters {
        for (commit, f) in scan(store, scope, &filter, true).await? {
            // A version a merge set aside by `t` lost to the kept side's value: it supersedes nothing.
            if f.valid_from > t || !vis.allows(&commit, &f, who, None) || vis.lost_merge(&f, who, t)
            {
                continue;
            }
            // The type scan reads entities no key names.
            let key = (f.entity, f.predicate);
            if !keys.contains(&key) {
                continue;
            }
            let v = (f.valid_from, f.fact_id);
            let n = out.entry(key).or_insert(v.clone());
            if v > *n {
                *n = v;
            }
        }
    }
    Ok(out)
}

/// Facts of entities of type `ty`, and of any written before their scope had a schema (no type).
pub fn of_type(ty: &str) -> Filter {
    Filter::Or(vec![
        Filter::Eq(Col::EntityType, Lit::Str(ty.to_string())),
        Filter::Null(Col::EntityType),
    ])
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub fact: Fact,
    /// Distinct informative query terms the fact's text contains.
    pub terms_matched: usize,
    /// BM25 from the index (0 if the fact didn't match by keyword); for rows from the unindexed
    /// tail, an estimate (see `search_with`).
    pub bm25: f32,
    /// Distance of the fact's embedding from the query's, if it was among the nearest.
    pub distance: Option<f32>,
    /// Reciprocal-rank fusion of the keyword and vector rankings; hits are ordered by it.
    pub score: f32,
    /// Where the fact came from: `extract:<model>` for the engine's extraction, else `posted` (an agent, a
    /// script, an import); never the poster's identity.
    pub written_by: Option<String>,
    /// The scale recency gave the fact's scores before ranking (see [`recency_bias`]).
    pub recency: f32,
    pub from_tail: bool,
    /// The fact's score in each keyword ranking (see `keyword_queries`): any terms, all terms, phrases.
    /// `bm25` is their weighted sum.
    pub signals: [f32; 3],
    /// Whether the fact is in the time the query names (see [`when::window`]): stated in it, or naming one of
    /// its years. False when the query names no time.
    pub in_window: bool,
    /// With [`Rank::group`], the facts that came with this one (see `group_hits`), best first.
    pub group: Vec<Fact>,
}

/// Facts a grouped hit brings at most (see [`Rank::group`]); `AMPUP_MEMORY_RANK_GROUP` overrides.
pub const GROUP: usize = 4;

pub fn terms(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// `s`'s words as the full-text index holds them: its own analyzer (lower case, English stemming, stop words
/// removed, ASCII folding), so a match here is a match there.
pub fn stems(s: &str) -> HashSet<String> {
    use lance_index::scalar::inverted::tokenizer::document_tokenizer::LanceTokenizer;
    use lance_tokenizer::TokenStream;
    thread_local! {
        static ANALYZER: std::cell::RefCell<Option<Box<dyn LanceTokenizer>>> =
            const { std::cell::RefCell::new(None) };
    }
    ANALYZER.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            *cell = InvertedIndexParams::default().build().ok();
        }
        // The default analyzer always builds; were it not to, plain words are the next best thing.
        let Some(t) = cell.as_mut() else {
            return terms(s);
        };
        let mut out = HashSet::new();
        let mut stream = t.token_stream_for_doc(s);
        while stream.advance() {
            out.insert(stream.token().text.clone());
        }
        out
    })
}

/// Words that carry no topic: question words, articles, auxiliaries, and pure digits (date parts
/// like 2026 / 04 / 02 match half the corpus; time is answered by valid-time, not by text).
pub fn informative(t: &str) -> bool {
    !STOP.contains(&t) && !t.bytes().all(|b| b.is_ascii_digit())
}

const STOP: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "did", "do", "does", "for", "from",
    "has", "have", "how", "i", "in", "is", "it", "its", "of", "on", "or", "our", "that", "the",
    "this", "to", "was", "we", "were", "what", "when", "where", "which", "who", "whom", "why",
    "will", "with", "you",
];

/// A fact search's keyword terms, in the query's order and repeats kept (phrases are read off neighbours):
/// its words as the index reads them ([`normalize_text`], lower case), less stop words and stray letters.
/// Numbers stay: a year, a version or an amount is often the most specific word asked.
pub fn fact_terms(query: &str) -> Vec<String> {
    let digits = knob("DIGITS", 1.0) > 0.0;
    normalize_text(query)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .filter(|t| {
            let number = t.bytes().all(|b| b.is_ascii_digit());
            !STOP.contains(&t.as_str()) && (t.chars().count() > 1 || number) && (digits || !number)
        })
        .collect()
}

/// Distinct terms a fact search's keyword list searches at most (`AMPUP_MEMORY_RANK_MAX_TERMS`, default 64): of a
/// longer query, the ones the fewest facts hold ([`fewest`]).
fn max_terms() -> usize {
    (knob("MAX_TERMS", 64.0) as usize).max(1)
}

/// The rows a fact search's keyword list may reach, as a multiple of the facts its index holds
/// (`AMPUP_MEMORY_RANK_TERM_ROWS`, default 1.5): its terms' rows, summed over the terms and the fields ([`fewest`]).
/// On the relevance set (126 judged queries as asked, and the same inside long prompts as the hook trims
/// them), 2 and 1.5 changed no ranking of a query as asked, and 1.5 raised the trimmed prompts' R@8 from
/// 0.290 to 0.295 and MRR from 0.361 to 0.376; 1 lowered R@8 to 0.286, and 0.5 to 0.245. On a production deployment's facts,
/// 1.5 cut a trimmed prompt's list from 0.55-0.87 s to 0.45-0.6 s on an idle machine.
fn term_rows_budget() -> f64 {
    f64::from(knob("TERM_ROWS", 1.5).max(0.0))
}

/// The keyword terms a list over `cols` of `ds` searches, of `terms`: the rarest first by the rows holding them
/// ([`term_rows`]), at most `max`, and past the rarest only while the rows they reach together stay within
/// `budget` times the rows `ds` holds; None when that is every one of them. A fact search's keyword list
/// scores every row holding any of its terms (Lance's combined-fields search prunes none), so its time is in
/// the rows its terms reach, about half a second per index's worth on a production deployment's facts (441k) on an idle
/// machine: a session's whole first prompt, which the memstore plugin's hook searched for (95 distinct words
/// at the median in a production qa_log, cut at 256), took 6-10 s a list there, and a burst of agent sessions held
/// every core of the pod with them, so that searches of the org waited until their callers gave up. Cut to
/// its 64 rarest words, such a list reaches a fourteenth of the index at the median (40 ms). Cut to
/// its first and last characters by the hook instead, a prompt keeps its common words, each in 5-12%
/// of a production deployment's facts ("step", "agent", "issue"; "2026" is in every fact's date), and its list reached 1.3
/// times the index at the median and 2.7 at most: 0.55-1.2 s idle, 1.3-2.6 s on the busy pod. The commonest
/// words weigh least in BM25 and cost most; the rarest (names, paths, ids) are what a keyword match is for.
/// When the counts can't be read, the first `max` terms are searched.
pub(crate) async fn fewest(
    ds: Option<&Dataset>,
    cols: &[&str],
    terms: &[String],
    max: usize,
    budget: f64,
) -> Option<HashSet<String>> {
    let mut seen = HashSet::new();
    let distinct: Vec<String> = terms.iter().filter(|t| seen.insert(*t)).cloned().collect();
    let first = || (distinct.len() > max).then(|| distinct.iter().take(max).cloned().collect());
    if distinct.len() <= max && budget.is_infinite() {
        return None;
    }
    let Some(ds) = ds else {
        return first();
    };
    let counted = async {
        anyhow::Ok((
            term_rows(ds, cols, &distinct).await?,
            ds.count_rows(None).await?,
        ))
    };
    let (rows, held) = match counted.await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("memstore: a query's terms read in order (no term counts: {e:#})");
            return first();
        }
    };
    // Of a query longer than `max`, a term no row holds finds nothing and is left out, rather than take the
    // place of one that finds something; a shorter query keeps it (it costs nothing).
    let long = distinct.len() > max;
    let mut by_rows: Vec<(usize, usize)> = rows
        .into_iter()
        .enumerate()
        .filter(|(_, n)| *n > 0 || !long)
        .map(|(i, n)| (n, i))
        .collect();
    by_rows.sort_unstable();
    let budget = budget * held as f64;
    let mut reached = 0;
    let kept: HashSet<String> = by_rows
        .into_iter()
        .take(max)
        .take_while(|(n, _)| {
            reached += n;
            reached as f64 <= budget || reached == *n
        })
        .map(|(_, i)| distinct[i].clone())
        .collect();
    (kept.len() < distinct.len()).then_some(kept)
}

/// How many rows of `ds` each of `terms` is in, summed over the keyword-indexed columns `cols` and each one's
/// index segments: the postings a keyword query scores for it. Read from the index's posting lengths, which a
/// reader loads with its postings ([`warm_keywords`]).
async fn term_rows(ds: &Dataset, cols: &[&str], terms: &[String]) -> Result<Vec<usize>> {
    use lance::index::{DatasetIndexExt, DatasetIndexInternalExt};
    use lance_index::scalar::inverted::InvertedIndex;
    use lance_tokenizer::TokenStream;
    let mut rows = vec![0usize; terms.len()];
    let indices = ds.load_indices().await?;
    let mut found = false;
    for col in cols {
        let Some(field) = ds.schema().field(col) else {
            continue;
        };
        for meta in indices.iter().filter(|m| m.fields == [field.id]) {
            let index = ds
                .open_generic_index(col, &meta.uuid, &lance_index::metrics::NoOpMetricsCollector)
                .await?;
            let Some(inverted) = index.as_any().downcast_ref::<InvertedIndex>() else {
                continue;
            };
            found = true;
            // Each term as the index holds it (its analyzer stems it); one it reads as no token is in no row.
            let mut analyzer = inverted.tokenizer();
            let tokens: Vec<Option<String>> = terms
                .iter()
                .map(|t| {
                    let mut stream = analyzer.token_stream_for_search(t);
                    stream.advance().then(|| stream.token().text.clone())
                })
                .collect();
            let asked: Vec<String> = tokens.iter().flatten().cloned().collect();
            let (_, _, docs) = inverted.bm25_stats_for_terms(&asked, None).await?;
            let mut docs = docs.into_iter();
            for (n, t) in rows.iter_mut().zip(&tokens) {
                if t.is_some() {
                    *n += docs.next().unwrap_or(0);
                }
            }
        }
    }
    // No keyword index to count by is no count at all, not every term in no row.
    anyhow::ensure!(found, "no keyword index on {}", cols.join(", "));
    Ok(rows)
}

/// The keyword queries a fact search runs, each its own ranked list, and how much it weighs in a fact's
/// keyword score: BM25F over the fields ([`FTS_FIELDS`]) with any of the terms, the same with all of them
/// (`AMPUP_MEMORY_RANK_AND`), and neighbouring terms as phrases (one word between allowed) in the value or quote
/// (`AMPUP_MEMORY_RANK_PHRASE`). A [`Rank::classic`] search runs the one any-terms query, over the subject, predicate
/// and value alike, as BM25 over their text together did before the current ranking.
fn keyword_queries(
    terms: &[String],
    classic: bool,
    keep: Option<&HashSet<String>>,
) -> Result<Vec<(lance_index::scalar::inverted::query::FtsQuery, f32)>> {
    use lance_index::scalar::inverted::query::{
        BooleanQuery, CombinedFieldsQuery, FtsQuery, Occur, Operator, PhraseQuery,
    };
    // The distinct terms in order, those `keep` names ([`fewest`]), else the first `max_terms()`: a query is
    // any caller's text, up to a whole request body.
    let kept = |t: &str| keep.is_none_or(|k| k.contains(t));
    let mut seen = HashSet::new();
    let distinct: Vec<&str> = terms
        .iter()
        .map(String::as_str)
        .filter(|t| kept(t) && seen.insert(*t))
        .take(max_terms())
        .collect();
    if distinct.is_empty() {
        return Ok(vec![]);
    }
    let fields: Vec<(&str, f32)> = FTS_FIELDS
        .iter()
        .map(|(col, name, w)| match classic {
            true => (*col, f32::from(["VALUE", "SUBJECT", "PRED"].contains(name))),
            false => (*col, knob(&format!("W_{name}"), *w)),
        })
        .filter(|(_, w)| *w > 0.0)
        .collect();
    let least = fields.iter().map(|f| f.1).fold(f32::INFINITY, f32::min);
    let combined = |op: Operator| -> Result<FtsQuery> {
        let q = CombinedFieldsQuery::try_new(
            distinct.join(" "),
            fields.iter().map(|f| f.0.to_string()).collect(),
        )?
        .try_with_boosts(fields.iter().map(|f| f.1 / least).collect())?
        .with_operator(op);
        Ok(FtsQuery::CombinedFields(q))
    };
    let mut out = vec![(combined(Operator::Or)?, 1.0)];
    if classic {
        return Ok(out);
    }
    // Off by default: on the relevance set (126 queries, 10-27) no mean dropped without it and 108 rankings
    // were the same, and it is a whole keyword query's scoring, which queues on Lance's CPU pool under load.
    let and = knob("AND", 0.0);
    if distinct.len() > 1 && and > 0.0 {
        out.push((combined(Operator::And)?, and));
    }
    // Off by default: on the relevance set, phrase matches (at 5x, and even 1x) pushed facts that repeat the
    // query's words ahead of the ones that answer it.
    let phrase = knob("PHRASE", 0.0);
    let mut pairs: Vec<String> = terms
        .windows(2)
        .filter(|w| w[0] != w[1] && kept(&w[0]) && kept(&w[1]))
        .map(|w| format!("{} {}", w[0], w[1]))
        .collect();
    pairs.dedup();
    pairs.truncate(8);
    if !pairs.is_empty() && phrase > 0.0 {
        let clauses = pairs.iter().flat_map(|p| {
            ["t_value", "t_quote"].map(|col| {
                (
                    Occur::Should,
                    FtsQuery::Phrase(
                        PhraseQuery::new(p.clone())
                            .with_column(Some(col.to_string()))
                            .with_slop(1),
                    ),
                )
            })
        });
        out.push((FtsQuery::Boolean(BooleanQuery::new(clauses)), phrase));
    }
    Ok(out)
}

/// Top-`k` facts matching `query` that the principal may see on `branch`, valid at `valid_at`
/// (None = any time). Newer facts rank a little higher, as of `valid_at` when it is a time, else now.
pub async fn search(
    store: &Store,
    scope: &Scope,
    branch: &str,
    who: &Principal,
    query: &str,
    k: usize,
    valid_at: Option<i64>,
) -> Result<Vec<Hit>> {
    let session = store.pin(scope, branch).await?;
    let rank = Rank {
        as_of: Some(
            valid_at
                .filter(|&t| t < i64::MAX)
                .unwrap_or_else(crate::now_micros),
        ),
        diverse: false,
        group: 0,
        classic: false,
    };
    session.search(store, who, query, k, valid_at, rank).await
}

/// Load what a scope's first search reads, so it isn't one reader's wait: the ledger and the scope's tables
/// (the visibility state), the embedding model, and the fact and passage indexes with their index files: one
/// query of each, for row ids only, so no row is read for anyone.
pub async fn warm(store: &Store, scope: &Scope) -> Result<()> {
    store.pin(scope, crate::MAIN).await?;
    let query = "warm up";
    let q = Float32Array::from(embed_text(query).await?);
    let index = open_cached(store, scope).await?;
    store.fact_files(scope, &index.held).await?;
    let keyword = keyword_queries(&fact_terms(query), false, None)?;
    for ds in [&index.open, &index.closed].into_iter().flatten() {
        for (k, _) in &keyword {
            let mut scan = ds.scan();
            scan.full_text_search(FullTextSearchQuery::new_query(k.clone()).limit(Some(1)))?;
            rows::ids(scan).await?;
        }
        if rows::nearest_flat(ds, "vec", &q, 1, None).await?.is_none() {
            let mut scan = ds.scan();
            probe(scan.nearest("vec", &q, 1)?);
            rows::ids(scan).await?;
        }
    }
    passages::warm(store, scope, query, &q).await
}

/// Index rows fetched per search part start at this multiple of `k`, and grow by 4x while too
/// few of them are visible to the principal (a selective filter or a narrow ACL), up to
/// MAX_FETCH, so a principal who may see few facts still gets their top `k`.
pub const FETCH_MULTIPLE: usize = 20;
pub const MAX_FETCH: usize = 20_000;
/// Fact search ranks a pool of this many index rows per ranked list, however many hits are asked for, so
/// a search's first hits are the same at every `k` (growing only while too few are visible). Set by
/// `AMPUP_MEMORY_RANK_POOL`.
pub const FACT_POOL: usize = 300;

/// A ranking knob from `AMPUP_MEMORY_RANK_<name>`, else `default` (for experiments; the defaults are measured).
pub fn knob(name: &str, default: f32) -> f32 {
    std::env::var(format!("AMPUP_MEMORY_RANK_{name}"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// [`search`] against an already-pinned visibility state, optionally restricted by `filter`
/// (applied before ranking), ordered by `rank`.
///
/// The hits are one fixed sequence for a pinned state, whatever `k`, so a page past an offset never repeats a hit
/// an earlier page returned: the hits a pool of [`FACT_POOL`] index rows per ranked list ranks ([`ranked`]), all
/// of them once folded (near-duplicates as one, groups); then, while that holds fewer than `k` and more rows exist,
/// the hits of a pool 4 times as deep that aren't already shown, as a hit or in a group; and so on to
/// [`MAX_FETCH`]. A deeper pool ranks differently (scores are normalized over the pool), so its hits are appended,
/// never re-ranked into what an earlier page saw.
#[allow(clippy::too_many_arguments)]
pub async fn search_with(
    store: &Store,
    scope: &Scope,
    vis: &crate::Vis,
    pin: Option<&IndexPin>,
    who: &Principal,
    query: &str,
    k: usize,
    valid_at: Option<i64>,
    filter: Option<&Filter>,
    rank: Rank,
) -> Result<Vec<Hit>> {
    // A classic ranking's pool grows with `k`, as its lists did before the current ranking.
    let mut pool = match rank.classic {
        true => k * FETCH_MULTIPLE,
        false => knob("POOL", FACT_POOL as f32) as usize,
    }
    .max(1);
    let mut out: Vec<Hit> = vec![];
    let mut shown: HashSet<String> = HashSet::new();
    loop {
        // Enough of this pool's hits to find what's left of `k` past those already shown.
        let want = k + shown.len();
        let (hits, deeper) = ranked(
            store, scope, vis, pin, who, query, want, valid_at, filter, rank, pool,
        )
        .await?;
        for mut h in hits {
            if out.len() >= k {
                break;
            }
            if !shown.insert(h.fact.fact_id.clone()) {
                continue;
            }
            h.group.retain(|f| shown.insert(f.fact_id.clone()));
            out.push(h);
        }
        if out.len() >= k || !deeper || pool >= MAX_FETCH {
            return Ok(out);
        }
        pool = (pool * 4).min(MAX_FETCH);
    }
}

/// A fact search taking this many ms logs its phases, as a passage search does (`AMPUP_MEMORY_RANK_SLOW_MS`).
const SLOW_FACT_MS: f32 = 1_000.0;

/// A fact search's phases, logged when it ends slow however it ends: each phase's end in ms since the start, and
/// each ranked list's own time (the vector list reads its index's partitions, from object storage when cold).
struct SlowLog {
    scope: String,
    t0: std::time::Instant,
    took: std::sync::Mutex<Vec<(&'static str, f64)>>,
    lists: std::sync::Mutex<Vec<String>>,
    /// The query's distinct keyword terms: a keyword list's time grows with them.
    terms: std::sync::atomic::AtomicUsize,
    /// Of those, how many were searched ([`fewest`]); 0: all of them.
    searched: std::sync::atomic::AtomicUsize,
}

impl SlowLog {
    fn mark(&self, what: &'static str) {
        self.took
            .lock()
            .unwrap()
            .push((what, self.t0.elapsed().as_secs_f64() * 1000.0));
    }
}

impl Drop for SlowLog {
    fn drop(&mut self) {
        if (self.t0.elapsed().as_millis() as f32) < knob("SLOW_MS", SLOW_FACT_MS) {
            return;
        }
        let phases: Vec<String> = self
            .took
            .get_mut()
            .unwrap()
            .iter()
            .map(|(w, ms)| format!("{w} {ms:.0}"))
            .collect();
        let searched = match self.searched.load(std::sync::atomic::Ordering::Relaxed) {
            0 => String::new(),
            n => format!(", the {n} fewest facts hold searched"),
        };
        eprintln!(
            "slow fact search in {}: {} ms (ms since start: {}; lists: {}; {} keyword terms{searched})",
            self.scope,
            self.t0.elapsed().as_millis(),
            phases.join(", "),
            self.lists.lock().map(|l| l.join(", ")).unwrap_or_default(),
            self.terms.load(std::sync::atomic::Ordering::Relaxed),
        );
    }
}

/// The hits a pool of `pool` index rows per ranked list finds, best first, folded, at most `k`; and whether a
/// ranked list was cut at `pool` rows (a deeper pool may find more). The first `n` hits are the same at every
/// `k` ≥ `n`.
#[allow(clippy::too_many_arguments)]
async fn ranked(
    store: &Store,
    scope: &Scope,
    vis: &crate::Vis,
    pin: Option<&IndexPin>,
    who: &Principal,
    query: &str,
    k: usize,
    valid_at: Option<i64>,
    filter: Option<&Filter>,
    rank: Rank,
    pool: usize,
) -> Result<(Vec<Hit>, bool)> {
    // A request's own date (`[2028-01-01] Email …`) is when it was asked, not what it is about.
    let (asked_on, query) = when::request_date(query);
    if query.trim().is_empty() {
        return Ok((vec![], false));
    }
    let slow = SlowLog {
        scope: scope.to_string(),
        t0: std::time::Instant::now(),
        took: Default::default(),
        lists: Default::default(),
        terms: Default::default(),
        searched: Default::default(),
    };
    // The time the query names, its relative words ("last year") read as of the day it was asked, else the
    // search's date (`as_of`), else today. `AMPUP_MEMORY_RANK_TIME=0` ignores it.
    let today = asked_on
        .or_else(|| {
            rank.as_of
                .filter(|&t| t > 0 && t < i64::MAX)
                .and_then(chrono::DateTime::from_timestamp_micros)
                .map(|d| d.date_naive())
        })
        .unwrap_or_else(|| chrono::Utc::now().date_naive());
    let window = when::window(query, today).filter(|_| !rank.classic && knob("TIME", 1.0) > 0.0);
    let mut ordered = fact_terms(query);
    // The years of the time the query names are the time term's to rank by: as words they match every fact of
    // any date that writes the same digits. (`TIME_WORDS=2` drops a month's day numbers too; no better.)
    let time_words = knob("TIME_WORDS", 1.0) as i32;
    if window.is_some() && time_words > 0 {
        ordered.retain(|t| {
            let n: Option<u32> = t
                .parse()
                .ok()
                .filter(|_| t.bytes().all(|b| b.is_ascii_digit()));
            match n {
                Some(y) if t.len() == 4 && (1990..=2100).contains(&y) => false,
                Some(d) if time_words == 2 && t.len() <= 2 && (1..=31).contains(&d) => false,
                _ => true,
            }
        });
    }
    let q_terms: HashSet<String> = ordered.iter().cloned().collect();
    slow.terms
        .store(q_terms.len(), std::sync::atomic::Ordering::Relaxed);
    // The query's words as the full-text index holds them (stemmed, stop words out), to tell which facts the
    // keyword ranking matched for a reason: `exports` and `owned` match a query for `export owner`.
    let q_stems: HashSet<String> = stems(&ordered.join(" "));
    // Shared by `consider` and the fetch loop, which checks how many visible hits it has.
    let hits: std::sync::Mutex<HashMap<String, Hit>> = Default::default();
    let prepare = |fact: &Fact, commit: &str| {
        if !vis.allows(commit, fact, who, valid_at) || filter.is_some_and(|flt| !flt.matches(fact))
        {
            return None;
        }
        Some(
            stems(&format!(
                "{} {}",
                fact_text(&fact.entity, &fact.predicate, &fact.value),
                fact.quote.as_deref().unwrap_or_default()
            ))
            .intersection(&q_stems)
            .count(),
        )
    };
    // A fact the principal may see, found by a keyword ranking (with its score there, and which one) or by
    // vector (with its distance), or both. Preparation is shared across lists for the same physical row.
    let consider = |fact: std::borrow::Cow<'_, Fact>,
                    commit: &str,
                    matched: usize,
                    kw: Option<(usize, f32)>,
                    distance: Option<f32>,
                    from_tail: bool| {
        let kw = kw.filter(|_| matched > 0);
        if kw.is_none() && distance.is_none() {
            return;
        }
        let mut hits = hits.lock().unwrap();
        // The engine's extraction by model; anyone else only as "posted": a reader learns where a fact
        // came from, not who (an email, a service key's name) wrote it.
        let h = hits.entry(fact.fact_id.clone()).or_insert_with(|| Hit {
            written_by: vis.actor(commit).map(|a| match a.starts_with("extract:") {
                true => a.to_string(),
                false => "posted".to_string(),
            }),
            fact: vis.annotate(fact.into_owned(), who),
            terms_matched: matched,
            bm25: 0.0,
            distance: None,
            score: 0.0,
            recency: 1.0,
            from_tail,
            signals: [0.0; 3],
            in_window: false,
            group: vec![],
        });
        if let Some((i, b)) = kw {
            h.signals[i] = h.signals[i].max(b);
        }
        if let Some(d) = distance {
            h.distance = Some(h.distance.map_or(d, |x| x.min(d)));
        }
    };

    // Embedding and storage reads are independent. Record each completion separately so a slow search
    // still says which one it waited for, even when they finish out of order.
    // `tail` is every current file the index doesn't hold, read directly below. Index rows are
    // trusted only for files that are still current with the same deletes.
    let (q_vec, found) = tokio::join!(
        async {
            let v = embed_text(query).await?;
            slow.mark("embed");
            Ok::<_, anyhow::Error>(Float32Array::from(v))
        },
        async {
            let index = index_for(store, scope, pin).await?;
            slow.mark("index");
            let (current, tail) = store.fact_files(scope, &index.held).await?;
            slow.mark("files");
            Ok::<_, anyhow::Error>((index, current, tail))
        }
    );
    let q_vec = q_vec?;
    let (index, current, tail) = found?;
    let current = index.trusted(current);
    let cols: Vec<&str> = FTS_FIELDS.iter().map(|f| f.0).collect();
    let keep = fewest(
        index.open.as_ref(),
        &cols,
        &ordered,
        max_terms(),
        term_rows_budget(),
    )
    .await;
    if let Some(k) = &keep {
        slow.searched
            .store(k.len(), std::sync::atomic::Ordering::Relaxed);
    }
    let keyword = keyword_queries(&ordered, rank.classic, keep.as_ref())?;
    // Open rows are searched without a prefilter. Closed rows (the small part) are prefiltered
    // to those still valid at `valid_at`; at the end of time (`i64::MAX`, "current") none can
    // be, so that part is skipped.
    let user_sql = filter.map(Filter::to_sql);
    let parts: Vec<(&Dataset, Option<String>)> = [
        index.open.as_ref().map(|d| (d, user_sql.clone())),
        index
            .closed
            .as_ref()
            .filter(|_| valid_at != Some(i64::MAX))
            .map(|d| {
                let time = valid_at.map(|t| {
                    format!("(valid_from IS NULL OR valid_from <= {t}) AND valid_to > {t}")
                });
                let both = match (time, &user_sql) {
                    (Some(t), Some(u)) => Some(format!("({t}) AND ({u})")),
                    (t, u) => t.or(u.clone()),
                };
                (d, both)
            }),
    ]
    .into_iter()
    .flatten()
    .collect();
    // Supersession: under now/as_of, a hit on a one-valued attribute is dropped when the reader
    // can see a newer version of it (see `newest_versions`). Decided per reader at read time,
    // never in the index, so a version one principal can't see never hides one they can.
    let one = |f: &Fact| {
        valid_at.is_some()
            && vis.schema.as_ref().is_some_and(|s| {
                s.card(f.entity_type.as_deref(), &f.predicate) == crate::query::schema::Card::One
            })
    };
    let mut newest: HashMap<VersionKey, (i64, String)> = HashMap::new();
    let mut looked_up: std::collections::BTreeSet<VersionKey> = Default::default();
    let superseded = |f: &Fact, newest: &HashMap<VersionKey, (i64, String)>| {
        one(f)
            && newest
                .get(&(f.entity.clone(), f.predicate.clone()))
                .is_some_and(|(from, id)| (*from, id) > (f.valid_from, &f.fact_id))
    };
    // Looks up the keys of one-valued hits not looked up yet; returns how many hits are current.
    let settle = async |newest: &mut HashMap<VersionKey, (i64, String)>,
                        looked_up: &mut std::collections::BTreeSet<VersionKey>|
           -> Result<usize> {
        let todo: std::collections::BTreeSet<VersionKey> = hits
            .lock()
            .unwrap()
            .values()
            .filter(|h| one(&h.fact))
            .map(|h| (h.fact.entity.clone(), h.fact.predicate.clone()))
            .filter(|key| !looked_up.contains(key))
            .collect();
        if !todo.is_empty() {
            let t = valid_at.unwrap_or(i64::MAX);
            newest.extend(newest_versions(store, scope, vis, who, &todo, t, None).await?);
            looked_up.extend(todo);
        }
        Ok(hits
            .lock()
            .unwrap()
            .values()
            .filter(|h| !superseded(&h.fact, newest))
            .count())
    };
    // Index rows as candidates: each with its keyword score (`_score`, when the batch is a keyword ranking)
    // or its distance (`_distance`). Keyword hits are scored by vector too, from their stored embedding, so
    // every candidate has both signals: otherwise a fact that only keywords find (outside the nearest pool,
    // say behind hundreds of near-duplicates of one entity) is outvoted by any fact both lists happen to hold.
    let q = q_vec.values();
    // Never share preparation across requests: visibility and matches depend on the principal and query.
    let prepared: std::sync::Mutex<PreparedCandidates> = Default::default();
    // `signal`: which keyword ranking the rows are (see `keyword_queries`), none for the vector one; each row
    // with its score in that ranking, or its distance.
    let take = |rows: &[(Arc<FactRow>, Option<f32>)], signal: Option<usize>| {
        let mut prepared = prepared.lock().unwrap();
        for (row, x) in rows {
            if !current.contains(&row.file) {
                continue;
            }
            let Some((matched, d)) = prepared.for_list(row, q, signal.is_some(), *x, prepare)
            else {
                continue;
            };
            let kw = signal.zip(*x);
            consider(
                std::borrow::Cow::Borrowed(&row.fact),
                &row.commit,
                matched,
                kw,
                d,
                false,
            );
        }
    };
    let fts = |q: &lance_index::scalar::inverted::query::FtsQuery| {
        FullTextSearchQuery::new_query(q.clone())
    };
    let fetch = pool;
    // Whether some ranked list was cut at `fetch` rows (more candidates exist past it).
    let mut saturated = false;
    // Every ranked list of every part is one query of the part's indexes for row ids, and all run at once. The
    // rows come from the row cache after (see `fact_rows`), each part's missing ones in one read, and are
    // taken list by list in this order.
    let mut queries = vec![];
    for (part, (ds, filter)) in parts.iter().enumerate() {
        // Over-fetch: visibility filtering happens after ranking. The keyword limit must be on
        // the query, which makes the index return the BM25 top-k; a scanner limit would
        // truncate an unranked match stream instead.
        // Each keyword ranking and the vector one; with a time named, also the best keyword and nearest
        // matches stated in it, so the facts of that time are candidates however many others match better.
        let mut lists: Vec<(Option<usize>, Option<String>)> = (0..keyword.len())
            .map(Some)
            .chain([None])
            .map(|s| (s, filter.clone()))
            .collect();
        if let Some(w) = &window {
            let only = format!("valid_from >= {} AND valid_from < {}", w.from, w.to);
            let only = match filter {
                Some(f) => format!("({f}) AND ({only})"),
                None => only,
            };
            let first = (!keyword.is_empty()).then_some(Some(0));
            lists.extend(
                first
                    .into_iter()
                    .chain([None])
                    .map(|s| (s, Some(only.clone()))),
            );
        }
        for (signal, filter) in lists {
            let mut scan = ds.scan();
            if let Some(f) = &filter {
                scan.filter(f)?.prefilter(true);
            }
            match signal {
                None => {
                    probe(scan.nearest("vec", &q_vec, fetch)?);
                }
                Some(s) => {
                    scan.full_text_search(fts(&keyword[s].0).limit(Some(fetch as i64)))?;
                }
            }
            let q_vec = &q_vec;
            let log = &slow.lists;
            queries.push(async move {
                let t = std::time::Instant::now();
                // A part with no vector index is searched by its embeddings held in memory.
                let flat = match signal {
                    None => rows::nearest_flat(ds, "vec", q_vec, fetch, filter.as_deref()).await?,
                    Some(_) => None,
                };
                let how = match (signal, flat.is_some()) {
                    (Some(s), _) => format!("keyword {s}"),
                    (None, true) => "flat".to_string(),
                    (None, false) => "vector".to_string(),
                };
                let ids = match flat {
                    Some(ids) => ids,
                    None => rows::ids(scan).await?,
                };
                if let Ok(mut l) = log.lock() {
                    l.push(format!(
                        "part {part} {how} {} ms {} rows",
                        t.elapsed().as_millis(),
                        ids.len()
                    ));
                }
                anyhow::Ok((part, signal, ids))
            });
        }
    }
    let lists = futures::future::try_join_all(queries).await?;
    slow.mark("lists");
    let fetched = futures::future::try_join_all(parts.iter().enumerate().map(|(part, (ds, _))| {
        let ids: Vec<u64> = lists
            .iter()
            .filter(|l| l.0 == part)
            .flat_map(|l| l.2.iter().map(|(id, _)| *id))
            .collect();
        let log = &slow.lists;
        async move {
            let t = std::time::Instant::now();
            let asked = ids.len();
            let (rows, taken) = fact_rows_counted(ds, ids).await?;
            if let Ok(mut l) = log.lock() {
                l.push(format!(
                    "part {part} rows {} ms {taken} of {asked} taken from storage",
                    t.elapsed().as_millis()
                ));
            }
            anyhow::Ok(rows)
        }
    }))
    .await?;
    slow.mark("rows");
    for (part, signal, ids) in &lists {
        saturated |= ids.len() >= fetch;
        let rows = &fetched[*part];
        let listed: Vec<(Arc<FactRow>, Option<f32>)> =
            ids.iter().map(|(id, x)| (rows[id].clone(), *x)).collect();
        take(&listed, *signal);
    }
    // Candidates the any-terms keyword ranking didn't reach (the vector list found them) that share a word
    // with the query: their keyword score too, from the index (a keyword search over just them), so both
    // signals rank every candidate.
    let unscored: Vec<Lit> = hits
        .lock()
        .unwrap()
        .values()
        .filter(|h| h.signals[0] == 0.0 && h.terms_matched > 0 && !h.from_tail)
        .map(|h| Lit::Str(h.fact.fact_id.clone()))
        .collect();
    if !unscored.is_empty() && !keyword.is_empty() && !rank.classic {
        let only = Filter::In(Col::FactId, unscored.clone()).to_sql();
        let scored = futures::future::try_join_all(parts.iter().map(|(ds, _)| {
            let (only, n, any_terms) = (&only, unscored.len(), &keyword[0].0);
            async move {
                let mut scan = ds.scan();
                scan.filter(only)?.prefilter(true);
                scan.full_text_search(fts(any_terms).limit(Some(n as i64)))?;
                let ids = rows::ids(scan).await?;
                let rows = fact_rows(ds, ids.iter().map(|(id, _)| *id).collect()).await?;
                anyhow::Ok(
                    ids.iter()
                        .map(|(id, x)| (rows[id].clone(), *x))
                        .collect::<Vec<_>>(),
                )
            }
        }))
        .await?;
        for rows in &scored {
            take(rows, Some(0));
        }
    }
    slow.mark("rescored");

    // The unindexed tail: newer, rewritten or newly-deleted-from data files, scanned directly.
    // Its facts are embedded here too, unless there are more than TAIL_EMBED_MAX of them (after a
    // compaction, say), when they are found by keyword only until the next index update.
    let tail_facts: Vec<(String, Fact)> = tail
        .iter()
        .flat_map(|(_, rows)| rows.iter())
        .flat_map(|b| crate::decode_facts(b, "source_episodes"))
        .collect();
    let tail_vecs = if tail_facts.len() <= TAIL_EMBED_MAX && !tail_facts.is_empty() {
        let texts: Vec<String> = tail_facts
            .iter()
            .map(|(_, f)| fact_text(&f.entity, &f.predicate, &f.value))
            .collect();
        Some(embed_tail(&texts).await?)
    } else {
        None
    };
    for (i, (commit, fact)) in tail_facts.into_iter().enumerate() {
        // Squared L2, as Lance reports `_distance`, so tail and index rows rank on one scale.
        let d = tail_vecs.as_ref().map(|v| {
            v[i].iter()
                .zip(q_vec.values().iter())
                .map(|(a, b)| (a - b) * (a - b))
                .sum::<f32>()
        });
        if let Some(matched) = prepare(&fact, &commit) {
            consider(
                std::borrow::Cow::Owned(fact),
                &commit,
                matched,
                Some((0, 0.0)),
                d,
                true,
            );
        }
    }
    slow.mark("tail");
    settle(&mut newest, &mut looked_up).await?;
    slow.mark("settled");

    // Keyword ranking is by BM25 (IDF-weighted: a rare word counts for more than a common one).
    // Rows from the unindexed tail have no BM25 yet; they get the share of the query's
    // informative terms they contain, times the best indexed score, so a fresh exact match
    // ranks with the best hits.
    // ponytail: the tail estimate ignores IDF; keep index-wide term stats if fresh rows misrank.
    let mut out: Vec<Hit> = hits
        .into_inner()
        .unwrap()
        .into_values()
        .filter(|h| !superseded(&h.fact, &newest))
        .collect();
    for h in out.iter_mut() {
        h.bm25 = keyword.iter().zip(h.signals).map(|((_, w), s)| w * s).sum();
    }
    let top = out
        .iter()
        .filter(|h| !h.from_tail)
        .map(|h| h.bm25)
        .fold(1.0_f32, f32::max);
    let n_terms = q_terms.len().max(1) as f32;
    for h in out.iter_mut().filter(|h| h.from_tail) {
        h.bm25 = top * h.terms_matched as f32 / n_terms;
    }
    // Fuse the keyword and vector signals. Each is a fact's keyword score (BM25F, see `keyword_queries`) and
    // its cosine similarity (1 - d/2 for unit vectors at squared L2 distance d), optionally scaled by recency
    // (`AMPUP_MEMORY_RANK_RECENCY`, off by default: the time a query names is the time term, below).
    let recency = rank.classic || knob("RECENCY", 0.0) > 0.0;
    for h in out.iter_mut() {
        h.recency = match recency {
            true => recency_bias(h.fact.valid_from, rank.as_of),
            false => 1.0,
        };
    }
    let kw = |h: &Hit| h.bm25 * h.recency;
    let sim = |h: &Hit| h.distance.map(|d| (1.0 - d / 2.0).max(0.0) * h.recency);
    let tiebreak = |a: &Hit, b: &Hit| {
        b.terms_matched
            .cmp(&a.terms_matched)
            .then(a.fact.fact_id.cmp(&b.fact.fact_id))
    };
    let n_kw = out.iter().filter(|h| h.bm25 > 0.0).count();
    let n_sem = out.iter().filter(|h| h.distance.is_some()).count();
    // A convex combination of the min-max normalized scores, `ALPHA` of meaning and the rest keyword (a side
    // a fact lacks counts 0); with fewer than 3 hits in a list its scores say little, and reciprocal-rank
    // fusion (k = 60) ranks instead.
    let alpha = knob("ALPHA", 0.4);
    if n_kw >= 3 && n_sem >= 3 && !rank.classic && knob("RRF", 0.0) == 0.0 {
        let span = |v: &mut dyn Iterator<Item = f32>| {
            v.fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), x| {
                (lo.min(x), hi.max(x))
            })
        };
        let (klo, khi) = span(&mut out.iter().filter(|h| h.bm25 > 0.0).map(kw));
        let (slo, shi) = span(&mut out.iter().filter_map(sim));
        let norm = |x: f32, lo: f32, hi: f32| if hi > lo { (x - lo) / (hi - lo) } else { 1.0 };
        for h in out.iter_mut() {
            let k = if h.bm25 > 0.0 {
                norm(kw(h), klo, khi)
            } else {
                0.0
            };
            let s = sim(h).map_or(0.0, |x| norm(x, slo, shi));
            h.score = alpha * s + (1.0 - alpha) * k;
        }
    } else {
        out.sort_by(|a, b| kw(b).total_cmp(&kw(a)).then(tiebreak(a, b)));
        for (r, h) in out.iter_mut().filter(|h| h.bm25 > 0.0).enumerate() {
            h.score += 1.0 / (RRF_K + r as f32);
        }
        let sem = |h: &Hit| sim(h).unwrap_or(f32::NEG_INFINITY);
        out.sort_by(|a, b| sem(b).total_cmp(&sem(a)).then(tiebreak(a, b)));
        for (r, h) in out.iter_mut().filter(|h| h.distance.is_some()).enumerate() {
            h.score += 1.0 / (RRF_K + r as f32);
        }
    }
    // The time the query names: a fact stated in it, or whose value names one of its years, is in time; one
    // whose value names only other years counts `TIME_DAMP` as much. In-time facts then rank first
    // (`TIME_MODE` 1, keeping their order) or have their score raised by `TIME_BOOST` (`TIME_MODE` 2).
    if let Some(w) = &window {
        let damp = knob("TIME_DAMP", 0.7);
        let boost = knob("TIME_BOOST", 0.3);
        let mode = knob("TIME_MODE", 1.0) as i32;
        for h in out.iter_mut() {
            // The time the value itself states (a date, a month, a year), if any.
            let stated = when::stated(&h.fact.value);
            let named = stated.as_ref().is_some_and(|s| s.overlaps(w));
            h.in_window = w.contains(h.fact.valid_from) || named;
            if stated.is_some() && !named {
                h.score *= damp;
            }
            if mode == 2 && h.in_window {
                h.score *= 1.0 + boost;
            }
        }
        if mode == 1 {
            // In time first, each side in score order.
            for h in out.iter_mut().filter(|h| h.in_window) {
                h.score += 1.0;
            }
        }
    }
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(kw(b).total_cmp(&kw(a)))
            .then(tiebreak(a, b))
    });
    // A dream's names of a thing (`_name`, `_alias`) are for finding it by another name (`match`, a passage search's
    // name variants), not what it holds: never a hit, nor a group's member (below), where they would push out what
    // the thing's facts say.
    out.retain(|h| !crate::dream_name(&h.fact));
    // Near-duplicates: facts about the same entity that say the same thing (the same value but for case,
    // spacing and punctuation) show once, the best ranked; the others come with it in a group, or not at all.
    let same = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let mut kept: Vec<Hit> = Vec::with_capacity(out.len());
    // The hits each kept hit folded in by a dream's link (other words, so other facts): should the kept hit be shown
    // only in another hit's group, they are hits again in its place, never lost with it.
    let mut folded: Vec<Vec<Hit>> = vec![];
    // How many of each kept hit's sources are its own (a fold adds the folded facts' sources after them).
    let mut own: Vec<usize> = vec![];
    if !rank.classic && knob("DEDUP", 1.0) > 0.0 {
        // So do facts a dream linked as duplicates, where this reader may read the link: each joins its group, and
        // the one shown cites the others' sources too (both citations kept).
        let linked = vis.duplicates(who);
        let mut seen: HashMap<(String, String), usize> = HashMap::new();
        for h in out {
            let by_value = (h.fact.entity.clone(), same(&h.fact.value));
            let by_link = linked
                .get(&h.fact.fact_id)
                .map(|g| (String::new(), g.clone()));
            let (at, link) = match seen.get(&by_value) {
                Some(i) => (Some(*i), false),
                None => match by_link.as_ref().and_then(|k| seen.get(k)) {
                    Some(&i) => {
                        let shown = &mut kept[i].fact.source_episodes;
                        for e in &h.fact.source_episodes {
                            if !shown.contains(e) {
                                shown.push(e.clone());
                            }
                        }
                        (Some(i), true)
                    }
                    None => (None, false),
                },
            };
            let at = match at {
                Some(i) => {
                    kept[i].group.push(h.fact.clone());
                    if link {
                        folded[i].push(h);
                    }
                    i
                }
                None => {
                    own.push(h.fact.source_episodes.len());
                    kept.push(h);
                    folded.push(vec![]);
                    kept.len() - 1
                }
            };
            seen.entry(by_value).or_insert(at);
            if let Some(k) = by_link {
                seen.entry(k).or_insert(at);
            }
        }
    } else {
        folded = out.iter().map(|_| vec![]).collect();
        own = out.iter().map(|h| h.fact.source_episodes.len()).collect();
        kept = out;
    }
    if rank.group == 0 {
        for h in kept.iter_mut() {
            h.group.clear();
        }
        kept.truncate(k);
        return Ok((kept, saturated));
    }
    // Groups: each hit brings the other facts about its entity from its source episodes, then the other
    // items of its list (same entity and predicate), best ranked first, up to `rank.group`; a fact shown in
    // a group isn't shown again, and a group is one of the `k`. Each group takes at most `rank.group` other
    // hits, so the `k`-th group's hit is among the first (group + 1) * k: their entities are every group's,
    // and a group is the same at every `k` that shows it.
    let mut entities: Vec<Lit> = vec![];
    let mut seen: HashSet<&str> = HashSet::new();
    for h in kept.iter().take((rank.group + 1) * k + 10) {
        let e = Lit::Str(h.fact.entity.clone());
        if seen.insert(&h.fact.entity) {
            entities.push(e);
        }
    }
    let mut about: HashMap<String, Vec<Fact>> = HashMap::new();
    if !entities.is_empty() {
        for (commit, f) in scan(store, scope, &Filter::In(Col::Entity, entities), false).await? {
            let key = (f.entity.clone(), f.predicate.clone());
            if crate::dream_name(&f)
                || !vis.allows(&commit, &f, who, valid_at)
                || filter.is_some_and(|flt| !flt.matches(&f))
                || (one(&f) && (!looked_up.contains(&key) || superseded(&f, &newest)))
            {
                continue;
            }
            about
                .entry(f.entity.clone())
                .or_default()
                .push(vis.annotate(f, who));
        }
    }
    slow.mark("grouped");
    let rank_of: HashMap<String, usize> = kept
        .iter()
        .enumerate()
        .map(|(i, h)| (h.fact.fact_id.clone(), i))
        .collect();
    let mut shown: HashSet<String> = HashSet::new();
    let mut groups: Vec<Hit> = vec![];
    // `GROUP_POOL`: only facts the search itself found (by keyword or meaning) join a group.
    let pooled = knob("GROUP_POOL", 0.0) > 0.0;
    let mut queue: std::collections::VecDeque<(Hit, Vec<Hit>, usize)> = kept
        .into_iter()
        .zip(folded)
        .zip(own)
        .map(|((h, f), n)| (h, f, n))
        .collect();
    while let Some((mut h, linked, own)) = queue.pop_front() {
        if groups.len() >= k {
            break;
        }
        if !shown.insert(h.fact.fact_id.clone()) {
            // Shown already, in a better hit's group: what a dream's link folded into it would go unseen with it, so
            // the first of them is a hit here instead (it leads its own group), the others folded into it.
            let mut rest: Vec<Hit> = linked
                .into_iter()
                .filter(|d| !shown.contains(&d.fact.fact_id))
                .collect();
            if !rest.is_empty() {
                let mut first = rest.remove(0);
                first.group = rest.iter().map(|d| d.fact.clone()).collect();
                let n = first.fact.source_episodes.len();
                queue.push_front((first, rest, n));
            }
            continue;
        }
        // What a dream's link folded in comes with the hit without taking a slot of its group: the hit cites it,
        // and it doesn't push out what else the thing holds.
        let free: HashSet<String> = linked.iter().map(|d| d.fact.fact_id.clone()).collect();
        let dups = std::mem::take(&mut h.group);
        // The facts its own sources cite join it first, not those of what it folded in.
        let sources: HashSet<&String> = h.fact.source_episodes.iter().take(own).collect();
        let mut more: Vec<(u8, usize, &Fact)> = about
            .get(&h.fact.entity)
            .into_iter()
            .flatten()
            .filter(|f| f.fact_id != h.fact.fact_id && !shown.contains(&f.fact_id))
            .filter_map(|f| {
                let cited = f.source_episodes.iter().any(|e| sources.contains(e));
                let listed = f.predicate == h.fact.predicate;
                let tier = match (cited, listed) {
                    (true, true) => 0,
                    (true, false) => 1,
                    (false, true) => 2,
                    (false, false) => return None,
                };
                let at = rank_of.get(&f.fact_id).copied();
                if pooled && at.is_none() {
                    return None;
                }
                Some((tier, at.unwrap_or(usize::MAX), f))
            })
            .collect();
        more.sort_by(|a, b| {
            (a.0, a.1)
                .cmp(&(b.0, b.1))
                .then(b.2.valid_from.cmp(&a.2.valid_from))
                .then(a.2.fact_id.cmp(&b.2.fact_id))
        });
        let mut group: Vec<Fact> = vec![];
        let mut slots = 0;
        for f in dups
            .into_iter()
            .chain(more.into_iter().map(|(_, _, f)| f.clone()))
        {
            let linked = free.contains(&f.fact_id);
            if slots >= rank.group && !linked {
                continue;
            }
            if shown.insert(f.fact_id.clone()) {
                slots += usize::from(!linked);
                group.push(f);
            }
        }
        h.group = group;
        groups.push(h);
    }
    Ok((groups, saturated))
}

/// Every fact row with this key, as (commit id, fact), before visibility filtering: from the
/// index's BTree key index for files it holds that are still current, and by scanning the
/// unindexed tail for the rest.
pub async fn key_lookup(store: &Store, scope: &Scope, key: Key<'_>) -> Result<Vec<(String, Fact)>> {
    let index = open_cached(store, scope).await?;
    let (current, tail) = store.fact_files(scope, &index.held).await?;
    let current = index.trusted(current);
    let mut out = vec![];
    let literal = key.text().replace('\'', "''");
    for ds in [&index.open, &index.closed].into_iter().flatten() {
        // The key index finds the row ids; the rows come from the row cache.
        let mut scan = ds.scan();
        scan.filter(&format!("{} = '{literal}'", key.column()))?;
        let ids: Vec<u64> = rows::ids(scan)
            .await?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let rows = keyed_rows(ds, ids.clone()).await?;
        for id in &ids {
            let r = &rows[id];
            // Re-check the real fields: a value containing the separator could otherwise
            // make one key column value collide with another key's.
            if current.contains(&r.file) && key.matches(&r.fact) {
                out.push((r.commit.clone(), r.fact.clone()));
            }
        }
    }
    for (_, rows) in &tail {
        for b in rows.iter() {
            // Match on the raw columns first; decode only the matching rows.
            let col = |n: &str| b.column_by_name(n).unwrap().as_string::<i32>();
            let (a, c, want_a, want_c) = match key {
                Key::Entity(e, p) => (col("entity"), col("predicate"), e, p),
                Key::Value(p, v) => (col("predicate"), col("value"), p, v),
            };
            let mask = arrow_array::BooleanArray::from_iter(
                (0..b.num_rows()).map(|i| Some(a.value(i) == want_a && c.value(i) == want_c)),
            );
            if mask.true_count() > 0 {
                let hits = arrow_select::filter::filter_record_batch(b, &mask)?;
                out.extend(crate::decode_facts(&hits, "source_episodes"));
            }
        }
    }
    Ok(out)
}

/// Bytes every index of namespace `ns` takes under the store's index location (0: none).
pub async fn namespace_index_bytes(store: &Store, ns: &str) -> Result<u64> {
    anyhow::ensure!(
        !ns.is_empty() && !ns.contains('/') && !ns.contains(".."),
        "bad namespace {ns:?}"
    );
    bytes_under(&format!(
        "{}/{ns}",
        store.index_base().trim_end_matches('/')
    ))
    .await
}

/// Bytes of every file under `dir` (a local path, `file://…`, `gs://…` or `s3://…`); 0 when there is none.
pub async fn bytes_under(dir: &str) -> Result<u64> {
    let local = dir.strip_prefix("file://").unwrap_or(dir);
    if !local.contains("://") {
        fn walk(p: &std::path::Path) -> u64 {
            match std::fs::read_dir(p) {
                Err(_) => 0,
                Ok(rd) => rd
                    .flatten()
                    .map(|e| match e.metadata() {
                        Ok(m) if m.is_dir() => walk(&e.path()),
                        Ok(m) => m.len(),
                        Err(_) => 0,
                    })
                    .sum(),
            }
        }
        return Ok(walk(std::path::Path::new(local)));
    }
    let (os, path) = lance::io::ObjectStore::from_uri(dir).await?;
    let mut n = 0;
    let mut files = os.read_dir_all(&path, None);
    while let Some(f) = files.try_next().await? {
        n += f.size;
    }
    Ok(n)
}

/// The names of the directories directly under `dir`, sorted; none when it doesn't exist.
pub async fn subdirs(dir: &str) -> Result<Vec<String>> {
    let local = dir.strip_prefix("file://").unwrap_or(dir);
    let mut out: Vec<String> = if !local.contains("://") {
        match std::fs::read_dir(local) {
            Err(_) => vec![],
            Ok(rd) => rd
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().to_str().map(String::from))
                .collect(),
        }
    } else {
        let (os, path) = lance::io::ObjectStore::from_uri(dir).await?;
        os.read_dir(path).await.unwrap_or_default()
    };
    out.sort();
    Ok(out)
}

/// Delete `dir` and everything under it, and forget this process's open handles there.
pub async fn remove_dir(dir: &str) -> Result<()> {
    // Rows of the deleted indexes (forgotten memory among them) leave memory with them.
    forget_cached();
    let local = dir.strip_prefix("file://").unwrap_or(dir);
    let removed = if !local.contains("://") {
        match std::fs::remove_dir_all(local) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("delete index {local}"))
            }
            _ => Ok(()),
        }
    } else {
        let (os, path) = lance::io::ObjectStore::from_uri(dir).await?;
        match os.remove_dir_all(path).await {
            Err(e) if !e.to_string().to_lowercase().contains("not found") => {
                Err(e).with_context(|| format!("delete index {dir}"))
            }
            _ => Ok(()),
        }
    };
    // Again once the files are gone: a read during the delete may have opened and cached them meanwhile.
    forget_cached();
    removed
}

/// Forget what this process holds of every index: open handles, cached rows and vectors, and the Lance
/// caches (all of them: one deleted index is rare, and the next reads load the rest again). Lance can't clear a cache, so the session is replaced, and every open handle is dropped
/// with it (they reopen in the new session), so only one bounded session is ever in use.
fn forget_cached() {
    drop_open(|_| true);
    FACT_ROWS.clear();
    KEYED_ROWS.clear();
    rows::clear_flat();
    passages::forget_open("");
    new_caches();
}

/// Delete every index of namespace `ns` under the store's index location (fact and passage indexes, every
/// layout and embedding model), and forget this process's open handles and cached vectors. Indexes are
/// derived data: the next `update` rebuilds them from the tables. Used when memory is deleted for good
/// (`memstore org delete`, `memstore users forget`), since a Lance dataset keeps deleted rows in older
/// versions until it is compacted and cleaned up.
pub async fn drop_namespace_index(store: &Store, ns: &str) -> Result<()> {
    anyhow::ensure!(
        !ns.is_empty() && !ns.contains('/') && !ns.contains(".."),
        "bad namespace {ns:?}"
    );
    let dir = format!("{}/{ns}", store.index_base().trim_end_matches('/'));
    TAIL_CACHE.lock().unwrap().clear();
    remove_dir(&dir).await
}

#[cfg(test)]
mod index_tests {
    use super::*;
    use arrow_array::{FixedSizeListArray, StringArray, types::Float32Type};

    fn rows(from: usize, n: usize) -> (Arc<Schema>, RecordBatch) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("text", DataType::Utf8, false),
            Field::new(
                "vec",
                DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), 8),
                false,
            ),
        ]));
        let text =
            StringArray::from_iter_values((from..from + n).map(|i| format!("note {i} word{i}")));
        let vec = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
            (from..from + n).map(|i| Some((0..8).map(move |d| Some(((i * 7 + d) % 13) as f32)))),
            8,
        );
        let b = RecordBatch::try_new(schema.clone(), vec![Arc::new(text), Arc::new(vec)]).unwrap();
        (schema, b)
    }

    /// The memory log counts a buffer once however many batches share it: a batch and a slice of it hold
    /// what the batch alone does, and a second batch adds its own.
    #[test]
    fn shared_buffers_count_once() {
        let (_, a) = rows(0, 300);
        let (_, b) = rows(300, 300);
        let alone = crate::distinct_bytes([&a].into_iter(), &mut HashSet::new());
        assert!(alone > 300 * 8 * 4, "{alone}");
        let slice = a.slice(10, 20);
        let with_slice = crate::distinct_bytes([&a, &slice].into_iter(), &mut HashSet::new());
        assert_eq!(with_slice, alone);
        let both = crate::distinct_bytes([&a, &b].into_iter(), &mut HashSet::new());
        assert!(both > alone);
    }

    /// Every index dataset opens in the one session, so one pair of bounded caches holds what all of them
    /// load: two datasets, written and reopened, share it, and what their indexes loaded stays in its bound.
    #[tokio::test]
    async fn every_index_dataset_shares_one_bounded_session() {
        let dir = std::env::temp_dir().join(format!("memstore-session-{}", uuid::Uuid::new_v4()));
        let mut opened = vec![];
        for name in ["a", "b"] {
            let uri = dir.join(format!("{name}.lance")).display().to_string();
            let (schema, b) = rows(0, 300);
            let mut ds = append_rows(&uri, vec![b], schema).await.unwrap().unwrap();
            build_index(
                &mut ds,
                &["text"],
                IndexType::Inverted,
                "text",
                &InvertedIndexParams::default(),
            )
            .await
            .unwrap();
            let ds = open_ds(&uri).await.unwrap();
            let mut scan = ds.scan();
            scan.full_text_search(lance_index::scalar::FullTextSearchQuery::new(
                "word7".into(),
            ))
            .unwrap();
            scan.try_into_batch().await.unwrap();
            opened.push(ds);
        }
        assert!(Arc::ptr_eq(&opened[0].session(), &opened[1].session()));
        assert!(Arc::ptr_eq(&opened[0].session(), &session()));
        assert!(session().size_bytes() <= (1 << 30) + (256 << 20));

        // Deleted for good, then rebuilt at the same path with other rows: the old caches answer nothing.
        // (In this test, not one of its own: a removal swaps the session other tests would compare.)
        drop(opened);
        let before = session();
        remove_dir(&dir.display().to_string()).await.unwrap();
        assert!(!Arc::ptr_eq(&before, &session()), "the caches were kept");
        let uri = dir.join("a.lance").display().to_string();
        let (schema, b) = rows(1000, 10);
        let ds = append_rows(&uri, vec![b], schema).await.unwrap().unwrap();
        assert_eq!(ds.count_rows(None).await.unwrap(), 10);
        let mut scan = open_ds(&uri).await.unwrap().scan();
        scan.full_text_search(lance_index::scalar::FullTextSearchQuery::new(
            "word7".into(),
        ))
        .unwrap();
        assert!(
            !matches!(scan.try_into_batch().await, Ok(b) if b.num_rows() > 0),
            "a deleted row was served"
        );
    }

    /// A second update indexes only the new rows, beside the existing indexes, instead of training on every
    /// row again; the new rows are found by both indexes, and an update with nothing new does nothing.
    #[tokio::test]
    async fn an_index_takes_new_rows_without_a_rebuild() {
        let dir = std::env::temp_dir().join(format!("memstore-index-{}", uuid::Uuid::new_v4()));
        let uri = dir.join("p.lance").display().to_string();
        let (schema, b) = rows(0, 300);
        let mut ds = append_rows(&uri, vec![b], schema.clone())
            .await
            .unwrap()
            .unwrap();
        let ivf = vec_index(16);
        let fts = InvertedIndexParams::default();
        build_index(&mut ds, &["vec"], IndexType::Vector, "vec", &ivf)
            .await
            .unwrap();
        build_index(&mut ds, &["text"], IndexType::Inverted, "text", &fts)
            .await
            .unwrap();
        let first: HashSet<_> = ds
            .load_indices()
            .await
            .unwrap()
            .iter()
            .map(|i| i.uuid)
            .collect();

        let (_, b) = rows(300, 50);
        let mut ds = append_rows(&uri, vec![b], schema).await.unwrap().unwrap();
        build_index(&mut ds, &["vec"], IndexType::Vector, "vec", &ivf)
            .await
            .unwrap();
        build_index(&mut ds, &["text"], IndexType::Inverted, "text", &fts)
            .await
            .unwrap();

        // The first indexes are still there: the new rows went into deltas, nothing was rebuilt.
        let now = ds.load_indices().await.unwrap();
        assert!(
            first.iter().all(|u| now.iter().any(|i| i.uuid == *u)),
            "an index was rebuilt"
        );
        let hits = ds
            .scan()
            .full_text_search(lance_index::scalar::FullTextSearchQuery::new(
                "word321".into(),
            ))
            .unwrap()
            .try_into_batch()
            .await
            .unwrap();
        assert_eq!(hits.num_rows(), 1, "a new row is found by the text index");
        let q = arrow_array::Float32Array::from_iter_values(
            (0..8).map(|d| ((321 * 7 + d) % 13) as f32),
        );
        let near = ds
            .scan()
            .nearest("vec", &q, 1)
            .unwrap()
            .try_into_batch()
            .await
            .unwrap();
        assert_eq!(near.num_rows(), 1);

        let v = ds.version().version;
        build_index(&mut ds, &["vec"], IndexType::Vector, "vec", &ivf)
            .await
            .unwrap();
        assert_eq!(ds.version().version, v, "nothing new, nothing written");
    }
}
