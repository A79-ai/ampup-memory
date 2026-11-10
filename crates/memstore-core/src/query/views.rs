//! Current values kept in memory between queries: the entities of a type a reader may see a fact of, each
//! entity's current values of one attribute, and the names of entities that have other names, as that
//! reader sees them now. They are built by the plain reads (`visible`, `current`), so they answer exactly
//! as those do, and kept while nothing they were read from changes: the same visibility state (no commit,
//! retraction, closure or retention change since), the same files in the facts table, the same reader. A
//! grouped count or a filter on a type then reads them instead of scanning and decoding every fact of the
//! type again. A value keeps what filters and grouping read (about 150 bytes), not the whole fact.
//!
//! `AMPUP_MEMORY_VIEW_FACTS` (default 200000) caps the values and entities kept; the least recently used go
//! first. 0 keeps none.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use anyhow::Result;

use crate::Vis;
use crate::search::{Col, Lit};

/// What a view holds.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum What {
    /// The entities of a type the reader may see a fact of.
    Universe(String),
    /// For a type and an attribute, each entity's current values.
    Values(String, String),
    /// Each entity with another name (an alias): all its names.
    Names,
    /// A named metric's answer, kept past new writes ([`Key::lasting`]): the scope's namespace, and the
    /// call (JSON) that asked for it.
    LastingMetric(String, String),
}

/// An entity's current value of an attribute, as filters and grouping read it.
#[derive(Clone, Debug)]
pub(super) struct Cur {
    pub value: String,
    pub value_ref: Option<String>,
    pub value_num: Option<f64>,
    pub value_ts: Option<i64>,
    /// Typed with the view's type (not written before the scope had a schema).
    pub typed: bool,
}

impl Cur {
    /// Its value in column `c`, as `Col::of` gives a fact's; none for columns it doesn't keep.
    pub fn col(&self, c: Col) -> Option<Lit> {
        match c {
            Col::Value => Some(Lit::Str(self.value.clone())),
            Col::ValueNum => self.value_num.map(Lit::Num),
            Col::ValueTs => self.value_ts.map(Lit::Int),
            Col::ValueRef => self.value_ref.clone().map(Lit::Str),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub(super) enum View {
    Universe(Arc<BTreeSet<String>>),
    Values(Arc<HashMap<String, Vec<Cur>>>),
    Names(Arc<std::collections::BTreeMap<String, Vec<String>>>),
    Answer(Arc<serde_json::Value>),
}

impl View {
    fn size(&self) -> usize {
        match self {
            View::Universe(s) => s.len(),
            View::Values(m) => m.values().map(Vec::len).sum(),
            View::Names(m) => m.values().map(Vec::len).sum(),
            // A group or a cited fact weighs about what a kept value does.
            View::Answer(a) if a["rows"].is_array() => a["rows"].as_array().map_or(1, |r| {
                r.iter()
                    .map(|r| 1 + r["cites"].as_array().map_or(0, Vec::len))
                    .sum()
            }),
            View::Answer(a) => a["groups"].as_array().map_or(1, |g| {
                g.iter()
                    .map(|g| {
                        1 + g["members"].as_array().map_or(0, |m| {
                            m.iter()
                                .map(|m| m["facts"].as_array().map_or(1, Vec::len))
                                .sum()
                        })
                    })
                    .sum()
            }),
        }
    }
}

/// What a view was read from, and what it holds.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    /// The visibility state's address. The entry holds it weakly, so no other state takes the address
    /// while the entry lives.
    vis: usize,
    /// The facts table's current files (see `crate::search::files_stamp`).
    files: u64,
    /// The reader's ACL entries: all that decides what they see.
    tokens: Arc<Vec<String>>,
    what: What,
}

impl Key {
    pub(super) fn new(vis: &Arc<Vis>, files: u64, tokens: &Arc<Vec<String>>, what: What) -> Key {
        Key {
            vis: Arc::as_ptr(vis) as usize,
            files,
            tokens: tokens.clone(),
            what,
        }
    }

    /// The key of a metric's answer that new writes don't make stale: kept for the reader under the state's
    /// [`Vis::removals`] (no two states share one unless one follows the other with nothing lost, so a
    /// branch's or a pinned read's answers are their own), so anything memory loses (a retraction, a forget, a narrowed ACL, an episode
    /// aged out) leaves it behind, while facts written since are left out until someone asks for it anew
    /// ([`forget`]).
    pub(super) fn lasting(
        scope: String,
        removals: u64,
        facts_generation: u64,
        tokens: &Arc<Vec<String>>,
        call: String,
    ) -> Key {
        Key {
            // No state's address: the facts generation ([`facts_generation`]).
            vis: facts_generation as usize,
            files: removals,
            tokens: tokens.clone(),
            what: What::LastingMetric(scope, call),
        }
    }

    fn is_lasting(&self) -> bool {
        matches!(self.what, What::LastingMetric(..))
    }

    /// Whether `self` is a lasting answer `newer` leaves behind: the same scope, under another state's
    /// `removals` or another facts generation.
    fn superseded_by(&self, newer: &Key) -> bool {
        match (&self.what, &newer.what) {
            (What::LastingMetric(a, _), What::LastingMetric(b, _)) => {
                a == b && (self.files != newer.files || self.vis != newer.vis)
            }
            _ => false,
        }
    }
}

/// `scope`'s facts generation, for a read of its facts table's data `files` as of snapshot `seq`: the same
/// while the table only gains files, a new one once a file it held is gone (facts rewritten without some:
/// an erasure or a forget run by any process, compaction). Lasting answers are kept under it as well as
/// `Vis::removals`: an erasure deletes facts before the episodes that would show it to a `Vis`, and may
/// stop between the two. A read of an older snapshot than one already seen may still hold what was
/// erased since: it gets a generation of its own, which no other read shares, so what it builds is never
/// served to anyone else.
pub(super) fn facts_generation(scope: &str, seq: i64, files: Vec<String>) -> u64 {
    struct Seen {
        seq: i64,
        files: BTreeSet<String>,
        generation: u64,
    }
    static SEEN: LazyLock<Mutex<HashMap<String, Seen>>> = LazyLock::new(Default::default);
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let fresh = || NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let files: BTreeSet<String> = files.into_iter().collect();
    let mut seen = SEEN.lock().unwrap();
    match seen.get_mut(scope) {
        Some(s) if seq < s.seq => fresh(),
        Some(s) if seq == s.seq && files == s.files => s.generation,
        Some(s) => {
            let gone = s.files.difference(&files).count();
            if gone > 0 {
                eprintln!(
                    "memstore: {scope}'s facts table lost {gone} data files (rewritten): its kept metric answers go"
                );
                s.generation = fresh();
            }
            s.seq = seq;
            s.files = files;
            s.generation
        }
        None => {
            let generation = fresh();
            seen.insert(
                scope.to_string(),
                Seen {
                    seq,
                    files,
                    generation,
                },
            );
            generation
        }
    }
}

/// Drop `scope`'s lasting answers kept under any `removals` but this one: memory lost something they may
/// hold.
pub(super) fn drop_lasting(scope: &str, removals: u64) {
    let mut c = CACHE.lock().unwrap();
    let c = &mut *c;
    c.entries.retain(|k, e| {
        let stale =
            matches!(&k.what, What::LastingMetric(s, _) if s == scope) && k.files != removals;
        if stale {
            c.size -= e.size;
        }
        !stale
    });
}

/// Drop the view `key` names, so the next read builds it anew.
pub(super) fn forget(key: &Key) {
    let mut c = CACHE.lock().unwrap();
    if let Some(e) = c.entries.remove(key) {
        c.size -= e.size;
    }
}

struct Entry {
    vis: Weak<Vis>,
    view: Arc<tokio::sync::OnceCell<View>>,
    size: usize,
    used: u64,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<Key, Entry>,
    size: usize,
    clock: u64,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

fn budget() -> usize {
    static BUDGET: LazyLock<usize> = LazyLock::new(|| {
        std::env::var("AMPUP_MEMORY_VIEW_FACTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200_000)
    });
    *BUDGET
}

/// The view `key` names: kept, or built by `build` (once, however many readers ask for it at the same
/// time) and kept within the budget.
pub(super) async fn get_or_build<F, Fut>(key: Key, vis: &Arc<Vis>, build: F) -> Result<View>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<View>>,
{
    let cell = {
        let mut c = CACHE.lock().unwrap();
        c.clock += 1;
        let clock = c.clock;
        let e = c.entries.entry(key.clone()).or_insert_with(|| Entry {
            vis: Arc::downgrade(vis),
            view: Default::default(),
            size: 0,
            used: clock,
        });
        e.used = clock;
        e.view.clone()
    };
    let view = cell.get_or_try_init(build).await?.clone();
    let mut c = CACHE.lock().unwrap();
    let c = &mut *c;
    if let Some(e) = c.entries.get_mut(&key)
        && e.size == 0
    {
        e.size = view.size().max(1);
        c.size += e.size;
    }
    // Views of states no reader holds any more can't be asked for again. A lasting one outlives its state
    // until its scope's memory loses something: it may hold what was lost, so it goes then.
    c.entries.retain(|k, e| {
        let live = match k.is_lasting() {
            true => !k.superseded_by(&key),
            false => e.vis.strong_count() > 0,
        };
        if !live {
            c.size -= e.size;
        }
        live
    });
    while c.size > budget() {
        let Some(oldest) = c
            .entries
            .iter()
            .filter(|(_, e)| e.size > 0)
            .min_by_key(|(_, e)| e.used)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        if let Some(e) = c.entries.remove(&oldest) {
            c.size -= e.size;
        }
    }
    Ok(view)
}
