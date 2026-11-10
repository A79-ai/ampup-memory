//! Passage search: the text of episodes (emails, notes, call summaries, CRM records), chunked
//! and indexed next to the facts index, so a question can be answered from what was said and
//! not only from what was extracted. Each chunk carries its episode's source, time and the
//! entities the episode is about, which filters narrow to before ranking; a chunk is visible
//! exactly when its episode is (committed, and the reader is in its ACL).
//!
//! Like the facts index this one covers whole episodes data files: its rows are trusted only
//! for files still current, and episodes in newer files are chunked and searched from memory.
//! It has its own layout version, so changing it never forces the facts index to re-embed.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use arrow_array::builder::{ListBuilder, StringBuilder};
use arrow_array::{
    Array, ArrayRef, FixedSizeListArray, Float32Array, Int32Array, Int64Array, RecordBatch,
    StringArray, cast::AsArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lance::Dataset;
use lance_index::IndexType;
use lance_index::scalar::{
    BuiltinIndexType, FullTextSearchQuery, InvertedIndexParams, ScalarIndexParams,
};

use crate::search::{self, informative, terms};
use crate::{Principal, Scope, Store, Vis};

const LAYOUT: &str = "passages-v1";
/// Chunks are about this many bytes, cut at a paragraph, sentence or word boundary.
const CHUNK_BYTES: usize = 1500;
/// Consecutive chunks share about this much text, so a sentence cut in two is whole in one.
const OVERLAP_BYTES: usize = 150;
/// IN lists longer than this are checked in memory only, not sent to the index as a prefilter.
const MAX_PREFILTER_IDS: usize = 5000;
/// ...and at most one in this many of the scope's episodes: a reader who sees most of a scope is filtered
/// after the scan (on CRMArena's 4,227 episodes, every reader's 4,227 ids went into both scans' filter).
const PREFILTER_SHARE: usize = 4;
/// A passage search this slow logs its phases.
const SLOW_SEARCH: std::time::Duration = std::time::Duration::from_secs(1);
/// A search hit shows about this much of its chunk: enough to judge it, short enough that
/// twenty fit in one answer. Reading the passage by id gives the whole text.
pub const SNIPPET_BYTES: usize = 400;
/// Reading by id returns at most this much of an episode: all of it, or the chunks around one.
pub const READ_BYTES: usize = 8000;
/// Passage ids are this many characters of the episode id, then `#` and the chunk.
const SHORT_ID: usize = 16;
const TITLE_CHARS: usize = 100;
/// Episode titles by episode id, so repeated searches don't re-read bodies.
static TITLES: std::sync::LazyLock<Mutex<HashMap<String, String>>> =
    std::sync::LazyLock::new(Default::default);
/// Most rows a search without text reads from the index (newest first after filtering).
const MAX_SCAN_ROWS: usize = 20_000;
/// Unindexed episodes' chunks, by episode id, so repeated searches don't re-read bodies.
static TAIL: std::sync::LazyLock<Mutex<HashMap<String, Arc<Vec<String>>>>> =
    std::sync::LazyLock::new(Default::default);
const TAIL_MAX: usize = 20_000;
/// The tail's chunks as the full-text index would hold them (stemmed, stop words out), by the chunk's text:
/// stemming every unindexed chunk again for each search took ~1.2 s on 16,000 chunks.
static TAIL_STEMS: std::sync::LazyLock<Mutex<HashMap<String, Arc<HashSet<String>>>>> =
    std::sync::LazyLock::new(Default::default);
/// Most chunks [`TAIL_STEMS`] holds before it starts over.
const TAIL_STEMS_MAX: usize = 100_000;

fn tail_stems(text: &str) -> Arc<HashSet<String>> {
    if let Some(s) = TAIL_STEMS.lock().unwrap().get(text) {
        return s.clone();
    }
    let s = Arc::new(search::stems(text));
    let mut cache = TAIL_STEMS.lock().unwrap();
    if cache.len() >= TAIL_STEMS_MAX {
        cache.clear();
    }
    cache.insert(text.to_string(), s.clone());
    s
}

/// A candidate passage with its keyword score and vector distance, when it has them.
type Ranked = (Passage, Option<f32>, Option<f32>);

/// One chunk of an episode's text.
#[derive(Clone, Debug, PartialEq)]
pub struct Passage {
    pub episode_id: String,
    pub chunk: i32,
    pub source: String,
    /// The source up to its first `:` (`email`, `crm`, `slack`).
    pub source_kind: String,
    pub occurred_at: i64,
    pub about: Vec<String>,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct PassageHit {
    pub passage: Passage,
    /// Reciprocal-rank fusion of the keyword and vector rankings; 0 for a search without text.
    pub score: f32,
}

/// What a passage must match. Empty means anything.
#[derive(Clone, Debug, Default)]
pub struct PassageFilter {
    /// The passage's episode is about at least one of these entities.
    pub about: Option<BTreeSet<String>>,
    pub source_kinds: Vec<String>,
    pub occurred_from: Option<i64>,
    pub occurred_to: Option<i64>,
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl PassageFilter {
    /// The filter as a Lance prefilter; parts it can't express are left to [`Self::matches`].
    fn to_sql(&self) -> Option<String> {
        let mut parts = vec![];
        if let Some(ids) = &self.about
            && ids.len() <= MAX_PREFILTER_IDS
        {
            let list: Vec<String> = ids.iter().map(|i| lit(i)).collect();
            parts.push(format!("array_has_any(`about`, [{}])", list.join(", ")));
        }
        if !self.source_kinds.is_empty() {
            let list: Vec<String> = self.source_kinds.iter().map(|k| lit(k)).collect();
            parts.push(format!("`source_kind` IN ({})", list.join(", ")));
        }
        if let Some(t) = self.occurred_from {
            parts.push(format!("`occurred_at` >= {t}"));
        }
        if let Some(t) = self.occurred_to {
            parts.push(format!("`occurred_at` <= {t}"));
        }
        (!parts.is_empty()).then(|| {
            parts
                .iter()
                .map(|p| format!("({p})"))
                .collect::<Vec<_>>()
                .join(" AND ")
        })
    }

    pub fn matches(&self, p: &Passage) -> bool {
        self.about
            .as_ref()
            .is_none_or(|ids| p.about.iter().any(|a| ids.contains(a)))
            && (self.source_kinds.is_empty() || self.source_kinds.contains(&p.source_kind))
            && self.occurred_from.is_none_or(|t| p.occurred_at >= t)
            && self.occurred_to.is_none_or(|t| p.occurred_at <= t)
    }
}

/// The chunking a store starts with, as (bytes, overlap): `MEMSTORE_CHUNK_BYTES` and
/// `MEMSTORE_CHUNK_OVERLAP`, else [`CHUNK_BYTES`] and [`OVERLAP_BYTES`].
pub fn default_chunking() -> (usize, usize) {
    let var = |k: &str, d: usize| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n: &usize| n > 0)
            .unwrap_or(d)
    };
    let bytes = var("MEMSTORE_CHUNK_BYTES", CHUNK_BYTES).max(100);
    let overlap = std::env::var("MEMSTORE_CHUNK_OVERLAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(OVERLAP_BYTES)
        .min(bytes / 2);
    (bytes, overlap)
}

/// Split text into chunks of about [`CHUNK_BYTES`], preferring paragraph, then sentence, then
/// line, then word boundaries in the second half of each window, with [`OVERLAP_BYTES`] overlap.
pub fn chunks(body: &str) -> Vec<String> {
    chunks_with(body, (CHUNK_BYTES, OVERLAP_BYTES))
}

/// [`chunks`] with another chunking: (bytes, overlap).
pub fn chunks_with(body: &str, chunking: (usize, usize)) -> Vec<String> {
    let body = body.trim();
    spans_with(body, chunking)
        .into_iter()
        .map(|(a, z)| body[a..z].to_string())
        .collect()
}

/// The byte ranges of `body` that [`chunks_with`] cuts it into (`body` already trimmed).
fn spans_with(body: &str, (chunk_bytes, overlap_bytes): (usize, usize)) -> Vec<(usize, usize)> {
    let n = body.len();
    let mut out = vec![];
    let mut start = 0;
    while start < n {
        let mut end = floor_boundary(body, (start + chunk_bytes).min(n));
        if end < n {
            let window = &body[start..end];
            let min = window.len() / 2;
            if let Some(cut) = ["\n\n", ". ", "? ", "! ", "\n", " "]
                .iter()
                .find_map(|sep| {
                    window
                        .rfind(sep)
                        .filter(|&i| i >= min)
                        .map(|i| i + sep.len())
                })
            {
                end = start + cut;
            }
        }
        if end <= start {
            end = ceil_boundary(body, start + 1);
        }
        let raw = &body[start..end];
        let piece = raw.trim();
        if !piece.is_empty() {
            let a = start + (raw.len() - raw.trim_start().len());
            out.push((a, a + piece.len()));
        }
        if end >= n {
            break;
        }
        // Start the next chunk a little before this one ended, at the start of a word.
        let mut next = ceil_boundary(body, end.saturating_sub(overlap_bytes).max(start + 1));
        if let Some(i) = body[next..end].find(char::is_whitespace) {
            next = ceil_boundary(body, next + i + 1);
        }
        start = next.max(start + 1);
    }
    out
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

/// The passage index's directory name under the scope's index location: its layout, then the embedding
/// provider (except the default local model, see `crate::search::embed`) and the chunking (except the
/// default), so each configuration is an index of its own.
pub fn dir_name(store: &Store) -> String {
    let emb = crate::search::embed::provider().layout_tag();
    let (bytes, overlap) = store.chunking();
    let chunk = if (bytes, overlap) == (CHUNK_BYTES, OVERLAP_BYTES) {
        String::new()
    } else {
        format!("-c{bytes}-{overlap}")
    };
    format!("{LAYOUT}{emb}{chunk}")
}

/// Where the scope's passage index lives in the store's index location.
pub fn dir_uri(store: &Store, scope: &Scope) -> String {
    format!(
        "{}/{}/{}",
        store.index_base().trim_end_matches('/'),
        scope.namespace(),
        dir_name(store)
    )
}

/// What the passage index is built with.
pub fn config(store: &Store) -> search::IndexConfig {
    let (bytes, overlap) = store.chunking();
    search::IndexConfig {
        kind: "passages".into(),
        layout: LAYOUT.into(),
        embedding: crate::search::embed::provider().describe(),
        dims: search::dims(),
        chunk_bytes: Some(bytes),
        chunk_overlap: Some(overlap),
        bm25: search::bm25_settings(),
        dir: dir_name(store),
    }
}

fn uri(store: &Store, scope: &Scope, part: &str) -> String {
    format!("{}/{part}.lance", dir_uri(store, scope))
}

fn schema() -> Arc<Schema> {
    let s = |n: &str| Field::new(n, DataType::Utf8, false);
    Arc::new(Schema::new(vec![
        // The episodes data file's key (path plus delete files) these rows came from.
        s("data_file"),
        s("commit_id"),
        s("episode_id"),
        Field::new("chunk", DataType::Int32, false),
        s("source"),
        s("source_kind"),
        Field::new("occurred_at", DataType::Int64, false),
        Field::new(
            "about",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        ),
        s("text"),
        search::vec_field(),
    ]))
}

async fn open(uri: &str) -> Result<Option<Dataset>> {
    match crate::search::open_ds(uri).await {
        Ok(d) => Ok(Some(d)),
        Err(lance::Error::DatasetNotFound { .. }) => Ok(None),
        Err(e) => Err(e).context("open passage index"),
    }
}

async fn append(
    uri: &str,
    batches: Vec<RecordBatch>,
    schema: Arc<Schema>,
) -> Result<Option<Dataset>> {
    search::append_rows(uri, batches, schema).await
}

/// An episodes table row, as the index needs it.
struct EpisodeRow {
    commit: String,
    id: String,
    source: String,
    occurred_at: i64,
    about: Vec<String>,
}

/// Episodes passage search indexes: all but those whose writer turned text indexing off.
fn episode_rows(b: &RecordBatch) -> Vec<EpisodeRow> {
    rows_of(b, false)
}

/// The rows of an episodes batch, with or without those left out of passage search.
fn rows_of(b: &RecordBatch, unindexed: bool) -> Vec<EpisodeRow> {
    rows_at(b, unindexed).into_iter().map(|(_, r)| r).collect()
}

/// [`rows_of`], each row with its position in the batch.
fn rows_at(b: &RecordBatch, unindexed: bool) -> Vec<(usize, EpisodeRow)> {
    let (commit, id, source) = (
        crate::strings(b, "commit_id"),
        crate::strings(b, "episode_id"),
        crate::strings(b, "source"),
    );
    let at = crate::timestamps(b, "occurred_at");
    // Rows written before episodes had `about` read it as empty, and are indexed.
    let about = crate::optional_strings(b, "about");
    let indexed = crate::optional_bools(b, "index_text");
    (0..b.num_rows())
        .filter(|&i| unindexed || indexed[i] != Some(false))
        .map(|i| {
            (
                i,
                EpisodeRow {
                    commit: commit[i].clone(),
                    id: id[i].clone(),
                    source: source[i].clone(),
                    occurred_at: at[i].unwrap_or_default(),
                    about: crate::split_list(about[i].as_deref().unwrap_or("")),
                },
            )
        })
        .collect()
}

/// The rows of an episodes data file (its batches, in the order the table reads them) that its passages are
/// written from, by position among all its rows: each episode's newest indexed row there. An episode written
/// again (ingested with a new ACL, or back after a newer version of its source superseded it) is another row
/// with the same id and body, so the same passages: a file holding several of them, as a rewrite's one file
/// holds every row of the scope (one production scope's: 40,288 rows of 22,263 episodes, one 34 times), would otherwise
/// write each of its passages as many times.
fn passage_rows(batches: &[RecordBatch]) -> HashSet<usize> {
    let mut newest: HashMap<String, (i64, usize)> = HashMap::new();
    let mut start = 0;
    for b in batches {
        let recorded = crate::timestamps(b, "recorded_at");
        for (i, r) in rows_at(b, false) {
            let at = (recorded[i].unwrap_or(i64::MIN), start + i);
            let e = newest.entry(r.id).or_insert(at);
            *e = (*e).max(at);
        }
        start += b.num_rows();
    }
    newest.into_values().map(|(_, at)| at).collect()
}

fn source_kind(source: &str) -> String {
    source.split(':').next().unwrap_or_default().to_lowercase()
}

fn passage(r: &EpisodeRow, chunk: i32, text: String) -> Passage {
    Passage {
        episode_id: r.id.clone(),
        chunk,
        source: r.source.clone(),
        source_kind: source_kind(&r.source),
        occurred_at: r.occurred_at,
        about: r.about.clone(),
        text,
    }
}

fn to_batch(rows: &[(String, String, Passage)], vecs: Vec<Vec<f32>>) -> Result<RecordBatch> {
    let col = |f: fn(&(String, String, Passage)) -> &str| -> ArrayRef {
        Arc::new(StringArray::from_iter_values(rows.iter().map(f)))
    };
    let mut about = ListBuilder::new(StringBuilder::new());
    for (_, _, p) in rows {
        for a in &p.about {
            about.values().append_value(a);
        }
        about.append(true);
    }
    let vec: ArrayRef = Arc::new(FixedSizeListArray::try_new(
        Arc::new(Field::new("item", DataType::Float32, true)),
        search::dims(),
        Arc::new(Float32Array::from(vecs.concat())),
        None,
    )?);
    Ok(RecordBatch::try_new(
        schema(),
        vec![
            col(|r| &r.0),
            col(|r| &r.1),
            col(|r| &r.2.episode_id),
            Arc::new(Int32Array::from_iter_values(rows.iter().map(|r| r.2.chunk))),
            col(|r| &r.2.source),
            col(|r| &r.2.source_kind),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.2.occurred_at),
            )),
            Arc::new(about.finish()),
            col(|r| &r.2.text),
            vec,
        ],
    )?)
}

/// Index rows as (data file, passage).
/// The columns [`decode`] reads.
const COLUMNS: &[&str] = &[
    "data_file",
    "episode_id",
    "source",
    "source_kind",
    "text",
    "chunk",
    "occurred_at",
    "about",
];

/// Passage rows read from the index (see `search::rows`), with their data file: a chunk of text each (about
/// 1 KB), so at most some tens of MB. `MEMSTORE_INDEX_PASSAGE_ROWS` overrides.
const ROWS_MAX: usize = 50_000;

static ROWS: std::sync::LazyLock<search::rows::RowCache<(String, Passage)>> =
    std::sync::LazyLock::new(|| {
        search::rows::RowCache::new(search::index_knob("PASSAGE_ROWS", ROWS_MAX))
    });

fn decode(b: &RecordBatch) -> Vec<(String, Passage)> {
    let s = |n: &str| crate::strings(b, n);
    let (file, id, source, kind, text) = (
        s("data_file"),
        s("episode_id"),
        s("source"),
        s("source_kind"),
        s("text"),
    );
    let chunk = b
        .column_by_name("chunk")
        .unwrap()
        .as_primitive::<arrow_array::types::Int32Type>();
    let at = b
        .column_by_name("occurred_at")
        .unwrap()
        .as_primitive::<arrow_array::types::Int64Type>();
    let about = b.column_by_name("about").unwrap().as_list::<i32>();
    (0..b.num_rows())
        .map(|i| {
            let ids = if about.is_null(i) {
                vec![]
            } else {
                let v = about.value(i);
                let v = v.as_string::<i32>();
                (0..v.len()).map(|j| v.value(j).to_string()).collect()
            };
            (
                file[i].clone(),
                Passage {
                    episode_id: id[i].clone(),
                    chunk: chunk.value(i),
                    source: source[i].clone(),
                    source_kind: kind[i].clone(),
                    occurred_at: at.value(i),
                    about: ids,
                    text: text[i].clone(),
                },
            )
        })
        .collect()
}

/// Bring a scope's passage index up to date: drop rows of episodes files no longer current,
/// chunk the episodes of files it doesn't hold and embed the chunks it has no vector for (a
/// rewritten file keeps its chunks' vectors), a part of the episodes at a time as the facts
/// index is written (see `search::Progress`), rebuild the indexes, then mark those files (the
/// commit point of an update). Returns (passages written, files, chunks embedded).
pub async fn update(store: &Store, scope: &Scope) -> Result<(usize, usize, usize)> {
    store.check_fence()?;
    crate::workspace::check_index(store, scope).await?;
    // The heads before the files are read: every commit reachable from them has its rows in those files.
    let heads = store.branch_heads(scope).await?;
    let (p_uri, f_uri) = (uri(store, scope, "passages"), uri(store, scope, "files"));
    let held = match open(&f_uri).await? {
        Some(f) => search::covered(&f).await?,
        None => HashSet::new(),
    };
    let files = store.episode_files(scope).await?;
    let current: HashSet<&String> = files.keys().collect();
    let fresh: Vec<(&String, &Arc<Vec<RecordBatch>>)> =
        files.iter().filter(|(k, _)| !held.contains(*k)).collect();
    let parts = [("passages", p_uri.clone())];
    let mut progress = search::Progress::load(&dir_uri(store, scope)).await?;
    progress.roll_back(&parts).await?;
    let resumed: usize = fresh.iter().map(|(k, _)| progress.written(k)).sum();
    if resumed > 0 {
        eprintln!(
            "memstore: passage index of {}: resuming an interrupted update, {resumed} episodes already written",
            scope.namespace()
        );
    }
    let mut clear: Vec<String> = held
        .iter()
        .chain(&progress.files())
        .filter(|k| !current.contains(k))
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let stale = clear.len();
    // Rows of files about to be added are cleared too, unless a recorded chunk wrote them, so a
    // crashed update is redone cleanly.
    clear.extend(
        fresh
            .iter()
            .filter(|(k, _)| progress.written(k) == 0)
            .map(|(k, _)| (*k).clone()),
    );
    // Where the vectors of the rows about to be cleared are (`search::Reuse`): a rewritten file (compaction, a
    // new delete) mostly carries the same chunks. Only needed when there are files to write.
    let mut before = vec![];
    if !fresh.is_empty() {
        before.extend(open(&p_uri).await?);
    }
    let reuse = search::Reuse::of(before, &clear).await?;
    for u in [&p_uri, &f_uri] {
        if let Some(d) = open(u).await? {
            for keys in clear.chunks(500) {
                search::retrying(&format!("clear {u}"), || {
                    let d = d.clone();
                    let filter = search::in_list(keys);
                    async move { Ok(search::at_newest(d).await?.delete(&filter).await?) }
                })
                .await?;
            }
        }
    }
    // Rows written twice (two updates that once ran at once both wrote a file) are looked for once a process
    // and each night (`tidy_night`): the table counts no passages to tell a surplus by, as the facts index's
    // update does.
    static CHECKED: std::sync::LazyLock<Mutex<HashSet<String>>> =
        std::sync::LazyLock::new(Default::default);
    if CHECKED.lock().unwrap().insert(p_uri.clone())
        && let Some(mut d) = open(&p_uri).await?
    {
        store.check_fence()?;
        if search::drop_duplicates(&mut d, search::PASSAGE_KEY).await? > 0 {
            DROPPED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            OPEN.lock().unwrap().remove(&p_uri);
        }
    }
    if fresh.is_empty() && stale == 0 {
        record_coverage(store, scope, heads).await?;
        for (part, name) in [(&p_uri, "passages"), (&f_uri, "files")] {
            if let Some(ds) = open(part).await? {
                search::drop_old_versions_idle(&ds, &dir_uri(store, scope), name).await;
            }
        }
        return Ok((0, 0, 0));
    }
    let total: usize = fresh
        .iter()
        .flat_map(|(_, b)| b.iter())
        .map(|b| b.num_rows())
        .sum();
    let (mut n_out, mut n_missing, mut n_episodes) = (0, 0, 0);
    // Each file's passages once per episode, and where each file's rows resume (a chunk's rows are each file's
    // next ones), so a resumed update writes from the same rows.
    let keep: HashMap<&String, HashSet<usize>> =
        fresh.iter().map(|(k, b)| (*k, passage_rows(b))).collect();
    let mut at: HashMap<&String, usize> = fresh
        .iter()
        .map(|(k, _)| (*k, progress.written(k)))
        .collect();
    // An episode makes a few passages: a chunk is a quarter as many episodes as facts.
    for chunk in search::chunks(
        fresh.iter().map(|(k, b)| (*k, b.as_slice())),
        |k| progress.written(k),
        (search::chunk_rows() / 4).max(1),
    ) {
        let mut rows: Vec<(String, EpisodeRow)> = vec![];
        for (k, b) in &chunk {
            let start = at
                .get_mut(k)
                .map_or(0, |n| std::mem::replace(n, *n + b.num_rows()));
            let kept = &keep[k];
            rows.extend(
                rows_at(b, false)
                    .into_iter()
                    .filter(|(i, _)| kept.contains(&(start + i)))
                    .map(|(_, r)| (k.clone(), r)),
            );
        }
        let ids: Vec<String> = rows
            .iter()
            .map(|(_, r)| r.id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let bodies = store.episode_bodies(scope, ids).await?;
        let mut out: Vec<(String, String, Passage)> = vec![];
        for (file, r) in &rows {
            let Some(body) = bodies.get(&r.id) else {
                continue;
            };
            for (i, text) in chunks_with(&String::from_utf8_lossy(body), store.chunking())
                .into_iter()
                .enumerate()
            {
                out.push((file.clone(), r.commit.clone(), passage(r, i as i32, text)));
            }
        }
        let known = reuse.vectors(out.iter().map(|r| &r.2.text)).await;
        let missing: Vec<String> = out
            .iter()
            .map(|r| &r.2.text)
            .filter(|t| !known.contains_key(*t))
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let mut embedded: HashMap<String, Vec<f32>> = HashMap::new();
        for part in missing.chunks(4096) {
            embedded.extend(
                part.iter()
                    .cloned()
                    .zip(search::embed_cache::embed_cached(part.to_vec()).await?),
            );
        }
        let mut batches = vec![];
        for part in out.chunks(4096) {
            let vecs = part
                .iter()
                .map(|r| {
                    let t = &r.2.text;
                    known.get(t).or_else(|| embedded.get(t)).unwrap().clone()
                })
                .collect();
            batches.push(to_batch(part, vecs)?);
        }
        append(&p_uri, batches, schema()).await?;
        let through = search::through(&chunk, |k| progress.written(k));
        progress.record(&through, &parts).await?;
        n_out += out.len();
        n_missing += missing.len();
        n_episodes += chunk.iter().map(|(_, b)| b.num_rows()).sum::<usize>();
        eprintln!(
            "memstore: passage index of {}: wrote {} of {total} episodes ({n_out} passages)",
            scope.namespace(),
            resumed + n_episodes
        );
    }
    if let Some(mut ds) = open(&p_uri).await? {
        search::build_index(
            &mut ds,
            &["text"],
            IndexType::Inverted,
            "passages_text",
            &search::fts_build(InvertedIndexParams::default()),
        )
        .await?;
        let rows = ds.count_rows(None).await?;
        if rows >= search::VEC_INDEX_ROWS {
            search::build_index(
                &mut ds,
                &["vec"],
                IndexType::Vector,
                "vec",
                &search::vec_index(rows),
            )
            .await?;
        }
        for (col, kind, index_type) in [
            ("episode_id", BuiltinIndexType::BTree, IndexType::BTree),
            ("occurred_at", BuiltinIndexType::BTree, IndexType::BTree),
            ("source_kind", BuiltinIndexType::Bitmap, IndexType::Bitmap),
            ("about", BuiltinIndexType::LabelList, IndexType::LabelList),
        ] {
            search::build_index(
                &mut ds,
                &[col],
                index_type,
                col,
                &ScalarIndexParams::for_builtin(kind),
            )
            .await?;
        }
        search::tidy(&mut ds, search::Tidy::Update).await?;
    }
    let marks = RecordBatch::try_new(
        search::files_schema(),
        vec![Arc::new(StringArray::from_iter_values(
            fresh.iter().map(|(k, _)| k.as_str()),
        ))],
    )?;
    append(&f_uri, vec![marks], search::files_schema()).await?;
    progress.clear().await?;
    {
        let mut open = OPEN.lock().unwrap();
        DROPPED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        open.remove(&p_uri);
    }
    record_coverage(store, scope, heads).await?;
    for (part, name) in [(&p_uri, "passages"), (&f_uri, "files")] {
        if let Some(ds) = open(part).await? {
            search::drop_old_versions(&ds, &dir_uri(store, scope), name).await;
        }
    }
    Ok((n_out, fresh.len(), n_missing))
}

/// The nightly window's tidy of a scope's passage index, as [`search::tidy_night`] tidies its facts index.
/// Returns (indexes merged, parts compacted, rows dropped).
pub async fn tidy_night(store: &Store, scope: &Scope) -> Result<(usize, usize, usize)> {
    store.check_fence()?;
    if !search::Progress::load(&dir_uri(store, scope))
        .await?
        .files()
        .is_empty()
    {
        return Ok((0, 0, 0));
    }
    let p_uri = uri(store, scope, "passages");
    let Some(mut ds) = open(&p_uri).await? else {
        return Ok((0, 0, 0));
    };
    let dropped = search::drop_duplicates(&mut ds, search::PASSAGE_KEY).await?;
    let (merged, compacted) = search::tidy(&mut ds, search::Tidy::Night).await?;
    if merged > 0 || compacted || dropped > 0 {
        DROPPED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        OPEN.lock().unwrap().remove(&p_uri);
    }
    Ok((merged, usize::from(compacted), dropped))
}

/// Record in the index's manifest that it now covers `heads` (read before its files were), with each
/// part's version and the configuration; nothing when the newest record already says so.
async fn record_coverage(
    store: &Store,
    scope: &Scope,
    heads: std::collections::BTreeMap<String, Vec<String>>,
) -> Result<()> {
    let mut versions = std::collections::BTreeMap::new();
    for part in ["passages", "files"] {
        if let Some(d) = open(&uri(store, scope, part)).await? {
            versions.insert(part.to_string(), d.version().version);
        }
    }
    search::record_coverage(&dir_uri(store, scope), heads, versions, config(store)).await
}

/// The passages dataset and the files it holds, opened once per process and re-checked after
/// the store's staleness window.
struct OpenIndex {
    checked: std::time::Instant,
    files_version: Option<u64>,
    passages: Option<Dataset>,
    held: Arc<HashSet<String>>,
}

static OPEN: std::sync::LazyLock<Mutex<HashMap<String, Arc<OpenIndex>>>> =
    std::sync::LazyLock::new(Default::default);

/// Past the staleness window a read still gets the open index at once and a check for a newer one runs in
/// the background, as for the facts index (see `search::open_cached`).
async fn open_cached(store: &Store, scope: &Scope) -> Result<Arc<OpenIndex>> {
    let p_uri = uri(store, scope, "passages");
    let cached = OPEN.lock().unwrap().get(&p_uri).cloned();
    if let Some(o) = &cached {
        let age = o.checked.elapsed();
        if age < store.max_staleness() {
            return Ok(o.clone());
        }
        if age < store.hard_staleness() {
            if RECHECKING.lock().unwrap().insert(p_uri.clone()) {
                let (store, scope, o) = (store.clone(), scope.clone(), o.clone());
                tokio::spawn(async move {
                    if let Err(e) = recheck(&store, &scope, Some(o)).await {
                        eprintln!(
                            "memstore: checking the passage index of {scope} failed (searching the one open): {e:#}"
                        );
                    }
                    RECHECKING
                        .lock()
                        .unwrap()
                        .remove(&uri(&store, &scope, "passages"));
                });
            }
            return Ok(o.clone());
        }
    }
    recheck(store, scope, cached).await
}

/// Passage indexes whose re-check is under way.
static RECHECKING: std::sync::LazyLock<Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

/// Bumped whenever open passage indexes are dropped, so a re-check begun before doesn't put back what it
/// read then.
static DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One re-check of a passage index at a time (see [`recheck`]).
static TURNS: std::sync::LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    std::sync::LazyLock::new(Default::default);

/// The passage index as it is now: `cached`'s handles when `files` hasn't changed, else opened again. One at a
/// time per index, as for the facts index (`search::recheck`): one that waited for its turn takes what the one
/// before it opened, if that one finished after it asked, rather than open and load the same version again (its
/// keyword postings take a second or two, and every search of the scope after an idle spell asks at once).
async fn recheck(
    store: &Store,
    scope: &Scope,
    cached: Option<Arc<OpenIndex>>,
) -> Result<Arc<OpenIndex>> {
    let (p_uri, f_uri) = (uri(store, scope, "passages"), uri(store, scope, "files"));
    let asked = std::time::Instant::now();
    let turn = TURNS
        .lock()
        .unwrap()
        .entry(p_uri.clone())
        .or_default()
        .clone();
    let _turn = turn.lock().await;
    let current = OPEN.lock().unwrap().get(&p_uri).cloned();
    if let Some(o) = &current
        && o.checked >= asked
    {
        return Ok(o.clone());
    }
    let cached = current.or(cached);
    let dropped = DROPPED.load(std::sync::atomic::Ordering::SeqCst);
    let files = open(&f_uri).await?;
    let version = files.as_ref().map(|f| f.version().version);
    let fresh = match cached {
        Some(o) if o.files_version == version => OpenIndex {
            checked: std::time::Instant::now(),
            files_version: version,
            passages: o.passages.clone(),
            held: o.held.clone(),
        },
        _ => OpenIndex {
            checked: std::time::Instant::now(),
            files_version: version,
            // `files` is written last, so without it the first build (of a new scope, or after a forget or a purge
            // dropped the index) is still under way: `passages` may exist with no full-text index yet, and a keyword
            // query on it fails. Nothing is trusted from it until `files` lands; episodes are read directly.
            passages: match &files {
                Some(_) => {
                    let ds = open(&p_uri).await?;
                    // Its keyword postings first, as for the facts index (`search::warm_keywords`): left to the
                    // searches, a cold keyword list read them a term at a time from object storage, 0.6 s at the
                    // median and up to 5.7 s for a trimmed prompt on a production deployment's passages, against 25 ms loaded.
                    if let Some(ds) = &ds {
                        let t0 = std::time::Instant::now();
                        search::warm_keyword_indexes(ds, &["passages_text"]).await;
                        if t0.elapsed() > std::time::Duration::from_secs(1) {
                            eprintln!(
                                "memstore: passage keyword index of {scope} loaded in {} ms before reads use it",
                                t0.elapsed().as_millis()
                            );
                        }
                    }
                    ds
                }
                None => None,
            },
            held: Arc::new(match &files {
                Some(f) => search::covered(f).await?,
                None => HashSet::new(),
            }),
        },
    };
    let fresh = Arc::new(fresh);
    let mut open = OPEN.lock().unwrap();
    if DROPPED.load(std::sync::atomic::Ordering::SeqCst) == dropped {
        open.insert(p_uri, fresh.clone());
    }
    Ok(fresh)
}

/// Open the scope's passage index and load its index files: one keyword and one vector query for `text`
/// (embedded as `q`), for row ids only (see `search::warm`).
pub async fn warm(
    store: &Store,
    scope: &Scope,
    text: &str,
    q: &arrow_array::Float32Array,
) -> Result<()> {
    let index = open_cached(store, scope).await?;
    let Some(ds) = &index.passages else {
        return Ok(());
    };
    let mut scan = ds.scan();
    scan.full_text_search(FullTextSearchQuery::new(text.to_string()).limit(Some(1)))?;
    search::rows::ids(scan).await?;
    if search::rows::nearest_flat(ds, "vec", q, 1, None)
        .await?
        .is_none()
    {
        let mut scan = ds.scan();
        search::probe(scan.nearest("vec", q, 1)?);
        search::rows::ids(scan).await?;
    }
    Ok(())
}

/// The dataset version of each part of the scope's passage index that reads use now; empty without one.
pub async fn current_versions(
    store: &Store,
    scope: &Scope,
) -> Result<std::collections::BTreeMap<String, u64>> {
    let o = open_cached(store, scope).await?;
    let mut out = std::collections::BTreeMap::new();
    if let Some(v) = o.files_version {
        out.insert("files".to_string(), v);
    }
    if let Some(d) = &o.passages {
        out.insert("passages".to_string(), d.version().version);
    }
    Ok(out)
}

/// The passage index a read uses: the current one, or for a pinned read the version its snapshot recorded
/// (or none, when it recorded none).
async fn index_for(
    store: &Store,
    scope: &Scope,
    pin: Option<&search::IndexPin>,
) -> Result<Arc<OpenIndex>> {
    let Some(pin) = pin else {
        return open_cached(store, scope).await;
    };
    let Some(c) = &pin.passages else {
        return Ok(Arc::new(OpenIndex {
            checked: std::time::Instant::now(),
            files_version: None,
            passages: None,
            held: Arc::default(),
        }));
    };
    search::check_config(c, &config(store))?;
    let dir = dir_uri(store, scope);
    let key = format!("{dir}/@{}", serde_json::to_string(&c.versions)?);
    if let Some(o) = OPEN.lock().unwrap().get(&key).cloned() {
        return Ok(o);
    }
    let files = search::checkout(&dir, c, "files").await?;
    let o = Arc::new(OpenIndex {
        checked: std::time::Instant::now(),
        files_version: files.as_ref().map(|f| f.version().version),
        passages: search::checkout(&dir, c, "passages").await?,
        held: Arc::new(match &files {
            Some(f) => search::covered(f).await?,
            None => HashSet::new(),
        }),
    });
    OPEN.lock().unwrap().insert(key, o.clone());
    Ok(o)
}

/// Chunks of the episodes in files the index doesn't hold, read from the table and the bodies.
async fn tail(
    store: &Store,
    scope: &Scope,
    files: &HashMap<String, Arc<Vec<RecordBatch>>>,
    held: &HashSet<String>,
) -> Result<Vec<Passage>> {
    let rows: Vec<EpisodeRow> = files
        .iter()
        .filter(|(k, _)| !held.contains(*k))
        .flat_map(|(_, bs)| bs.iter().flat_map(episode_rows).collect::<Vec<_>>())
        .collect();
    // Cached by chunking and episode: stores that chunk differently share the process.
    let (bytes, overlap) = store.chunking();
    let key = |id: &str| format!("{bytes}/{overlap}/{id}");
    let missing: Vec<String> = {
        let cache = TAIL.lock().unwrap();
        rows.iter()
            .filter(|r| !cache.contains_key(&key(&r.id)))
            .map(|r| r.id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    };
    if !missing.is_empty() {
        let bodies = store.episode_bodies(scope, missing).await?;
        let mut cache = TAIL.lock().unwrap();
        if cache.len() + bodies.len() > TAIL_MAX {
            cache.clear();
        }
        for (id, b) in bodies {
            cache.insert(
                key(&id),
                Arc::new(chunks_with(&String::from_utf8_lossy(&b), store.chunking())),
            );
        }
    }
    let cache = TAIL.lock().unwrap();
    Ok(rows
        .iter()
        .flat_map(|r| {
            cache
                .get(&key(&r.id))
                .map(|cs| {
                    cs.iter()
                        .enumerate()
                        .map(|(i, t)| passage(r, i as i32, t.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect())
}

/// The keyword terms a search for `text` matches: its informative words and `extra` ones (other
/// spellings of names it uses).
pub fn query_terms(text: &str, extra: &[String]) -> HashSet<String> {
    std::iter::once(text)
        .chain(extra.iter().map(String::as_str))
        .flat_map(terms)
        .filter(|t| informative(t))
        .collect()
}

/// Passages the principal may see that match `filter`: ranked by keyword and meaning when there
/// is `text` (`extra` adds keyword terms), ordered by `rank` (recency, diversity over source
/// kinds), newest first otherwise. At most `k`, one hit per episode: its best chunk, or its first
/// without text.
#[allow(clippy::too_many_arguments)]
pub async fn search(
    store: &Store,
    scope: &Scope,
    vis: &Vis,
    pin: Option<&search::IndexPin>,
    who: &Principal,
    text: Option<&str>,
    extra: &[String],
    filter: &PassageFilter,
    k: usize,
    rank: search::Rank,
) -> Result<Vec<PassageHit>> {
    // Each phase's end, for the slow-search log below.
    let t0 = std::time::Instant::now();
    let mut took: Vec<(&str, f64)> = vec![];
    macro_rules! mark {
        ($what:expr) => {
            took.push(($what, t0.elapsed().as_secs_f64() * 1000.0))
        };
    }
    let index = index_for(store, scope, pin).await?;
    mark!("index");
    let files = store.episode_files(scope).await?;
    // Index rows are used only for current files the index marks: an update in progress (or interrupted)
    // writes rows before it marks their files, and those files are read directly, in the tail.
    let held = |file: &String| files.contains_key(file) && index.held.contains(file);
    // Of a document kept in versions, only the one current by the filter's end shows.
    let visible = |p: &Passage| {
        vis.can_see_episode(&p.episode_id, who)
            && filter.matches(p)
            && vis.is_current(&p.episode_id, who, filter.occurred_to)
    };
    let tail = tail(store, scope, &files, &index.held).await?;
    mark!("tail");
    let tail_len = tail.len();
    let mut sql = filter.to_sql();
    // A reader who may see few episodes, and few of the scope's: ask the index for those only, rather than
    // ranking everyone's passages, filtering after, and over-fetching until enough survive. A reader who sees
    // most of them is filtered after: thousands of ids in the scan's filter cost more than they prune.
    let episodes: usize = files
        .values()
        .map(|bs| bs.iter().map(RecordBatch::num_rows).sum::<usize>())
        .sum();
    let narrow = vis
        .narrow_episodes(who, MAX_PREFILTER_IDS)
        .filter(|ids| ids.len() * PREFILTER_SHARE <= episodes);
    let narrow_len = narrow.as_ref().map(Vec::len);
    if let Some(ids) = narrow.as_ref().filter(|ids| !ids.is_empty()) {
        let list: Vec<String> = ids.iter().map(|i| lit(i)).collect();
        let only = format!("`episode_id` IN ({})", list.join(", "));
        sql = Some(match sql {
            Some(f) => format!("({f}) AND ({only})"),
            None => only,
        });
    }
    // They may see nothing indexed at all: the index has nothing to give them.
    let nothing = narrow.as_ref().is_some_and(Vec::is_empty);
    let text = text.map(str::trim).filter(|t| !t.is_empty());

    let Some(text) = text else {
        let mut out: HashMap<(String, i32), Passage> = HashMap::new();
        if let Some(ds) = index.passages.as_ref().filter(|_| !nothing) {
            let mut scan = ds.scan();
            if let Some(f) = &sql {
                scan.filter(f)?;
            }
            scan.limit(Some(MAX_SCAN_ROWS as i64), None)?;
            let batches: Vec<RecordBatch> = scan.try_into_stream().await?.try_collect().await?;
            for b in &batches {
                for (file, p) in decode(b) {
                    if held(&file) && visible(&p) {
                        out.insert((p.episode_id.clone(), p.chunk), p);
                    }
                }
            }
        }
        for p in tail.into_iter().filter(|p| visible(p)) {
            out.insert((p.episode_id.clone(), p.chunk), p);
        }
        let mut out: Vec<Passage> = out.into_values().collect();
        out.sort_by(|a, b| {
            b.occurred_at
                .cmp(&a.occurred_at)
                .then(a.episode_id.cmp(&b.episode_id))
                .then(a.chunk.cmp(&b.chunk))
        });
        // each episode once, from its start
        out.dedup_by(|a, b| a.episode_id == b.episode_id);
        out.truncate(k);
        return Ok(out
            .into_iter()
            .map(|passage| PassageHit {
                passage,
                score: 0.0,
            })
            .collect());
    };

    let q_terms = query_terms(text, extra);
    let mut fts: Vec<String> = q_terms.iter().cloned().collect();
    fts.sort_unstable();
    // Of a longer query, the keyword list searches the `MEMSTORE_RANK_PASSAGE_MAX_TERMS` (default 256) terms
    // the fewest passages hold ([`search::fewest`]), as a fact search does its 64: on a busy production pod, where
    // agents searched with prompts of 900 words and more, passage lists took 1.4 s at the median and up to
    // 11 s, and a 400-word query's took half as long at 256 terms. Fewer cost relevance: at 64, whole prompts
    // of about 250 words found a relevant episode for 13 of 369 judged queries instead of 18. No budget of the
    // rows reached, as a fact search has: a passage list costs little a row (a trimmed prompt's words, in 4
    // times a production deployment's passages, 20 ms), and one changed which passages it found more than it saved.
    let max = (search::knob("PASSAGE_MAX_TERMS", 256.0) as usize).max(1);
    let keep = search::fewest(index.passages.as_ref(), &["text"], &fts, max, f64::INFINITY).await;
    let searched = keep.as_ref().map_or(fts.len(), HashSet::len);
    fts.retain(|t| keep.as_ref().is_none_or(|k| k.contains(t)));
    let fts = fts.join(" ");
    let q_vec = Float32Array::from(search::embed_text(text).await?);
    mark!("embed");
    // (bm25, distance) per (episode, chunk), and the passage.
    let mut hits: HashMap<(String, i32), Ranked> = HashMap::new();
    let mut fetch = k * search::FETCH_MULTIPLE;
    // Per round, for the slow search's line: how many it asked for, each query's own time and row count,
    // the rows' fetch, and how many of those rows the caller may see.
    let mut rounds: Vec<String> = vec![];
    if let Some(ds) = index.passages.as_ref().filter(|_| !nothing) {
        loop {
            let round_t = std::time::Instant::now();
            let mut saturated = false;
            // The keyword and the vector query run at once; their rows are taken in this order after.
            let mut queries = vec![];
            for by_vector in [false, true] {
                if !by_vector && fts.is_empty() {
                    continue;
                }
                let mut scan = ds.scan();
                if let Some(f) = &sql {
                    scan.filter(f)?.prefilter(true);
                }
                if by_vector {
                    search::probe(scan.nearest("vec", &q_vec, fetch)?);
                } else {
                    scan.full_text_search(
                        FullTextSearchQuery::new(fts.clone()).limit(Some(fetch as i64)),
                    )?;
                }
                // Row ids only: the rows come from the row cache (see `search::rows`).
                let (q_vec, sql) = (&q_vec, &sql);
                queries.push(async move {
                    let q_t = std::time::Instant::now();
                    // Without a vector index, the embeddings held in memory are searched.
                    let flat = match by_vector {
                        true => {
                            search::rows::nearest_flat(ds, "vec", q_vec, fetch, sql.as_deref())
                                .await?
                        }
                        false => None,
                    };
                    let ids = match flat {
                        Some(ids) => ids,
                        None => search::rows::ids(scan).await?,
                    };
                    anyhow::Ok((by_vector, ids, q_t.elapsed().as_millis()))
                });
            }
            let lists = futures::future::try_join_all(queries).await?;
            let queried = round_t.elapsed().as_millis();
            let mut round = format!("fetch {fetch}:");
            for (by_vector, l, ms) in &lists {
                let what = if *by_vector { "vector" } else { "keyword" };
                round.push_str(&format!(" {what} {ms} ms {} rows,", l.len()));
            }
            let lists: Vec<_> = lists.into_iter().map(|(v, l, _)| (v, l)).collect();
            let ids: Vec<u64> = lists
                .iter()
                .flat_map(|(_, l)| l.iter().map(|(id, _)| *id))
                .collect();
            let rows: HashMap<u64, Arc<(String, Passage)>> = ids
                .iter()
                .copied()
                .zip(ROWS.get(ds, &ids, COLUMNS, |b| Ok(decode(b))).await?)
                .collect();
            let fetched = round_t.elapsed().as_millis() - queried;
            let mut seen = 0;
            for (by_vector, list) in lists {
                saturated |= list.len() >= fetch;
                for (id, x) in list {
                    let (file, p) = &*rows[&id];
                    if !held(file) || !visible(p) {
                        continue;
                    }
                    seen += 1;
                    let x = x.unwrap_or(0.0);
                    let e = hits.entry((p.episode_id.clone(), p.chunk)).or_insert((
                        p.clone(),
                        None,
                        None,
                    ));
                    if by_vector {
                        e.2 = Some(e.2.map_or(x, |d: f32| d.min(x)));
                    } else {
                        e.1 = Some(e.1.map_or(x, |s: f32| s.max(x)));
                    }
                }
            }
            rounds.push(format!(
                "{round} rows fetched in {fetched} ms, {seen} of {} visible",
                ids.len()
            ));
            mark!("scan");
            let episodes = hits.keys().map(|(e, _)| e).collect::<HashSet<_>>().len();
            if episodes >= k || !saturated || fetch >= search::MAX_FETCH {
                break;
            }
            fetch = (fetch * 4).min(search::MAX_FETCH);
        }
    }

    // The unindexed tail: matched by its share of the query's terms, and by meaning when small.
    let tail: Vec<Passage> = tail.into_iter().filter(|p| visible(p)).collect();
    let tail_vecs = if !tail.is_empty() && tail.len() <= search::TAIL_EMBED_MAX {
        let texts: Vec<String> = tail.iter().map(|p| p.text.clone()).collect();
        Some(search::embed_tail(&texts).await?)
    } else {
        None
    };
    // Matched as the index would match it: stemmed, stop words out.
    let q_stems = search::stems(&q_terms.iter().cloned().collect::<Vec<_>>().join(" "));
    let matched = |t: &str| tail_stems(t).intersection(&q_stems).count();
    let top = hits.values().filter_map(|h| h.1).fold(1.0_f32, f32::max);
    let n_terms = q_stems.len().max(1) as f32;
    for (i, p) in tail.into_iter().enumerate() {
        let m = matched(&p.text);
        let d = tail_vecs.as_ref().map(|v| {
            v[i].iter()
                .zip(q_vec.values().iter())
                .map(|(a, b)| (a - b) * (a - b))
                .sum::<f32>()
        });
        if m == 0 && d.is_none() {
            continue;
        }
        let bm25 = (m > 0).then(|| top * m as f32 / n_terms);
        hits.entry((p.episode_id.clone(), p.chunk))
            .or_insert((p, bm25, d));
    }

    mark!("tail ranked");
    let out = fuse(hits.into_values().collect(), k, rank);
    mark!("fused");
    if t0.elapsed() >= SLOW_SEARCH {
        let phases: Vec<String> = took.iter().map(|(w, ms)| format!("{w} {ms:.0}")).collect();
        eprintln!(
            "slow passage search in {scope}: {} ms (ms since start: {}; {} tail passages, prefilter {:?} ids; \
             {searched} of {} keyword terms searched; rounds: {})",
            t0.elapsed().as_millis(),
            phases.join(", "),
            tail_len,
            narrow_len,
            q_terms.len(),
            rounds.join(" | "),
        );
    }
    Ok(out)
}

/// Reciprocal-rank fusion of the keyword and vector rankings, as for facts, over episodes: each
/// ranks by its best chunk in each, so one long episode can't fill a ranking with its chunks.
/// Each episode shows its best keyword chunk, else its nearest, and of chunks that score the same
/// its first. Both rankings are of the scores scaled by the episode's recency (`rank.as_of`).
/// Ties go to the stronger keyword match, then the nearer. With `rank.diverse`, the page is then
/// picked greedily with each source kind already on it counting [`search::DIVERSITY_DECAY`] less
/// per hit it has. At most `k`.
fn fuse(mut chunks: Vec<Ranked>, k: usize, rank: search::Rank) -> Vec<PassageHit> {
    // In chunk order, so the same search shows the same chunk every time (they come from a map).
    chunks.sort_by(|a, b| {
        a.0.episode_id
            .cmp(&b.0.episode_id)
            .then(a.0.chunk.cmp(&b.0.chunk))
    });
    // per episode: the chunk to show, its best keyword score and its nearest distance
    let mut eps: HashMap<String, (Passage, Option<f32>, Option<f32>)> = HashMap::new();
    for (p, bm25, d) in chunks {
        let e = eps
            .entry(p.episode_id.clone())
            .or_insert_with(|| (p.clone(), None, None));
        let better = match (bm25, e.1) {
            (Some(b), Some(best)) => b > best,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => d.unwrap_or(f32::INFINITY) < e.2.unwrap_or(f32::INFINITY),
        };
        if better {
            e.0 = p;
        }
        e.1 = match (e.1, bm25) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        e.2 = match (e.2, d) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
    // (passage, recency-scaled keyword score, recency-scaled similarity, fused score)
    let mut out: Vec<(Passage, Option<f32>, Option<f32>, f32)> = eps
        .into_values()
        .map(|(p, b, d)| {
            let bias = search::recency_bias(p.occurred_at, rank.as_of);
            let kw = b.map(|b| b * bias);
            let sem = d.map(|d| (1.0 - d / 2.0).max(0.0) * bias);
            (p, kw, sem, 0.0)
        })
        .collect();
    let by_id = |a: &Passage, b: &Passage| a.episode_id.cmp(&b.episode_id);
    out.sort_by(|a, b| {
        b.1.unwrap_or(0.0)
            .total_cmp(&a.1.unwrap_or(0.0))
            .then(by_id(&a.0, &b.0))
    });
    for (r, h) in out.iter_mut().filter(|h| h.1.is_some()).enumerate() {
        h.3 += 1.0 / (search::RRF_K + r as f32);
    }
    out.sort_by(|a, b| {
        b.2.unwrap_or(f32::NEG_INFINITY)
            .total_cmp(&a.2.unwrap_or(f32::NEG_INFINITY))
            .then(by_id(&a.0, &b.0))
    });
    for (r, h) in out.iter_mut().filter(|h| h.2.is_some()).enumerate() {
        h.3 += 1.0 / (search::RRF_K + r as f32);
    }
    out.sort_by(|a, b| {
        b.3.total_cmp(&a.3)
            .then(b.1.unwrap_or(0.0).total_cmp(&a.1.unwrap_or(0.0)))
            .then(
                b.2.unwrap_or(f32::NEG_INFINITY)
                    .total_cmp(&a.2.unwrap_or(f32::NEG_INFINITY)),
            )
            .then(by_id(&a.0, &b.0))
    });
    if rank.diverse {
        out = diversify(out, k);
    }
    out.truncate(k);
    out.into_iter()
        .map(|(passage, _, _, score)| PassageHit { passage, score })
        .collect()
}

/// The first `k` of `ranked` (best first) picked greedily, each pick the hit with the highest
/// fused score times [`search::DIVERSITY_DECAY`] to the number of hits of its source kind already
/// picked; equal picks keep their rank order. One kind can still fill a page when nothing else
/// comes close.
fn diversify(
    mut ranked: Vec<(Passage, Option<f32>, Option<f32>, f32)>,
    k: usize,
) -> Vec<(Passage, Option<f32>, Option<f32>, f32)> {
    let mut picked = Vec::with_capacity(k.min(ranked.len()));
    let mut seen: HashMap<String, i32> = HashMap::new();
    while picked.len() < k && !ranked.is_empty() {
        let mut best = (0, f32::NEG_INFINITY);
        for (i, h) in ranked.iter().enumerate() {
            let n = seen.get(&h.0.source_kind).copied().unwrap_or(0);
            let s = h.3 * search::DIVERSITY_DECAY.powi(n);
            if s > best.1 {
                best = (i, s);
            }
        }
        let h = ranked.remove(best.0);
        *seen.entry(h.0.source_kind.clone()).or_insert(0) += 1;
        picked.push(h);
    }
    picked
}

/// When the newest episode the reader may see occurred by `by`, over the whole scope (passages or
/// not), so an answer can say how far its memory reaches; an episode dated later (a meeting not
/// held yet) says nothing about that. None when they may see nothing.
pub async fn newest_visible(
    store: &Store,
    scope: &Scope,
    vis: &Vis,
    who: &Principal,
    by: i64,
) -> Result<Option<i64>> {
    /// Per namespace: the set of episode files it was computed from, and (occurred_at, id) of
    /// every episode, newest first.
    type Newest = HashMap<String, (String, Arc<Vec<(i64, String)>>)>;
    static NEWEST: std::sync::LazyLock<Mutex<Newest>> = std::sync::LazyLock::new(Default::default);
    let files = store.episode_files(scope).await?;
    let ns = scope.namespace();
    let mut key: Vec<&String> = files.keys().collect();
    key.sort_unstable();
    let key = key
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let cached = NEWEST
        .lock()
        .unwrap()
        .get(&ns)
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| v.clone());
    let by_time = match cached {
        Some(v) => v,
        None => {
            // every episode, newest first
            let mut rows: Vec<(i64, String)> = files
                .values()
                .flat_map(|bs| {
                    bs.iter().flat_map(|b| {
                        let at = crate::timestamps(b, "occurred_at");
                        crate::strings(b, "episode_id")
                            .into_iter()
                            .zip(at)
                            .map(|(id, t)| (t.unwrap_or_default(), id))
                    })
                })
                .collect();
            rows.sort_unstable_by(|a, b| b.cmp(a));
            let v = Arc::new(rows);
            NEWEST.lock().unwrap().insert(ns, (key, v.clone()));
            v
        }
    };
    Ok(by_time
        .iter()
        .find(|(t, id)| *t <= by && vis.can_see_episode(id, who))
        .map(|(t, _)| *t))
}

/// A passage's id: the start of its episode id, then `#` and the chunk.
pub fn short_id(episode_id: &str, chunk: i32) -> String {
    format!("{}#{chunk}", &episode_id[..episode_id.len().min(SHORT_ID)])
}

/// The stretch of `text` (whitespace folded) of about `max` bytes holding the most of `terms`,
/// marked with … where it was cut; the start of the text when none of them occurs.
pub fn snippet(text: &str, terms: &HashSet<String>, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.len() <= max {
        return flat;
    }
    // (offset, the query terms the word holds) for each word holding any
    let mut at = 0;
    let mut hits: Vec<(usize, HashSet<String>)> = vec![];
    for w in flat.split(' ') {
        let found: HashSet<String> = search::terms(w)
            .into_iter()
            .filter(|t| terms.contains(t))
            .collect();
        if !found.is_empty() {
            hits.push((at, found));
        }
        at += w.len() + 1;
    }
    // the window starting at a hit that holds the most distinct terms, then the most hits
    let mut anchor = 0;
    let mut best = (0, 0);
    for (j, (start, _)) in hits.iter().enumerate() {
        let mut seen: HashSet<&String> = HashSet::new();
        let mut n = 0;
        for (_, ts) in hits[j..]
            .iter()
            .take_while(|(o, _)| *o < start + max * 3 / 4)
        {
            seen.extend(ts);
            n += 1;
        }
        if (seen.len(), n) > best {
            best = (seen.len(), n);
            anchor = *start;
        }
    }
    // a little context before it, from a sentence start when there is one
    let mut start = 0;
    if anchor > max / 4 {
        let lead = floor_boundary(&flat, anchor - max / 4);
        start = match [". ", "? ", "! ", "; "]
            .iter()
            .filter_map(|sep| flat[lead..anchor].rfind(sep).map(|i| lead + i + sep.len()))
            .max()
        {
            Some(s) => s,
            None => flat[lead..]
                .find(' ')
                .map_or(lead, |i| lead + i + 1)
                .min(anchor),
        };
    }
    let mut end = floor_boundary(&flat, (start + max).min(flat.len()));
    if end < flat.len()
        && let Some(i) = flat[start..end].rfind(' ')
    {
        end = start + i;
    }
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        flat[start..end].trim(),
        if end < flat.len() { "…" } else { "" }
    )
}

/// An episode's first line, as its title (an email's subject, a document's name).
pub fn title(body: &str) -> String {
    let first = body.trim().lines().next().unwrap_or_default().trim();
    match first.char_indices().nth(TITLE_CHARS) {
        Some((i, _)) => format!("{}…", &first[..i]),
        None => first.to_string(),
    }
}

/// Titles of these episodes, read from their bodies once per process.
pub async fn titles(
    store: &Store,
    scope: &Scope,
    ids: &[String],
) -> Result<HashMap<String, String>> {
    let missing: Vec<String> = {
        let cache = TITLES.lock().unwrap();
        ids.iter()
            .filter(|id| !cache.contains_key(*id))
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    };
    if !missing.is_empty() {
        let bodies = store.episode_bodies(scope, missing).await?;
        let mut cache = TITLES.lock().unwrap();
        if cache.len() + bodies.len() > TAIL_MAX {
            cache.clear();
        }
        for (id, b) in bodies {
            cache.insert(id, title(&String::from_utf8_lossy(&b)));
        }
    }
    let cache = TITLES.lock().unwrap();
    Ok(ids
        .iter()
        .filter_map(|id| cache.get(id).map(|t| (id.clone(), t.clone())))
        .collect())
}

/// The episode an id names (a whole episode id or its start, at least 8 characters), if
/// exactly one committed episode the principal may see starts with it.
pub fn resolve(vis: &Vis, who: &Principal, id: &str) -> Result<Option<String>, String> {
    let id = id.trim();
    if id.len() < 8 {
        return Err(format!(
            "passage id {id:?} is too short: give at least 8 characters"
        ));
    }
    let mut found = vis
        .acls
        .iter()
        .filter(|(e, acl)| e.starts_with(id) && who.can_see(acl))
        .map(|(e, _)| e);
    match (found.next(), found.next()) {
        (Some(e), None) => Ok(Some(e.clone())),
        (None, _) => Ok(None),
        (Some(_), Some(_)) => Err(format!(
            "passage id {id:?} matches several episodes: give more of it"
        )),
    }
}

/// One episode's text as reading by id returns it.
pub struct Read {
    pub episode_id: String,
    pub source: String,
    pub occurred_at: i64,
    pub about: Vec<String>,
    pub text: String,
    /// How many chunks the episode has, and the first and last shown (inclusive).
    pub chunks: usize,
    pub shown: (usize, usize),
}

/// The text of these (visible, resolved) episodes: all of it when it fits in [`READ_BYTES`],
/// else the chunk asked for with its neighbours, or the first chunks.
pub async fn read(
    store: &Store,
    scope: &Scope,
    wanted: &[(String, Option<i32>)],
) -> Result<Vec<Read>> {
    let ids: HashSet<&String> = wanted.iter().map(|(e, _)| e).collect();
    let mut meta: HashMap<String, EpisodeRow> = HashMap::new();
    for bs in store.episode_files(scope).await?.values() {
        for r in bs.iter().flat_map(|b| rows_of(b, true)) {
            if ids.contains(&r.id) {
                meta.insert(r.id.clone(), r);
            }
        }
    }
    let bodies = store
        .episode_bodies(scope, ids.into_iter().cloned().collect())
        .await?;
    let mut out = vec![];
    for (id, chunk) in wanted {
        let (Some(m), Some(body)) = (meta.get(id), bodies.get(id)) else {
            continue;
        };
        let body = String::from_utf8_lossy(body);
        let body = body.trim();
        let sp = spans_with(body, store.chunking());
        let n = sp.len().max(1);
        let (text, shown) = if body.len() <= READ_BYTES || sp.is_empty() {
            (body.to_string(), (0, n - 1))
        } else {
            let k = chunk.map_or(0, |c| (c.max(0) as usize).min(n - 1));
            let (mut a, mut z) = (k.saturating_sub(1), (k + 1).min(n - 1));
            if chunk.is_none() {
                (a, z) = (0, 0);
                while z + 1 < n && sp[z + 1].1 - sp[0].0 <= READ_BYTES {
                    z += 1;
                }
            }
            (body[sp[a].0..sp[z].1].to_string(), (a, z))
        };
        out.push(Read {
            episode_id: id.clone(),
            source: m.source.clone(),
            occurred_at: m.occurred_at,
            about: m.about.clone(),
            text,
            chunks: n,
            shown,
        });
    }
    Ok(out)
}

/// Forget this process's open passage indexes under `prefix` (their files are being deleted).
/// Open passage indexes and passage rows held (for the memory log).
pub fn held() -> (usize, usize) {
    (OPEN.lock().unwrap().len(), ROWS.len())
}

pub fn forget_open(prefix: &str) {
    let mut open = OPEN.lock().unwrap();
    DROPPED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    open.retain(|k, _| !k.starts_with(prefix));
    // Their rows (forgotten source text among them) leave memory with them.
    ROWS.clear();
}
