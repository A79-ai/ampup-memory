//! Cross-scope reads. A scope whose `reads` setting names other scopes of its org (`Settings::reads`, the operator's
//! to set) is read together with them on the routes its readers search through: `/search` (and its rows),
//! `/episode`, and MCP's `search_memory` and `get_episode` (an episode, a fact or a subject by id); and advice
//! (`POST /advice`, MCP `get_advice` and a session's advice steps) routes over their skills and situations too
//! ([`crate::advice::get_across`]). Each scope is read as it is for its own readers, with the caller's principal, so
//! every episode's and document's ACL applies in its own scope as it does there; a search ranks each with the
//! caller's ranking, and the hits are merged by score, each naming its scope, the same episode (or source) found in
//! two scopes shown once. Everything else (writes, extraction, the schema, sessions, `query_memory`, briefs,
//! documents) stays in the caller's own scope, which [`Reads`] derefs to.
//! A scope with no `reads`, and every read on a branch or a snapshot, reads its own scope alone, as before.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Deref;

use anyhow::Result;
use arrow_array::Array as _;
use arrow_array::cast::AsArray;

use super::{State, ToolError, cell, writer};
use crate::query::schema::Schema;
use crate::search::passages::{PassageFilter, PassageHit};
use crate::{Episode, Fact, MAIN, PagePin, Principal, Scope, Session, Store, freshness, search};

/// The sessions a read route reads: the caller's scope's, then those of the scopes its `reads` setting adds.
#[derive(Clone)]
pub(super) struct Reads {
    own: Session,
    /// The scopes `reads` adds, on main as it is now (or as a cursor pinned them), in the setting's order.
    also: Vec<Session>,
    /// What these reads returned, for the audit's `read` row ([`Reads::took`]). Shared by clones.
    seen: std::sync::Arc<std::sync::Mutex<Seen>>,
}

/// Items and source episodes a request's reads returned.
#[derive(Default)]
struct Seen {
    items: usize,
    episodes: Vec<String>,
}

impl Deref for Reads {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.own
    }
}

/// Items read across scopes, best first, and the scope each came from where it isn't the caller's own (by fact id
/// for facts, by episode id for passages): an index into [`Reads::scope_at`].
pub(super) struct Scoped<T> {
    pub items: Vec<T>,
    pub from: HashMap<String, usize>,
}

impl Reads {
    /// The caller's own scope alone.
    pub fn alone(own: Session) -> Self {
        Reads {
            own,
            also: vec![],
            seen: Default::default(),
        }
    }

    /// The caller's scope and those its `reads` setting names, each this cell's, pinned on main as it is now. A
    /// session on a branch or a snapshot reads its own scope alone. A scope that can't be opened now is left out
    /// (and said in the log): the caller's own memory is still answered.
    pub async fn of(state: &State, store: &Store, own: Session) -> Self {
        if !own.on_main_now() {
            return Reads::alone(own);
        }
        let scopes: Vec<Scope> = match store.directory().await {
            Ok(d) => d.dir.reads(own.scope()),
            Err(e) => {
                eprintln!(
                    "reads of {}: directory unavailable: {}",
                    own.scope(),
                    writer::redacted(&e)
                );
                vec![]
            }
        };
        let scopes: Vec<Scope> = scopes.into_iter().filter(cell::mine).collect();
        if scopes.is_empty() {
            return Reads::alone(own);
        }
        let pinned = futures::future::join_all(scopes.iter().map(|s| store.pin(s, MAIN))).await;
        let mut also = vec![];
        for (scope, p) in scopes.into_iter().zip(pinned) {
            match p {
                Ok(s) => {
                    // Kept warm with the scopes the server serves (`keep_warm`).
                    state.served.lock().unwrap().insert(scope);
                    also.push(s);
                }
                Err(e) => eprintln!(
                    "{} also reads {scope}, unavailable now: {}",
                    own.scope(),
                    writer::redacted(&e)
                ),
            }
        }
        Reads {
            own,
            also,
            seen: Default::default(),
        }
    }

    /// Note what a read returned: `items` of them, from these episodes.
    fn note<'a>(&self, items: usize, ids: impl Iterator<Item = &'a str>) {
        let mut s = self.seen.lock().unwrap();
        s.items += items;
        for id in ids {
            if s.episodes.len() >= crate::audit::MAX_READ_IDS {
                break;
            }
            if !s.episodes.iter().any(|e| e == id) {
                s.episodes.push(id.to_string());
            }
        }
    }

    /// What the reads so far returned: how many items, and the episodes they were from.
    pub fn took(&self) -> (usize, Vec<String>) {
        let s = self.seen.lock().unwrap();
        (s.items, s.episodes.clone())
    }

    /// The scopes read besides the caller's own, in the setting's order: what `get_advice` also routes over.
    pub fn also(&self) -> &[Session] {
        &self.also
    }

    /// Whether more than the caller's own scope is read.
    pub fn fans_out(&self) -> bool {
        !self.also.is_empty()
    }

    /// Every session read, the caller's own first.
    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        std::iter::once(&self.own).chain(&self.also)
    }

    pub fn session_at(&self, i: usize) -> &Session {
        match i {
            0 => &self.own,
            i => &self.also[i - 1],
        }
    }

    pub fn scope_at(&self, i: usize) -> &Scope {
        self.session_at(i).scope()
    }

    /// The scopes read, as their names: for a cursor's fingerprint, and a search's answer.
    pub fn scope_names(&self) -> Vec<String> {
        self.sessions().map(|s| s.scope().to_string()).collect()
    }

    /// The scope an item of `from` (by its key) came from.
    pub fn scope_of(&self, from: &HashMap<String, usize>, key: &str) -> &Scope {
        self.scope_at(from.get(key).copied().unwrap_or(0))
    }

    /// What a page read, for its cursor: each scope's pin, as of `at`.
    pub async fn pins(&self, store: &Store, at: i64) -> Result<Vec<PagePin>> {
        futures::future::try_join_all(self.sessions().map(|s| s.page_pin(store, at))).await
    }

    /// These reads as a cursor's first page read them: each scope pinned as it was then.
    pub async fn resume(&self, store: &Store, pins: &[PagePin]) -> Result<Reads> {
        if pins.len() != 1 + self.also.len() {
            return Err(ToolError("cursor belongs to a different search".into()).into());
        }
        let own = self.own.resume(store, &pins[0]).await?;
        let also = futures::future::try_join_all(
            self.also
                .iter()
                .zip(&pins[1..])
                .map(|(s, p)| s.resume(store, p)),
        )
        .await?;
        Ok(Reads {
            own,
            also,
            seen: self.seen.clone(),
        })
    }

    /// The top `k` facts for `query` across the scopes read: each scope searched with `rank` (the caller's) and
    /// `filter`, merged by score with each scope's own order kept, a fact citing an episode (by id or source) a
    /// fact kept from another scope cites left out. The caller's own scope alone is searched as it always was.
    #[allow(clippy::too_many_arguments)]
    pub async fn search(
        &self,
        store: &Store,
        who: &Principal,
        query: &str,
        k: usize,
        valid_at: Option<i64>,
        filter: Option<&search::Filter>,
        rank: search::Rank,
    ) -> Result<Scoped<search::Hit>> {
        let one = move |s: &Session| {
            let s = s.clone();
            async move {
                match filter {
                    Some(f) => {
                        s.search_filtered(store, who, query, k, valid_at, f, rank)
                            .await
                    }
                    None => s.search(store, who, query, k, valid_at, rank).await,
                }
            }
        };
        if !self.fans_out() {
            let items = one(&self.own).await?;
            self.note(
                items.len(),
                items
                    .iter()
                    .flat_map(|h| h.fact.source_episodes.iter().map(String::as_str)),
            );
            return Ok(Scoped {
                items,
                from: HashMap::new(),
            });
        }
        let (own, also) = tokio::join!(
            one(&self.own),
            futures::future::join_all(self.also.iter().map(one))
        );
        let lists = self.lists(own?, also, "search");
        let sources = self
            .sources(store, &lists, |h: &search::Hit| {
                h.fact.source_episodes.iter().map(String::as_str).collect()
            })
            .await?;
        let merged = dedup(
            merge(lists, |h: &search::Hit| h.score),
            |h| h.fact.source_episodes.iter().map(String::as_str).collect(),
            |_| None,
            &sources,
        );
        let out = scoped(merged, k, |h| h.fact.fact_id.clone());
        self.note(
            out.items.len(),
            out.items
                .iter()
                .flat_map(|h| h.fact.source_episodes.iter().map(String::as_str)),
        );
        Ok(out)
    }

    /// The top `k` passages across the scopes read, as [`Reads::search`] merges facts: by score, a passage of an
    /// episode (by id or source) another scope's kept passage is of left out.
    #[allow(clippy::too_many_arguments)]
    pub async fn passages(
        &self,
        store: &Store,
        who: &Principal,
        text: Option<&str>,
        filter: &PassageFilter,
        k: usize,
        rank: search::Rank,
    ) -> Result<Scoped<PassageHit>> {
        let one = move |s: &Session| {
            let s = s.clone();
            async move {
                search::passages::search(
                    store,
                    s.scope(),
                    s.vis(),
                    s.index_pin(),
                    who,
                    text,
                    &[],
                    filter,
                    k,
                    rank,
                )
                .await
            }
        };
        if !self.fans_out() {
            let items = one(&self.own).await?;
            self.note(
                items.len(),
                items.iter().map(|p| p.passage.episode_id.as_str()),
            );
            return Ok(Scoped {
                items,
                from: HashMap::new(),
            });
        }
        let (own, also) = tokio::join!(
            one(&self.own),
            futures::future::join_all(self.also.iter().map(one))
        );
        let lists = self.lists(own?, also, "passage search");
        let merged = dedup(
            merge(lists, |p: &PassageHit| p.score),
            |p| vec![p.passage.episode_id.as_str()],
            |p| Some(p.passage.source.as_str()),
            &HashMap::new(),
        );
        let out = scoped(merged, k, |p| p.passage.episode_id.clone());
        self.note(
            out.items.len(),
            out.items.iter().map(|p| p.passage.episode_id.as_str()),
        );
        Ok(out)
    }

    /// Each scope's list, the caller's own first; another scope's that failed is left out (and logged).
    fn lists<T>(&self, own: Vec<T>, also: Vec<Result<Vec<T>>>, what: &str) -> Vec<Vec<T>> {
        let mut lists = vec![own];
        for (s, r) in self.also.iter().zip(also) {
            lists.push(r.unwrap_or_else(|e| {
                eprintln!(
                    "{what} in {} (read by {}) failed: {}",
                    s.scope(),
                    self.own.scope(),
                    writer::redacted(&e)
                );
                vec![]
            }));
        }
        lists
    }

    /// The source of each episode the items of each list cite, read from that list's scope.
    async fn sources<T>(
        &self,
        store: &Store,
        lists: &[Vec<T>],
        cites: impl Fn(&T) -> Vec<&str>,
    ) -> Result<HashMap<String, String>> {
        let mut out = HashMap::new();
        for (i, list) in lists.iter().enumerate() {
            let ids: HashSet<&str> = list.iter().flat_map(&cites).collect();
            out.extend(episode_sources(store, self.scope_at(i), &ids).await?);
        }
        Ok(out)
    }

    /// An episode's body, from the first scope read that holds one the caller may read, with that scope's index.
    pub async fn episode_body(
        &self,
        store: &Store,
        who: &Principal,
        episode_id: &str,
    ) -> Result<Option<(usize, Vec<u8>)>> {
        for (i, s) in self.sessions().enumerate() {
            if let Some(b) = s.episode_body(store, who, episode_id).await? {
                self.note(1, std::iter::once(episode_id));
                return Ok(Some((i, b)));
            }
        }
        Ok(None)
    }

    /// These episodes as the caller may read them ([`Session::episodes`]), each from the first scope read that
    /// holds it, with that scope's index.
    pub async fn episodes(
        &self,
        store: &Store,
        who: &Principal,
        ids: &[String],
    ) -> Result<HashMap<String, (usize, Episode)>> {
        let mut out: HashMap<String, (usize, Episode)> = HashMap::new();
        for (i, s) in self.sessions().enumerate() {
            let todo: Vec<String> = ids
                .iter()
                .filter(|id| !out.contains_key(*id))
                .cloned()
                .collect();
            if todo.is_empty() {
                break;
            }
            for (id, ep) in s.episodes(store, who, &todo).await? {
                out.insert(id, (i, ep));
            }
        }
        self.note(out.len(), out.keys().map(String::as_str));
        Ok(out)
    }

    /// [`Session::read_in`] in every scope read, the caller's own first.
    pub async fn read_in(
        &self,
        store: &Store,
        who: &Principal,
        col: search::Col,
        values: &[String],
        valid_at: Option<i64>,
    ) -> Result<Vec<Fact>> {
        let mut out = self.own.read_in(store, who, col, values, valid_at).await?;
        for s in &self.also {
            out.extend(s.read_in(store, who, col, values, valid_at).await?);
        }
        Ok(out)
    }

    /// [`Session::read_where`] in every scope read, the caller's own first.
    pub async fn read_where(
        &self,
        store: &Store,
        who: &Principal,
        filter: &search::Filter,
        valid_at: Option<i64>,
    ) -> Result<Vec<Fact>> {
        let mut out = self.own.read_where(store, who, filter, valid_at).await?;
        for s in &self.also {
            out.extend(s.read_where(store, who, filter, valid_at).await?);
        }
        Ok(out)
    }

    /// The first scope read (its index) holding episode `id` where the caller may read it.
    pub fn holder(&self, id: &str, who: &Principal) -> Option<usize> {
        self.sessions().position(|s| s.can_read_episode(id, who))
    }

    /// An episode's type, in the first scope read that holds it.
    pub fn episode_type(&self, episode: &str) -> Option<&'static str> {
        self.sessions().find_map(|s| s.vis().episode_type(episode))
    }

    /// The episodes of type `kind` the caller may read, in every scope read.
    pub fn episodes_of_type(&self, kind: &str, who: &Principal) -> BTreeSet<String> {
        self.sessions()
            .flat_map(|s| s.vis().episodes_of_type(kind, who))
            .collect()
    }

    /// Each fact's feedback ([`crate::feedback::tallies`]), from every scope read: a fact's id is its own scope's.
    pub async fn tallies(
        &self,
        store: &Store,
        who: &Principal,
        facts: &[(String, Vec<String>)],
    ) -> Result<HashMap<String, crate::feedback::Tally>> {
        let mut out = crate::feedback::tallies(store, self.own.scope(), who, facts).await?;
        for s in &self.also {
            for (id, t) in crate::feedback::tallies(store, s.scope(), who, facts).await? {
                out.entry(id).or_insert(t);
            }
        }
        Ok(out)
    }

    /// What has gone stale or overdue among `facts` (`crate::freshness`), by fact id: a read's current facts as of
    /// `valid_at` (`i64::MAX`: now). A read over history (None) marks nothing, and nor does a scope whose own
    /// setting (or the server's) leaves marks off. The schemas of every scope read say what to mark. `whole`: the
    /// facts hold their entities' plans and statuses; else those are read beside them, so a status can read
    /// overdue.
    pub async fn marks(
        &self,
        store: &Store,
        who: &Principal,
        facts: &[&Fact],
        valid_at: Option<i64>,
        whole: bool,
    ) -> Result<HashMap<String, freshness::Mark>> {
        let Some(at) = valid_at.map(|t| if t == i64::MAX { self.now() } else { t }) else {
            return Ok(HashMap::new());
        };
        let held: Vec<std::sync::Arc<Schema>> = self
            .sessions()
            .filter_map(|s| s.vis().schema.clone())
            .collect();
        let schemas: Vec<&Schema> = held.iter().map(|s| s.as_ref()).collect();
        if facts.is_empty()
            || !freshness::any(&schemas)
            || !freshness::on(store, self.own.scope()).await
        {
            return Ok(HashMap::new());
        }
        let mut beside = vec![];
        if !whole {
            let (ids, preds) = freshness::beside(&schemas, facts);
            if !ids.is_empty() {
                let lits = |v: BTreeSet<String>| v.into_iter().map(search::Lit::Str).collect();
                let only = search::Filter::And(vec![
                    search::Filter::In(search::Col::Entity, lits(ids)),
                    search::Filter::In(search::Col::Predicate, lits(preds)),
                ]);
                beside = self.read_where(store, who, &only, valid_at).await?;
            }
        }
        let have: HashSet<&str> = facts.iter().map(|f| f.fact_id.as_str()).collect();
        let all: Vec<&Fact> = facts
            .iter()
            .copied()
            .chain(beside.iter().filter(|f| !have.contains(f.fact_id.as_str())))
            .collect();
        Ok(freshness::marks(&schemas, &all, at))
    }
}

/// A cursor's pins as its text: each scope's, `~` between (a pin's text is URL-safe base64, which has none).
pub(super) fn encode_pins(pins: &[PagePin]) -> String {
    pins.iter()
        .map(PagePin::encode)
        .collect::<Vec<_>>()
        .join("~")
}

/// The pins of a cursor's text; none when it isn't one.
pub(super) fn decode_pins(text: &str) -> Option<Vec<PagePin>> {
    text.split('~').map(PagePin::decode).collect()
}

/// The lists as one, by score: each step takes the best of the lists' next items (the earlier list's on a tie), so
/// each list's own order is kept.
fn merge<T>(lists: Vec<Vec<T>>, score: impl Fn(&T) -> f32) -> Vec<(usize, T)> {
    let mut lists: Vec<std::iter::Peekable<std::vec::IntoIter<T>>> = lists
        .into_iter()
        .map(|l| l.into_iter().peekable())
        .collect();
    let mut out = vec![];
    loop {
        let mut best: Option<(usize, f32)> = None;
        for (i, l) in lists.iter_mut().enumerate() {
            if let Some(x) = l.peek() {
                let s = score(x);
                if best.is_none_or(|(_, b)| s > b) {
                    best = Some((i, s));
                }
            }
        }
        let Some((i, _)) = best else {
            return out;
        };
        out.extend(lists[i].next().map(|x| (i, x)));
    }
}

/// `merged` without what another scope already showed: an item whose episodes (by id, or by the record they are
/// of: `source` of the item itself, else `sources` of its episodes) an item kept from another scope has. A note a
/// person remembered is no record of anything but itself (its source is their own label), so only its id counts.
fn dedup<T>(
    merged: Vec<(usize, T)>,
    episodes: impl Fn(&T) -> Vec<&str>,
    source: impl Fn(&T) -> Option<&str>,
    sources: &HashMap<String, String>,
) -> Vec<(usize, T)> {
    let record = |s: &str| (!s.starts_with(crate::REMEMBER_SOURCE)).then(|| format!("source:{s}"));
    let mut held: HashMap<String, usize> = HashMap::new();
    let mut out = vec![];
    for (i, x) in merged {
        let mut keys: Vec<String> = vec![];
        for e in episodes(&x) {
            keys.push(format!("id:{e}"));
            keys.extend(sources.get(e).and_then(|s| record(s)));
        }
        keys.extend(source(&x).and_then(record));
        if keys.iter().any(|k| held.get(k).is_some_and(|&j| j != i)) {
            continue;
        }
        for k in keys {
            held.entry(k).or_insert(i);
        }
        out.push((i, x));
    }
    out
}

/// The first `k` of `merged`, and the scope of each from a scope other than the caller's own.
fn scoped<T>(merged: Vec<(usize, T)>, k: usize, key: impl Fn(&T) -> String) -> Scoped<T> {
    let mut from = HashMap::new();
    let mut items = Vec::with_capacity(k.min(merged.len()));
    for (i, x) in merged.into_iter().take(k) {
        if i > 0 {
            from.insert(key(&x), i);
        }
        items.push(x);
    }
    Scoped { items, from }
}

/// The sources of these episodes in `scope`, from its episodes' rows (no body is read).
async fn episode_sources(
    store: &Store,
    scope: &Scope,
    ids: &HashSet<&str>,
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    for batches in store.episode_files(scope).await?.values() {
        for b in batches.iter() {
            let (Some(id), Some(source)) =
                (b.column_by_name("episode_id"), b.column_by_name("source"))
            else {
                continue;
            };
            let (id, source) = (id.as_string::<i32>(), source.as_string::<i32>());
            for row in 0..id.len() {
                if ids.contains(id.value(row)) {
                    out.insert(id.value(row).to_string(), source.value(row).to_string());
                }
            }
        }
    }
    Ok(out)
}
