//! `search_memory`'s answer as an agent reads it. The facts a search finds are grouped into one result per entity
//! (an item an episode opens, such as `objection:<episode>#3`, joins the entity its episode is the record of, on one
//! line). Each result shows the entity (its other current facts after the ones found, a few lines within an
//! equal share of the budget, the rest counted), is named by its
//! display name and its episode's title and time, names people and ids by name, and shows the episode's line that
//! best matches the query; the whole answer is compact text within a character budget. Facts that share no word
//! with the query are left out when others do, and source text that matches where no fact does follows the results.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use serde_json::{Value, json};

use super::reads::{self, Reads};
use super::{ToolError, page_start, unmentioned};
use crate::episode_types::{self, Header, Unit};
use crate::{Episode, Principal, Store, search};

/// Facts one search reads before grouping them: every page of its results is cut from these.
const POOL: usize = 100;
/// Results one page shows unless `k` says otherwise, and the most it may ask for.
pub(super) const DEFAULT_K: usize = 10;
pub(super) const MAX_RESULTS: usize = 50;
/// The answer's length in characters unless `max_chars` says otherwise, and its bounds.
pub(super) const DEFAULT_CHARS: usize = 8000;
pub(super) const MIN_CHARS: usize = 1000;
pub(super) const MAX_CHARS: usize = 60000;
/// Fact lines one result shows at most, so each of the `k` results has room; the rest are counted.
const LINES: usize = 4;
/// A line that facts with no quote share takes no more values once this long.
const MERGED_CHARS: usize = 300;
/// A `said:` line's characters beyond its excerpt (the speaker and where it is).
const SAID_EXTRA: usize = 40;
/// A value or quote is cut to this many characters, a line of source text to this.
const QUOTE_CHARS: usize = 200;
const SNIPPET_CHARS: usize = 240;
/// Ids named per page, beyond the people its episodes name.
const NAMES: usize = 200;
/// Episodes read to place items, and per result to name it.
const ITEM_EPISODES: usize = 32;
const EPISODES_PER_RESULT: usize = 3;
/// Passages searched, and at most this many shown, from episodes no result cites (first page only).
const PASSAGE_POOL: usize = 8;
const PASSAGES: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Order {
    Relevance,
    Newest,
}

/// One `search_memory` call in the text format.
pub(super) struct Ask<'a> {
    pub query: &'a str,
    pub k: usize,
    pub page: &'a Value,
    pub tags: &'a [String],
    pub only: Option<&'a search::Filter>,
    /// The memory as it stood then (None: any time, `i64::MAX`: now), recency ranked as of `rank_at`, and
    /// only facts first stated since `since` (`search_memory`'s `as_of`, `start_date` and `end_date`).
    pub valid_at: Option<i64>,
    pub rank_at: Option<i64>,
    pub since: Option<i64>,
    /// The source text shown keeps to the same dates: from episodes that occurred from `since` to `until`
    /// (`as_of` or `end_date`, whichever is earlier).
    pub until: Option<i64>,
    /// `search_memory`'s `episode_type`: the facts come only from episodes of these types (`only` says so),
    /// and the source text too. Empty: any type.
    pub kinds: &'a [&'a str],
    pub order: Order,
    pub max_chars: usize,
    /// Whether the search names dates (`start_date` or `end_date`): its first page then says what arrived from
    /// `since` to `until` ([`crate::arrivals`]).
    pub dated: bool,
}

/// The query's words as results are judged by them: the entity types it names (`meetings`: results of that type
/// come first) and the rest, its content words, which a fact must share to be kept when any fact does.
struct Words {
    types: HashSet<String>,
    content: HashSet<String>,
    /// The query's own spelling of its content words, for the answer to quote.
    shown: Vec<String>,
}

impl Words {
    fn of(query: &str, hits: &[search::Hit]) -> Self {
        let asked = search::stems(query);
        let mut type_stems = HashSet::new();
        let mut types = HashSet::new();
        for t in hits.iter().filter_map(|h| type_of(&h.fact.entity)) {
            let s = search::stems(&t.replace('_', " "));
            if !s.is_empty() && s.is_subset(&asked) {
                types.insert(t.to_string());
            }
            type_stems.extend(s);
        }
        let content: HashSet<String> = asked.difference(&type_stems).cloned().collect();
        let mut shown: Vec<String> = search::terms(query)
            .into_iter()
            .filter(|t| search::informative(t))
            .filter(|t| !search::stems(t).is_disjoint(&content))
            .collect();
        shown.sort();
        Words {
            types,
            content,
            shown,
        }
    }

    /// Whether `text` shares a content word.
    fn in_text(&self, text: &str) -> bool {
        !self.content.is_empty() && !search::stems(text).is_disjoint(&self.content)
    }

    fn in_fact(&self, f: &crate::Fact) -> bool {
        self.in_text(&format!(
            "{} {} {} {}",
            f.entity,
            f.predicate.replace('_', " "),
            f.value,
            f.quote.as_deref().unwrap_or_default()
        ))
    }
}

/// `deal` of `deal:42`; None for an id with no type.
fn type_of(id: &str) -> Option<&str> {
    id.split_once(':')
        .map(|(t, _)| t)
        .filter(|t| !t.is_empty() && !t.contains(char::is_whitespace))
}

/// Whether `id` names an item an episode opens: `<type>:<episode>#<n>`.
fn is_item(id: &str) -> bool {
    id.split_once(':')
        .and_then(|(_, rest)| rest.split_once('#'))
        .is_some_and(|(ep, n)| {
            !ep.is_empty()
                && ep.bytes().all(|b| b.is_ascii_hexdigit())
                && n.bytes().all(|b| b.is_ascii_digit())
        })
}

/// An episode read to show a result: its type's name, header, units and when it happened.
struct Source {
    kind: &'static str,
    header: Header,
    units: Vec<Unit>,
    occurred_at: i64,
}

impl Source {
    fn of(ep: &Episode) -> Self {
        let t = episode_types::of(ep);
        Source {
            kind: t.map_or("episode", |t| t.name()),
            header: t.map(|t| t.header(ep)).unwrap_or_default(),
            units: t.and_then(|t| t.units(ep).ok()).unwrap_or_default(),
            occurred_at: ep.occurred_at,
        }
    }
}

/// Reads the episodes of `ids` not read yet into `sources`, from every scope read.
async fn read(
    reads: &Reads,
    store: &Store,
    who: &Principal,
    ids: Vec<String>,
    sources: &mut HashMap<String, Source>,
) -> Result<()> {
    let todo: Vec<String> = ids
        .into_iter()
        .filter(|id| !sources.contains_key(id))
        .collect();
    if todo.is_empty() {
        return Ok(());
    }
    for (id, (_, ep)) in reads.episodes(store, who, &todo).await? {
        sources.insert(id, Source::of(&ep));
    }
    Ok(())
}

/// One result: an entity and the facts found about it and its items, best first.
struct Card {
    entity: String,
    hits: Vec<search::Hit>,
    first: usize,
}

impl Card {
    fn newest(&self) -> i64 {
        self.hits
            .iter()
            .map(|h| h.fact.valid_from)
            .max()
            .unwrap_or_default()
    }

    /// The episodes its facts cite, in the order its facts rank.
    fn episodes(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.hits
            .iter()
            .flat_map(|h| h.fact.source_episodes.iter())
            .filter(|e| seen.insert(e.as_str()))
            .cloned()
            .collect()
    }
}

/// The text answer to `ask`, from every scope read (`Reads`): each result and source from another scope than the
/// caller's own names it.
pub(super) async fn search(
    reads: &Reads,
    store: &Store,
    who: &Principal,
    ask: &Ask<'_>,
) -> Result<String> {
    let fingerprint = {
        use sha2::Digest;
        let mut asked = json!([
            "results",
            ask.query,
            ask.tags,
            ask.only.map(|f| format!("{f:?}")),
            ask.valid_at,
            ask.rank_at,
            format!("{:?}", ask.order)
        ]);
        if reads.fans_out() {
            asked
                .as_array_mut()
                .unwrap()
                .push(json!(reads.scope_names()));
        }
        let h = sha2::Sha256::digest(asked.to_string().as_bytes());
        crate::auth::hex(&h)[..16].to_string()
    };
    let (offset, pin) = page_start(ask.page, &fingerprint)?;
    // A paged search's first page pins what it reads, so its cursor names exactly that.
    let pin = match (pin, ask.page) {
        (Some(p), _) => Some(p),
        (None, Value::Null) => None,
        (None, _) => Some(reads.pins(store, reads.now()).await?),
    };
    let reads = match &pin {
        Some(p) => reads.resume(store, p).await?,
        None => reads.clone(),
    };
    let now = reads.now();
    let rank = search::Rank {
        as_of: Some(
            ask.rank_at
                .or(ask.valid_at.filter(|&t| t < i64::MAX))
                .unwrap_or(now),
        ),
        diverse: false,
        // Results are grouped per entity below.
        group: 0,
        classic: false,
    };
    // Whole records and names as they stood when the search asks about: then, the window's end, or now.
    let record_at = ask.valid_at.or(ask.rank_at).or(Some(i64::MAX));
    let mut only: Vec<search::Filter> = ask
        .tags
        .iter()
        .map(|t| search::Filter::Contains(search::Col::Tags, format!(",{t},")))
        .collect();
    only.extend(ask.only.cloned());
    let only = (!only.is_empty()).then_some(search::Filter::And(only));
    let facts = reads.search(
        store,
        who,
        ask.query,
        POOL,
        ask.valid_at,
        only.as_ref(),
        rank,
    );
    // Source text that matches, for the first page (see below): searched while the facts are.
    let passage_filter = search::passages::PassageFilter {
        occurred_from: ask.since,
        occurred_to: ask.until,
        ..Default::default()
    };
    let source_text = async {
        match offset {
            0 => Some(
                reads
                    .passages(
                        store,
                        who,
                        Some(ask.query),
                        &passage_filter,
                        PASSAGE_POOL,
                        rank,
                    )
                    .await
                    .map(|p| p.items),
            ),
            _ => None,
        }
    };
    // What arrived in the window, for a dated search's first page: from the episodes' rows, whatever the words.
    let arrived = async {
        match (offset, ask.dated) {
            (0, true) => {
                let a = crate::arrivals::Ask {
                    from: ask.since,
                    to: ask.until,
                    kinds: &[],
                    types: ask.kinds,
                    list: super::ARRIVALS_LISTED,
                    ..Default::default()
                };
                Some(reads.arrivals(store, who, &a).await)
            }
            _ => None,
        }
    };
    let (pool, found, arrived) = tokio::join!(facts, source_text, arrived);
    // Not counted, the search still answers.
    let arrived = arrived.and_then(|a| {
        a.map_err(|e| eprintln!("search_memory: arrivals: {}", super::writer::redacted(&e)))
            .ok()
    });
    let reads::Scoped {
        items: pool,
        from: hit_scope,
    } = pool?;

    // A name nothing mentions, said once, on the first page.
    let missing = match offset {
        0 => {
            let as_json: Vec<Value> = pool
                .iter()
                .map(|h| json!({"entity": h.fact.entity, "value": h.fact.value, "quote": h.fact.quote}))
                .collect();
            unmentioned(&reads, store, who, ask.query, &as_json).await?
        }
        _ => vec![],
    };

    // Facts that share a content word with the query, when any does; else what is nearest in meaning.
    let words = Words::of(ask.query, &pool);
    let total = pool.len();
    let shared = pool.iter().any(|h| words.in_fact(&h.fact));
    let pool: Vec<search::Hit> = match shared {
        true => pool
            .into_iter()
            .filter(|h| words.in_fact(&h.fact))
            .collect(),
        false => pool,
    };
    let left_out = total - pool.len();

    // Claims of absence ("no new tickets since nine") that what the store received since contradicts: left out of a
    // read as of now, said how many, and their episodes' text with them (`crate::absence`). A read as of a date or
    // over history keeps them, marked.
    let mut pool = pool;
    let mut gone: HashSet<String> = HashSet::new();
    let contradicted = match ask.valid_at {
        Some(i64::MAX) => {
            let facts: Vec<&crate::Fact> = pool.iter().map(|h| &h.fact).collect();
            let marks = reads.absence(store, who, &facts, ask.valid_at).await;
            let before = pool.len();
            pool.retain(|h| {
                let out = matches!(
                    marks.get(&h.fact.fact_id),
                    Some(crate::freshness::Mark::Contradicted { .. })
                );
                if out {
                    gone.extend(h.fact.source_episodes.iter().cloned());
                }
                !out
            });
            before - pool.len()
        }
        _ => 0,
    };

    // Items join the entity their episode is the record of.
    let mut sources: HashMap<String, Source> = HashMap::new();
    let item_episodes: Vec<String> = {
        let mut seen = HashSet::new();
        pool.iter()
            .filter(|h| is_item(&h.fact.entity))
            .filter_map(|h| h.fact.source_episodes.first())
            .filter(|e| seen.insert(e.as_str()))
            .take(ITEM_EPISODES)
            .cloned()
            .collect()
    };
    read(&reads, store, who, item_episodes, &mut sources).await?;
    let home = |h: &search::Hit| -> String {
        if is_item(&h.fact.entity)
            && let Some(subject) = h
                .fact
                .source_episodes
                .first()
                .and_then(|e| sources.get(e))
                .and_then(|s| s.header.subject.clone())
        {
            return subject;
        }
        h.fact.entity.clone()
    };
    let mut cards: Vec<Card> = vec![];
    let mut at: HashMap<String, usize> = HashMap::new();
    for (i, h) in pool.into_iter().enumerate() {
        let key = home(&h);
        match at.get(&key) {
            Some(&c) => cards[c].hits.push(h),
            None => {
                at.insert(key.clone(), cards.len());
                cards.push(Card {
                    entity: key,
                    hits: vec![h],
                    first: i,
                });
            }
        }
    }
    // The types the query names first; then by rank, or newest first.
    let named = |c: &Card| {
        std::iter::once(c.entity.as_str())
            .chain(c.hits.iter().map(|h| h.fact.entity.as_str()))
            .filter_map(type_of)
            .any(|t| words.types.contains(t))
    };
    match ask.order {
        Order::Relevance => cards.sort_by_key(|c| (!named(c), c.first)),
        Order::Newest => cards.sort_by_key(|c| (!named(c), -c.newest(), c.first)),
    }
    if offset > 0 && offset >= cards.len() {
        return Err(ToolError(format!(
            "this search has {} results: offset {offset} is past the last",
            cards.len()
        ))
        .into());
    }
    let window: Vec<&Card> = cards.iter().skip(offset).take(ask.k).collect();

    // Each shown result whole: the other current facts about its entity and the items it shows. A one-valued
    // attribute the search found keeps the version it found; one it didn't find, its newest.
    let ids: Vec<String> = {
        let mut seen = HashSet::new();
        window
            .iter()
            .flat_map(|c| std::iter::once(&c.entity).chain(c.hits.iter().map(|h| &h.fact.entity)))
            .filter(|id| seen.insert(id.as_str()))
            .cloned()
            .collect()
    };
    let read_facts = reads
        .read_in(store, who, search::Col::Entity, &ids, record_at)
        .await?;
    // One-valued where a scope read says so (the caller's own alone, unless it reads others).
    let one = |f: &crate::Fact| {
        reads.sessions().any(|r| {
            r.vis().schema.as_ref().is_some_and(|s| {
                s.card(f.entity_type.as_deref(), &f.predicate) == crate::query::schema::Card::One
            })
        })
    };
    // Facts a dream linked as duplicates (where the reader may read the link): a result shows one of each group.
    let linked: HashMap<String, String> = reads
        .sessions()
        .flat_map(|s| s.vis().duplicates(who))
        .collect();
    let rest: HashMap<&str, Vec<crate::Fact>> = window
        .iter()
        .map(|c| {
            let mut seen: HashSet<&str> = c.hits.iter().map(|h| h.fact.fact_id.as_str()).collect();
            let mut groups: HashSet<&str> = c
                .hits
                .iter()
                .filter_map(|h| linked.get(&h.fact.fact_id).map(String::as_str))
                .collect();
            let mut keys: HashSet<(&str, &str)> = c
                .hits
                .iter()
                .map(|h| (h.fact.entity.as_str(), h.fact.predicate.as_str()))
                .collect();
            let members: HashSet<&str> = std::iter::once(c.entity.as_str())
                .chain(c.hits.iter().map(|h| h.fact.entity.as_str()))
                .collect();
            let mut more: Vec<&crate::Fact> = read_facts
                .iter()
                .filter(|f| members.contains(f.entity.as_str()))
                .collect();
            // Newest first, save that a value a live decision approved comes first of its attribute's (`Vis::rank`).
            more.sort_by_key(|f| {
                let approved = reads.sessions().any(|r| r.vis().approved_at(f, i64::MAX));
                std::cmp::Reverse((approved, f.valid_from, f.fact_id.as_str()))
            });
            let more = more
                .into_iter()
                .filter(|f| seen.insert(f.fact_id.as_str()))
                .filter(|f| {
                    linked
                        .get(&f.fact_id)
                        .is_none_or(|g| groups.insert(g.as_str()))
                })
                .filter(|f| !one(f) || keys.insert((f.entity.as_str(), f.predicate.as_str())))
                .cloned()
                .collect();
            (c.entity.as_str(), more)
        })
        .collect();

    // Source text that matches where no fact does: the first page shows the best of it.
    let cited: HashSet<&str> = cards
        .iter()
        .flat_map(|c| c.hits.iter())
        .flat_map(|h| h.fact.source_episodes.iter().map(String::as_str))
        .chain(gone.iter().map(String::as_str))
        .collect();
    let mut passages: Vec<search::passages::PassageHit> = vec![];
    if let Some(found) = found {
        let mut seen = HashSet::new();
        // A scope with no source text indexed answers with facts alone.
        passages = found
            .unwrap_or_else(|e| {
                eprintln!("search_memory: passages: {e:#}");
                vec![]
            })
            .into_iter()
            .filter(|p| !cited.contains(p.passage.episode_id.as_str()))
            .filter(|p| {
                ask.kinds.is_empty()
                    || reads
                        .episode_type(&p.passage.episode_id)
                        .is_some_and(|k| ask.kinds.contains(&k))
            })
            .filter(|p| words.content.is_empty() || words.in_text(&p.passage.text))
            .filter(|p| seen.insert(p.passage.episode_id.clone()))
            .take(PASSAGES)
            .collect();
    }

    // Name the shown results by their episodes.
    let mut wanted: Vec<String> = window
        .iter()
        .flat_map(|c| c.episodes().into_iter().take(EPISODES_PER_RESULT))
        .collect();
    wanted.extend(passages.iter().map(|p| p.passage.episode_id.clone()));
    read(&reads, store, who, wanted, &mut sources).await?;
    // What to call an id: a person by the name their episodes give, anything by its display name.
    let mut names: HashMap<String, String> = HashMap::new();
    for s in sources.values() {
        for (id, name) in &s.header.people {
            if !name.trim().is_empty() {
                names
                    .entry(id.clone())
                    .or_insert_with(|| name.trim().to_string());
            }
        }
    }
    let unnamed: Vec<search::Lit> = {
        let mut seen = HashSet::new();
        window
            .iter()
            .flat_map(|c| {
                let facts = c
                    .hits
                    .iter()
                    .map(|h| &h.fact)
                    .chain(&rest[c.entity.as_str()]);
                std::iter::once(c.entity.as_str())
                    .chain(facts.map(|f| f.value.as_str()).filter(|v| is_id(v)))
            })
            .filter(|id| !names.contains_key(*id) && seen.insert(*id))
            .take(NAMES)
            .map(|id| search::Lit::Str(id.to_string()))
            .collect()
    };
    if !unnamed.is_empty() {
        let mut found = reads
            .read_where(
                store,
                who,
                &search::Filter::And(vec![
                    search::Filter::In(search::Col::Entity, unnamed),
                    search::Filter::Eq(
                        search::Col::Predicate,
                        search::Lit::Str(crate::query::schema::NAME.into()),
                    ),
                ]),
                record_at,
            )
            .await?;
        found.sort_by_key(|f| std::cmp::Reverse(f.valid_from));
        for f in found {
            if !f.value.trim().is_empty() {
                names
                    .entry(f.entity)
                    .or_insert_with(|| f.value.trim().to_string());
            }
        }
    }
    let shown_facts: Vec<(String, Vec<String>)> = window
        .iter()
        .flat_map(|c| {
            c.hits
                .iter()
                .map(|h| &h.fact)
                .chain(&rest[c.entity.as_str()])
        })
        .map(|f| (f.fact_id.clone(), f.source_episodes.clone()))
        .collect();
    let tallies = reads.tallies(store, who, &shown_facts).await?;
    // What has gone stale or overdue, where the scope asks (`crate::freshness`): said beside the fact. A result
    // holds its entities' every current fact, plans and statuses among them.
    // And what another fact says against one, where the scope keeps conflicts (`crate::conflicts`), that only an
    // agent wrote one (`crate::origin`), or how many independent sources say it, where more than one does
    // (`crate::corroboration`).
    let (marks, conflicts, origins) = {
        let all: Vec<&crate::Fact> = window
            .iter()
            .flat_map(|c| {
                c.hits
                    .iter()
                    .map(|h| &h.fact)
                    .chain(&rest[c.entity.as_str()])
            })
            .collect();
        let origins: HashMap<String, String> = all
            .iter()
            .filter_map(|f| {
                let note = match reads.fact_origin(f)?.note(f.valid_from) {
                    Some(n) => n,
                    None => match reads.corroboration(who, f, &all) {
                        n if n > 1 => format!("{n} sources say so"),
                        _ => return None,
                    },
                };
                Some((f.fact_id.clone(), note))
            })
            .collect();
        (
            reads.marks(store, who, &all, ask.valid_at, true).await?,
            reads.conflicts(store, who, &all, ask.valid_at).await?,
            origins,
        )
    };
    // The scope of each found fact that isn't the caller's own.
    let hit_scope: HashMap<String, String> = hit_scope
        .into_iter()
        .map(|(id, i)| (id, reads.scope_at(i).to_string()))
        .collect();

    let mut out = Text {
        sources: &sources,
        hit_scope: &hit_scope,
        names: &names,
        tallies: &tallies,
        marks: &marks,
        conflicts: &conflicts,
        origins: &origins,
        words: &words,
        query: ask.query,
        linked: &linked,
        refs: vec![],
        named: HashSet::new(),
    };
    let mut head = vec![];
    if !missing.is_empty() {
        head.push(format!(
            "Nothing in memory mentions {} (no fact, no source text you may read): the results share other words \
             only, so memory likely holds no answer about it.",
            missing.join(", ")
        ));
    }
    // What arrived, numbered first: its episodes are pointed at like a result's.
    let arrived = arrived
        .map(|(a, from)| out.arrived(&a, &from, &reads, ask.since, ask.until))
        .unwrap_or_default();
    let mut body = String::new();
    let mut shown = 0;
    // Room kept for the lines around the results: the head, what arrived, the sources, the cursor.
    let reserve = 400 + arrived.len();
    for (i, c) in window.iter().enumerate() {
        // Each result an equal share of what is left, less the sources it names, so one can't fill the answer.
        let left = ask
            .max_chars
            .saturating_sub(body.len() + 80 * out.refs.len() + reserve);
        let room = (left / (window.len() - i)).saturating_sub(160);
        let (refs, named) = (out.refs.clone(), out.named.clone());
        let text = out.card(offset + i + 1, c, &rest[c.entity.as_str()], room);
        let sources_len = 80 * out.refs.len();
        if shown > 0 && body.len() + text.len() + sources_len + reserve > ask.max_chars {
            (out.refs, out.named) = (refs, named);
            break;
        }
        body.push_str(&text);
        shown += 1;
    }
    let mut also = String::new();
    for p in &passages {
        let (refs, named) = (out.refs.clone(), out.named.clone());
        let text = out.passage(p);
        if body.len() + also.len() + text.len() + 80 * out.refs.len() + reserve > ask.max_chars {
            (out.refs, out.named) = (refs, named);
            break;
        }
        also.push_str(&text);
    }

    let mut lines = vec![];
    let mut summary = match cards.len() {
        0 => format!("No fact you can see matches \"{}\".", ask.query),
        n => format!(
            "{n} result{} for \"{}\"",
            if n == 1 { "" } else { "s" },
            ask.query
        ),
    };

    if !cards.is_empty() {
        let mut how = vec![];
        if !words.types.is_empty() {
            let mut t: Vec<&str> = words.types.iter().map(String::as_str).collect();
            t.sort();
            how.push(format!("{} first", t.join(", ")));
        }
        if ask.order == Order::Newest {
            how.push("newest first".into());
        }
        if !how.is_empty() {
            summary.push_str(&format!(" ({})", how.join(", ")));
        }
        summary.push_str(&format!(
            "; showing {}-{}.",
            offset + 1,
            offset + shown.max(1)
        ));
    }
    lines.push(summary);
    // Read across scopes, the answer says which.
    if reads.fans_out() {
        lines.push(format!(
            "Searched {}; a result or source from other than {} names its scope.",
            reads.scope_names().join(", "),
            reads.scope()
        ));
    }
    if left_out > 0 {
        lines.push(format!(
            "Left out {left_out} facts that share none of the words {}.",
            out.words.shown.join(", ")
        ));
    }
    if contradicted > 0 {
        lines.push(format!(
            "Left out {contradicted} claim{} that something didn't happen (nothing arrived, a feed stopped): what \
             the store received since contradicts {}. A search as_of a date shows {}, marked.",
            if contradicted == 1 { "" } else { "s" },
            if contradicted == 1 { "it" } else { "them" },
            if contradicted == 1 { "it" } else { "them" },
        ));
    }
    if !shared && !cards.is_empty() && !words.shown.is_empty() {
        lines.push(format!(
            "No fact has the words {}: these are only the nearest in meaning.",
            words.shown.join(", ")
        ));
    }
    lines.extend(head);
    let mut text = lines.join("\n");
    if !arrived.is_empty() {
        text.push_str("\n\n");
        text.push_str(arrived.trim_end());
    }
    text.push_str("\n\n");
    text.push_str(&body);
    if !also.is_empty() {
        text.push_str("\nSource text that matches, in episodes no result above cites:\n");
        text.push_str(&also);
    }
    if !out.refs.is_empty() {
        text.push_str("\nSources (get_episode reads one by its id):\n");
        for (i, id) in out.refs.iter().enumerate() {
            let kind = sources.get(id).map_or("episode", |s| s.kind);
            // From another scope than the caller's own: which.
            match reads.holder(id, who).filter(|&h| h > 0) {
                Some(h) => text.push_str(&format!(
                    "e{} {id} {kind} in {}\n",
                    i + 1,
                    reads.scope_at(h)
                )),
                None => text.push_str(&format!("e{} {id} {kind}\n", i + 1)),
            }
        }
    }
    if offset + shown < cards.len() {
        let next = match &pin {
            Some(p) => reads::encode_pins(p),
            None => reads::encode_pins(&reads.pins(store, now).await?),
        };
        text.push_str(&format!(
            "\n{} more: pass cursor \"{}.{fingerprint}.{next}\" for the next page.\n",
            cards.len() - offset - shown,
            offset + shown
        ));
    }
    Ok(text)
}

/// Renders results, numbering the episodes they cite (`e1`, `e2`) and naming each person once in full.
struct Text<'a> {
    sources: &'a HashMap<String, Source>,
    /// The scope of each found fact from another scope than the caller's own, by fact id.
    hit_scope: &'a HashMap<String, String>,
    names: &'a HashMap<String, String>,
    tallies: &'a HashMap<String, crate::feedback::Tally>,
    /// What has gone stale or overdue, by fact id (`crate::freshness`).
    marks: &'a HashMap<String, crate::freshness::Mark>,
    /// What another fact says against one, by fact id (`crate::conflicts`).
    conflicts: &'a HashMap<String, crate::conflicts::Conflict>,
    /// Where one's words came from, said beside it, by fact id: that only an agent wrote it (`crate::origin`), or how
    /// many independent sources say it (`crate::corroboration`).
    origins: &'a HashMap<String, String>,
    words: &'a Words,
    query: &'a str,
    /// Facts a dream linked as duplicates ([`crate::Vis::duplicates`]): the one shown cites the others' sources too.
    linked: &'a HashMap<String, String>,
    /// Episode ids in the order first shown: `e1` is the first.
    refs: Vec<String>,
    /// Ids already named with the id.
    named: HashSet<String>,
}

impl<'a> Text<'a> {
    fn eref(&mut self, episode: &str) -> String {
        let n = match self.refs.iter().position(|e| e == episode) {
            Some(i) => i + 1,
            None => {
                self.refs.push(episode.to_string());
                self.refs.len()
            }
        };
        format!("e{n}")
    }

    /// A value as a reader names it: an id by its name, with the id the first time.
    fn value(&mut self, v: &str) -> String {
        match self.names.get(v) {
            Some(name) if self.named.insert(v.to_string()) => format!("{name} ({v})"),
            Some(name) => name.clone(),
            None => clip(v, QUOTE_CHARS),
        }
    }

    /// `[e1 t192]`: the episode and where in it.
    fn pointer(&mut self, episode: &str, cite: Option<&Value>) -> String {
        let e = self.eref(episode);
        match cite.and_then(place) {
            Some(p) => format!("[{e} {p}]"),
            None => format!("[{e}]"),
        }
    }

    /// Result `n`, in about `room` characters.
    fn card(&mut self, n: usize, c: &Card, rest: &[crate::Fact], room: usize) -> String {
        let facts: Vec<&crate::Fact> = c.hits.iter().map(|h| &h.fact).chain(rest).collect();
        // Its episode: the one it is the record of, else the one most of its unquoted facts cite (their lines
        // leave it out).
        let home = c
            .episodes()
            .into_iter()
            .take(EPISODES_PER_RESULT)
            .find(|e| {
                self.sources
                    .get(e)
                    .is_some_and(|s| s.header.subject.as_deref() == Some(c.entity.as_str()))
            });
        let main = home.clone().or_else(|| {
            let mut count: Vec<(&str, usize)> = vec![];
            for e in facts
                .iter()
                .filter(|f| quote_of(f).is_none())
                .filter_map(|f| f.source_episodes.first())
            {
                match count.iter_mut().find(|(x, _)| x == e) {
                    Some((_, n)) => *n += 1,
                    None => count.push((e, 1)),
                }
            }
            let best = count.iter().map(|(_, n)| *n).max()?;
            count
                .iter()
                .find(|(_, n)| *n == best)
                .map(|(e, _)| e.to_string())
        });
        let when = home
            .as_ref()
            .and_then(|e| self.sources.get(e))
            .map_or(c.newest(), |s| s.occurred_at);
        let mut first = format!("{n}. {}", c.entity);
        let name = self.names.get(&c.entity).cloned();
        if let Some(name) = &name {
            first.push_str(&format!(" · {}", clip(name, 120)));
            self.named.insert(c.entity.clone());
        }
        first.push_str(&format!(" · {}", time(when)));
        if let Some(t) = home
            .as_ref()
            .and_then(|e| self.sources.get(e))
            .and_then(|s| s.header.title.as_ref())
        {
            first.push_str(&format!(" · \"{}\"", clip(t, 120)));
        }
        if let Some(e) = &main {
            let r = self.eref(e);
            first.push_str(&format!(" [{r}]"));
        }
        // Found in another scope than the caller's own: which.
        if let Some(scope) = c
            .hits
            .first()
            .and_then(|h| self.hit_scope.get(&h.fact.fact_id))
        {
            first.push_str(&format!(" · in {scope}"));
        }
        let mut out = vec![first];
        // What was said: the line of its episode that best matches the query, unless a fact shown cites it. Its
        // room is kept before the facts take theirs.
        let said = main
            .clone()
            .or_else(|| {
                c.episodes()
                    .into_iter()
                    .find(|e| self.sources.contains_key(e))
            })
            .and_then(|e| self.best_unit(&e).map(|u| (e, u)));
        let mut used = out[0].len()
            + said
                .as_ref()
                .map_or(0, |_| SNIPPET_CHARS * 3 / 2 + SAID_EXTRA);

        // The entity's facts, those the search found first, on as many lines as `room` holds (at most LINES, and
        // always one): those with no quote from one episode share a line, one per predicate. Each item is one
        // line, where its first fact ranks. The rest are only counted, so they name no source.
        let full = |lines: &[String], used: usize| {
            !lines.is_empty() && (lines.len() >= LINES || used >= room)
        };
        let mut lines: Vec<String> = vec![];
        let mut hidden = 0;
        let mut merged: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut cited_units: HashSet<String> = HashSet::new();
        let mut items_shown: HashSet<&str> = HashSet::new();
        for f in &facts {
            if f.entity != c.entity {
                if items_shown.insert(f.entity.as_str()) {
                    let of: Vec<&crate::Fact> = facts
                        .iter()
                        .filter(|g| g.entity == f.entity)
                        .copied()
                        .collect();
                    if full(&lines, used) {
                        hidden += of.len();
                        continue;
                    }
                    let line = self.item(&of, when, &mut cited_units);
                    used += line.len();
                    lines.push(line);
                }
                continue;
            }
            if name.is_some() && f.predicate == crate::query::schema::NAME {
                continue;
            }
            let episode = f.source_episodes.first().cloned().unwrap_or_default();
            // A stale fact's note, or a claim of absence's, says when it was stated, so its line doesn't again.
            let mark = self.marks.get(&f.fact_id);
            let stale = matches!(
                mark,
                Some(
                    crate::freshness::Mark::Stale { .. }
                        | crate::freshness::Mark::Absent { .. }
                        | crate::freshness::Mark::Contradicted { .. }
                )
            );
            let tail = self.tail(&[f], if stale { f.valid_from } else { when });
            let mut mark = mark.map_or(String::new(), |m| format!(" — {}", m.note(&f.predicate)));
            if let Some(c) = self.conflicts.get(&f.fact_id) {
                mark.push_str(&format!(" — {}", c.note()));
            }
            if let Some(n) = self.origins.get(&f.fact_id) {
                mark.push_str(&format!(" — {n}"));
            }
            let quote = quote_of(f);
            let cite = crate::cite_json(f);
            let key = (f.predicate.clone(), episode.clone());
            let merge = match quote {
                None if tail.is_empty() && mark.is_empty() => merged.get(&key).copied(),
                _ => None,
            };
            if let Some(i) = merge {
                let value = self.value(&f.value);
                if lines[i].contains(&value) {
                    continue;
                }
                if lines[i].chars().count() >= MERGED_CHARS {
                    hidden += 1;
                    continue;
                }
                used += value.len() + 2;
                lines[i].push_str(&format!(", {value}"));
                continue;
            }
            if full(&lines, used) {
                hidden += 1;
                continue;
            }
            if let Some(p) = cite.as_ref().and_then(place) {
                cited_units.insert(format!("{episode} {p}"));
            }
            let line = match quote {
                None if tail.is_empty() && mark.is_empty() => {
                    let value = self.value(&f.value);
                    merged.insert(key, lines.len());
                    let ptr = match episode.is_empty() || main.as_deref() == Some(&episode) {
                        true => String::new(),
                        false => format!("{} ", self.pointer(&episode, None)),
                    };
                    format!("{ptr}{}: {value}", f.predicate)
                }
                _ => {
                    let value = self.value(&f.value);
                    let q =
                        quote.map_or(String::new(), |q| format!(" \"{}\"", clip(q, QUOTE_CHARS)));
                    let here = cite.as_ref().and_then(place).is_some();
                    let ptr =
                        match episode.is_empty() || (!here && main.as_deref() == Some(&episode)) {
                            true => String::new(),
                            false => format!(" {}", self.pointer(&episode, cite.as_ref())),
                        };
                    format!("{}: {value}{q}{ptr}{tail}{mark}", f.predicate)
                }
            };
            // A duplicate a dream linked: the fact shown keeps the others' citations (search added their sources).
            let mut line = line;
            if self.linked.contains_key(&f.fact_id) {
                for e in f.source_episodes.iter().skip(1) {
                    let p = self.pointer(e, None);
                    line.push_str(&format!(" {p}"));
                }
            }
            used += line.len();
            lines.push(line);
        }
        out.extend(lines);
        if hidden > 0 {
            out.push(format!("+{hidden} more facts"));
        }
        if let Some((e, u)) = said {
            let cite = serde_json::to_value(&u.cite).ok();
            let at = cite.as_ref().and_then(place).unwrap_or_default();
            let text = u.text.to_lowercase();
            let repeats = facts
                .iter()
                .any(|f| f.value.chars().count() >= 4 && text.contains(&f.value.to_lowercase()));
            if !cited_units.contains(&format!("{e} {at}")) && !repeats {
                let line = self.said(&e, u);
                out.push(line);
            }
        }
        let mut text = String::new();
        for (i, l) in out.iter().enumerate() {
            text.push_str(if i == 0 { "" } else { "   " });
            text.push_str(l);
            text.push('\n');
        }
        text
    }

    /// An item on one line: its facts, the ones with the longest values first and ids last, then the quote of
    /// the first that has one, and where it is.
    fn item(&mut self, facts: &[&crate::Fact], when: i64, cited: &mut HashSet<String>) -> String {
        let mut order: Vec<&crate::Fact> = facts.to_vec();
        order.sort_by_key(|f| (is_id(&f.value), std::cmp::Reverse(f.value.chars().count())));
        let mut parts: Vec<(String, Vec<String>)> = vec![];
        for f in &order {
            let mut v = self.value(&f.value);
            if let Some(m) = self.marks.get(&f.fact_id) {
                v = format!("{v} — {}", m.note(&f.predicate));
            }
            if let Some(c) = self.conflicts.get(&f.fact_id) {
                v = format!("{v} — {}", c.note());
            }
            if let Some(n) = self.origins.get(&f.fact_id) {
                v = format!("{v} — {n}");
            }
            match parts.iter_mut().find(|(p, _)| *p == f.predicate) {
                Some((_, vs)) if !vs.contains(&v) => vs.push(v),
                Some(_) => {}
                None => parts.push((f.predicate.clone(), vec![v])),
            }
            if let (Some(e), Some(p)) = (
                f.source_episodes.first(),
                crate::cite_json(f).as_ref().and_then(place),
            ) {
                cited.insert(format!("{e} {p}"));
            }
        }
        let parts: Vec<String> = parts
            .into_iter()
            .map(|(p, vs)| format!("{p}: {}", vs.join(", ")))
            .collect();
        let quoted = order.iter().find(|f| quote_of(f).is_some());
        let mut line = format!("{}: {}", item_label(&facts[0].entity), parts.join("; "));
        if let Some(q) = quoted.and_then(|f| quote_of(f)) {
            line.push_str(&format!(" \"{}\"", clip(q, QUOTE_CHARS)));
        }
        let anchor = quoted.copied().unwrap_or(facts[0]);
        if let Some(e) = anchor.source_episodes.first() {
            let ptr = self.pointer(e, crate::cite_json(anchor).as_ref());
            line.push_str(&format!(" {ptr}"));
        }
        line.push_str(&self.tail(facts, when));
        line
    }

    /// ` (2026-06-03)` when the facts are from another day than their result, and what readers said of them.
    fn tail(&self, facts: &[&crate::Fact], when: i64) -> String {
        let mut tail = String::new();
        let from = facts.iter().map(|f| f.valid_from).max().unwrap_or(when);
        if day(from) != day(when) {
            tail.push_str(&format!(" ({})", time(from)));
        }
        let (up, down) = facts
            .iter()
            .filter_map(|f| self.tallies.get(&f.fact_id))
            .fold((0, 0), |(u, d), t| {
                (u + t.positive, d + t.negative + t.very_negative)
            });
        if up + down > 0 {
            tail.push_str(&format!(" (feedback +{up} -{down})"));
        }
        tail
    }

    /// What arrived from `since` to `until`, in a few lines: each kind of source's count and latest time, then the
    /// newest episodes not read for facts yet, each pointed at (`e1`) for get_episode.
    fn arrived(
        &mut self,
        a: &crate::arrivals::Arrivals,
        from: &HashMap<String, usize>,
        reads: &Reads,
        since: Option<i64>,
        until: Option<i64>,
    ) -> String {
        const KINDS: usize = 8;
        let window = match (since, until) {
            (Some(f), Some(t)) => format!("from {} to {}", time(f), time(t)),
            (Some(f), None) => format!("since {}", time(f)),
            (None, Some(t)) => format!("up to {}", time(t)),
            (None, None) => "at any time".into(),
        };
        // Said whatever the query's words: counted from the episodes' rows, not searched.
        if a.episodes == 0 {
            return format!("Arrived {window}, as ingested: nothing you may read.\n");
        }
        let mut kinds: Vec<String> = a
            .kinds
            .iter()
            .take(KINDS)
            .map(|k| {
                let unread = match k.unread {
                    0 => String::new(),
                    n => format!(", {n} not read for facts"),
                };
                format!(
                    "{} {} (latest {}{unread})",
                    k.kind,
                    k.episodes,
                    time(k.latest)
                )
            })
            .collect();
        if a.kinds.len() > KINDS {
            let rest: usize = a.kinds[KINDS..].iter().map(|k| k.episodes).sum();
            kinds.push(format!("{rest} of {} other kinds", a.kinds.len() - KINDS));
        }
        let mut text = format!(
            "Arrived {window}, as ingested (whatever the query's words): {} episode{}. {}.",
            a.episodes,
            if a.episodes == 1 { "" } else { "s" },
            kinds.join(", ")
        );
        if a.unread == 0 {
            text.push_str(" Every one has been read for facts.\n");
            return text;
        }
        text.push_str(&format!(
            " {} of them not read for facts yet, so a search finds {} only by {} words{}\n",
            a.unread,
            if a.unread == 1 { "it" } else { "them" },
            if a.unread == 1 { "its" } else { "their" },
            if a.listed.is_empty() {
                "."
            } else {
                "; the newest:"
            }
        ));
        for u in &a.listed {
            let r = self.eref(&u.episode_id);
            let mut line = format!("- {} · {}", time(u.occurred_at), clip(&u.source, 80));
            if let Some(t) = u.title.as_deref().filter(|t| !t.is_empty()) {
                line.push_str(&format!(" · \"{}\"", clip(t, 120)));
            }
            if let Some(&i) = from.get(&u.episode_id) {
                line.push_str(&format!(" in {}", reads.scope_at(i)));
            }
            text.push_str(&format!("{line} [{r}]\n"));
        }
        if a.unread > a.listed.len() {
            text.push_str(&format!("+{} more\n", a.unread - a.listed.len()));
        }
        text
    }

    /// A passage's episode, named, with its best-matching line.
    fn passage(&mut self, p: &search::passages::PassageHit) -> String {
        let e = &p.passage.episode_id;
        let r = self.eref(e);
        let s = self.sources.get(e);
        let mut first = format!(
            "- {} · {}",
            time(s.map_or(p.passage.occurred_at, |s| s.occurred_at)),
            s.map_or(p.passage.source_kind.as_str(), |s| s.kind)
        );
        if let Some(t) = s.and_then(|s| s.header.title.as_ref()) {
            first.push_str(&format!(" · \"{}\"", clip(t, 120)));
        }
        if let Some(subject) = s.and_then(|s| s.header.subject.as_ref()) {
            first.push_str(&format!(" · {subject}"));
        }
        first.push_str(&format!(" [{r}]\n"));
        let line = match self.best_unit(e) {
            Some(u) => self.said(e, u),
            None => {
                let text = one_line(&p.passage.text);
                let title = s.and_then(|s| s.header.title.as_deref()).map(one_line);
                let text = title
                    .and_then(|t| text.strip_prefix(t.as_str()).map(str::trim_start))
                    .filter(|rest| !rest.is_empty())
                    .unwrap_or(&text);
                format!("said: \"{}\"", excerpt(text, self.query, SNIPPET_CHARS))
            }
        };
        format!("{first}   {line}\n")
    }

    /// The unit of episode `e` sharing the most content words with the query (the earliest of equals); None
    /// when none shares one.
    fn best_unit(&self, e: &str) -> Option<&'a Unit> {
        let sources: &'a HashMap<String, Source> = self.sources;
        let s = sources.get(e)?;
        let title = s.header.title.as_deref().map(one_line);
        s.units
            .iter()
            .filter(|u| title.as_deref() != Some(one_line(&u.text).as_str()))
            .map(|u| {
                let n = search::stems(&u.text)
                    .intersection(&self.words.content)
                    .count();
                (n, u)
            })
            .filter(|(n, _)| *n > 0)
            .max_by_key(|(n, u)| (*n, std::cmp::Reverse(u.index)))
            .map(|(_, u)| u)
    }

    fn said(&mut self, e: &str, u: &Unit) -> String {
        let cite = serde_json::to_value(&u.cite).ok();
        let ptr = self.pointer(e, cite.as_ref());
        let who = u
            .speaker
            .as_deref()
            .map_or(String::new(), |s| format!("{s}: "));
        format!(
            "said: {who}\"{}\" {ptr}",
            excerpt(&u.text, self.query, SNIPPET_CHARS)
        )
    }
}

/// Where in its episode a citation points: `t192` (a call's turn), `m<ts>` (a message), `p3` (an email's
/// paragraph), `@120` (an offset in running text), a record's field by name.
fn place(cite: &Value) -> Option<String> {
    match cite["kind"].as_str()? {
        "turn" => cite["turn"].as_u64().map(|t| format!("t{t}")),
        "message" => cite["ts"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(|t| format!("m{t}")),
        "email" => cite["paragraph"].as_u64().map(|p| format!("p{p}")),
        "span" => cite["offset"].as_u64().map(|o| format!("@{o}")),
        "field" => cite["field"].as_str().map(str::to_string),
        _ => None,
    }
}

/// A fact's quote, unless it only repeats the value.
fn quote_of(f: &crate::Fact) -> Option<&str> {
    f.quote
        .as_deref()
        .filter(|q| !q.trim().is_empty() && q.trim() != f.value.trim())
}

/// Whether a value is an id (`deal:42`, `person:a@b.com`) rather than words.
fn is_id(v: &str) -> bool {
    type_of(v).is_some() && !v.contains(char::is_whitespace)
}

/// `objection #9` of `objection:bb213ab1#9`.
fn item_label(id: &str) -> String {
    match (type_of(id), id.rsplit_once('#')) {
        (Some(t), Some((_, n))) => format!("{t} #{n}"),
        _ => id.to_string(),
    }
}

/// A time in microseconds as `2026-10-23 17:00 UTC`, or the date alone at midnight.
fn time(micros: i64) -> String {
    match chrono::DateTime::from_timestamp_micros(micros) {
        Some(t) if t.format("%H:%M:%S").to_string() == "00:00:00" => {
            t.format("%Y-%m-%d").to_string()
        }
        Some(t) => t.format("%Y-%m-%d %H:%M UTC").to_string(),
        None => micros.to_string(),
    }
}

fn day(micros: i64) -> i64 {
    micros.div_euclid(86_400_000_000)
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// About `n` characters of `text` on one line, marked when cut: the sentence (or else the clause) under way at
/// `n` is finished when it ends soon after; else it ends at the last sentence or clause before `n`, else at a
/// word. Never mid-word.
fn clip(text: &str, n: usize) -> String {
    let chars: Vec<char> = one_line(text).chars().collect();
    cut(&chars, n)
}

fn cut(chars: &[char], n: usize) -> String {
    let len = chars.len();
    if len <= n {
        return chars.iter().collect();
    }
    let ends = |i: usize| i + 1 == len || chars[i + 1].is_whitespace();
    let sentence = |i: usize| matches!(chars[i], '.' | '!' | '?') && ends(i);
    let clause = |i: usize| matches!(chars[i], ',' | ';' | ':' | '—' | '–') && ends(i);
    let end = (n..len.min(n + n / 2))
        .find(|&i| sentence(i))
        .map(|i| i + 1)
        .or_else(|| (n..len.min(n + n / 4)).find(|&i| clause(i)))
        .or_else(|| (n / 2..n).rev().find(|&i| sentence(i)).map(|i| i + 1))
        .or_else(|| (n / 2..n).rev().find(|&i| clause(i)))
        .or_else(|| (1..=n).rev().find(|&i| chars[i].is_whitespace()))
        .unwrap_or(n);
    let kept: String = chars[..end].iter().collect();
    let kept = kept.trim_end();
    match end >= len {
        true => kept.to_string(),
        false => format!("{}…", kept.trim_end_matches([',', ';', ':', '—', '–'])),
    }
}

/// About `n` characters of `text` around where it best matches `query` ([`best_window`]), on one line, from the
/// start of a sentence or word and ending as [`clip`] ends one.
fn excerpt(text: &str, query: &str, n: usize) -> String {
    let chars: Vec<char> = one_line(text).chars().collect();
    if chars.len() <= n {
        return chars.iter().collect();
    }
    let at = super::best_window_at(&chars, query, n);
    let start = match at {
        0 => 0,
        _ => {
            let soon = (at.saturating_sub(1)..(at + n / 3).min(chars.len() - 1))
                .find(|&i| matches!(chars[i], '.' | '!' | '?') && chars[i + 1].is_whitespace());
            let word = (at - 1..chars.len()).find(|&i| chars[i].is_whitespace());
            soon.or(word).map_or(at, |i| i + 1)
        }
    };
    let rest = &chars[start..];
    let skip = rest.iter().take_while(|c| c.is_whitespace()).count();
    let body = cut(&rest[skip..], n);
    match start {
        0 => body,
        _ => format!("…{body}"),
    }
}
