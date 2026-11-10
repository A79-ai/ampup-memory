//! The query language: one JSON shape for HTTP (`/query`), MCP (`query_memory`) and the CLI.
//! It knows no domain. Types, predicates, value types and cardinality all come from the scope's
//! schema (`crate::query::schema`), so the same engine answers a sales scope, an engineering scope or
//! the test corpus.
//!
//! ```json
//! {"find": "entities", "type": "deal",
//!  "where": [{"attr": "stage", "in": ["proposal", "negotiation"]},
//!            {"attr": "amount", "gte": 50000},
//!            {"rel": "account", "to": {"where": [{"attr": "industry", "eq": "insurance"}]}}],
//!  "select": ["stage", "amount", "owner.name"], "order": [{"by": "amount", "dir": "desc"}],
//!  "time": "now", "limit": 20}
//! ```
//!
//! `find`: `entities` (cards: each entity's values), `facts`, `count` (with `group_by` and
//! `metrics`; `include: ["evidence"]` cites, per group, up to `limit` of the entities it counted with
//! the facts that put each there), `metrics` (the schema's named metrics with any of `tags`, each
//! answered as a count and kept for the reader until what they see changes), `changes` (what started or ended in a window, or ever), `passages` (snippets of
//! source text with ids; the same ids read the whole text) or `schema`. `where`
//! clauses all hold; each is `{"attr", <op>: value}` (`eq ne in gt gte lt lte between exists
//! missing contains older_than within`), `{"rel": attr, "to": query}` (follow a reference),
//! `{"rel_from": attr, "from": query}` (entities another entity points at), `{"any": [...]}`
//! or `{"not": clause}`. `time` is `now`, `all`, `{"as_of": T}`, `{"between": [T1, T2]}` (what held at some point in it)
//! or `{"during": [T1, T2]}` (what was first stated in it: a call's mentions in a quarter).
//! Under `now` and `as_of`, a newer value of a one-valued predicate hides the older ones.
//! The caller's identity is never a clause: every fact is filtered by what they may see.

pub mod schema;
pub mod sql;
mod views;

/// Memory in `scope` (its namespace, on main) lost something: drop the metric answers kept past new
/// writes from before ([`views::Key::lasting`]).
pub(crate) fn drop_lasting(scope: &str, removals: u64) {
    views::drop_lasting(scope, removals);
}

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::conflicts::{self, Conflict};
use crate::freshness::{self, Mark};
use crate::search::{Col, Filter, Lit, Op};
use crate::{Fact, Principal, Session, Store};
use schema::{Card, Schema, ValueType};
use views::{Cur, View, What};

/// Rel and rel_from sub-queries nest at most this deep.
const MAX_DEPTH: usize = 2;
/// Ids per IN list sent to the index.
const CHUNK: usize = 500;
const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 500;
/// Passages one search returns unless `limit` says otherwise: snippets are short, so a call can
/// show this many.
const PASSAGE_LIMIT: usize = 20;
/// Passages ranked for `group_by`, before they are rolled up into groups.
const GROUP_POOL: usize = 400;
/// A group's best hit shows this much: groups are an overview, searched into by their id.
const GROUP_SNIPPET_BYTES: usize = 240;
/// Episodes one read by id may name.
const MAX_READ: usize = 10;
/// How far `part_of` is followed: a meeting of a deal of an account is two hops.
const PART_HOPS: usize = 2;
/// Said when a query finds nothing, so a reader doesn't take it for proof of absence.
const NOTHING_VISIBLE: &str = "Nothing you can see matches. Records you may not see are left out, so this doesn't show they don't exist.";

/// A query the caller got wrong: the message is meant for them.
#[derive(Debug)]
pub struct QueryError(pub String);

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for QueryError {}

fn bad<T>(msg: impl Into<String>) -> Result<T> {
    Err(QueryError(msg.into()).into())
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Find {
    Entities,
    Facts,
    Count,
    Changes,
    /// Chunks of episode text (emails, notes, call summaries), filtered by `about` and
    /// `evidence`, ranked by `text`.
    Passages,
    Schema,
    /// The schema's named metrics (those with any of `tags`, or all), each answered as a count.
    Metrics,
}

/// What passages must be about: these entity ids, or the entities a sub-query selects.
#[derive(Clone, Debug)]
pub enum About {
    Ids(Vec<String>),
    Query(Box<Query>),
}

/// Which episodes passages may come from: their source kinds and when they happened.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EvidenceFilter {
    pub sources: Vec<String>,
    pub from: Option<i64>,
    pub to: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Time {
    Now,
    All,
    AsOf(i64),
    Between(i64, i64),
    /// Facts first stated within the window (`valid_from` in it): what was said or happened then, as a
    /// call's mentions are, rather than what still held.
    During(i64, i64),
}

#[derive(Clone, Debug)]
pub enum Cond {
    Eq(Value),
    Ne(Value),
    In(Vec<Value>),
    Gt(Value),
    Gte(Value),
    Lt(Value),
    Lte(Value),
    Between(Value, Value),
    Exists,
    Missing,
    Contains(String),
    /// Older than this many microseconds before the query's reference time.
    OlderThan(i64),
    Within(i64),
    /// The value names one of these entities (a resolved `rel`).
    RefIn(Arc<BTreeSet<String>>),
}

#[derive(Clone, Debug)]
pub enum Clause {
    Attr { attr: String, conds: Vec<Cond> },
    Rel { attr: String, to: Box<Query> },
    RelFrom { attr: String, from: Box<Query> },
    Any(Vec<Clause>),
    Not(Box<Clause>),
}

#[derive(Clone, Debug)]
pub enum Order {
    Relevance,
    Time,
    Id,
    By { attr: String, desc: bool },
}

#[derive(Clone, Debug)]
pub enum Metric {
    Count,
    Sum(String),
    Avg(String),
    Min(String),
    Max(String),
}

#[derive(Clone, Debug)]
pub struct Query {
    pub find: Find,
    pub ty: Option<String>,
    pub ids: Vec<String>,
    pub name: Option<String>,
    pub clauses: Vec<Clause>,
    pub time: Time,
    pub text: Option<String>,
    pub select: Option<Vec<String>>,
    /// Attributes (or one-hop dotted paths such as `owner.name`) to group counts by.
    pub group_by: Vec<String>,
    pub metrics: Vec<Metric>,
    /// For `find: metrics`: only the named metrics with one of these tags (none: all).
    pub tags: Vec<String>,
    /// For `find: metrics`: only these named metrics (none: all).
    pub names: Vec<String>,
    /// For `find: metrics`: answer each anew rather than from the answer kept for this reader.
    pub refresh: bool,
    /// `where` as given: for `find: metrics`, the filters each metric is answered under.
    pub filters: Vec<Value>,
    pub order: Option<Order>,
    pub evidence: bool,
    pub about: Option<About>,
    pub evidence_filter: EvidenceFilter,
    /// Rank newer sources and facts a little higher (`recency`, on unless the query says
    /// `false`), as of the query's time.
    pub recency: bool,
    /// Spread a page of passages over kinds of source (`diverse`, on unless `false`).
    pub diverse: bool,
    pub limit: usize,
    /// Where this page starts (from `cursor`): results are in a total order (ties broken by
    /// id), so the next page is the next slice of the same ordering.
    pub offset: usize,
    /// When the query was parsed (µs), what `"now"` meant.
    pub now: i64,
    /// The query without `cursor` and `limit`, hashed: a cursor only continues its own query.
    pub fingerprint: String,
    /// What the first page read ([`crate::PagePin`], encoded): every next cursor carries it, so each page
    /// reads the memory the first one did.
    pub pin: Option<String>,
}

impl Query {
    /// This page of `items` (already in the query's order), and the cursor for the next one.
    pub fn page<T>(&self, items: Vec<T>) -> (Vec<T>, Option<String>) {
        let total = items.len();
        let page: Vec<T> = items
            .into_iter()
            .skip(self.offset)
            .take(self.limit)
            .collect();
        let end = self.offset + self.limit;
        let next = (end < total).then(|| match &self.pin {
            Some(p) => format!("{end}.{}.{p}", self.fingerprint),
            None => format!("{end}.{}", self.fingerprint),
        });
        (page, next)
    }

    /// Parse a query; `now` (µs) resolves `"now"` and relative times.
    pub fn parse(v: &Value, now: i64) -> Result<Query> {
        let Some(o) = v.as_object() else {
            return bad("a query is a JSON object");
        };
        const KEYS: &[&str] = &[
            "find", "type", "id", "match", "where", "time", "text", "select", "group_by",
            "metrics", "order", "include", "limit", "cursor", "about", "evidence", "recency",
            "diverse", "tags", "names", "refresh",
        ];
        if let Some(k) = o.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return bad(format!(
                "unknown query key {k:?} (known: {})",
                KEYS.join(", ")
            ));
        }
        let find = match o.get("find").and_then(Value::as_str).unwrap_or("entities") {
            "entities" => Find::Entities,
            "facts" => Find::Facts,
            "count" => Find::Count,
            "changes" => Find::Changes,
            "passages" => Find::Passages,
            "schema" => Find::Schema,
            "metrics" => Find::Metrics,
            f => {
                return bad(format!(
                    "find must be entities, facts, count, changes, passages, schema or metrics, not {f:?}"
                ));
            }
        };
        let ids = match o.get("id") {
            None | Some(Value::Null) => vec![],
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a
                .iter()
                .map(|x| x.as_str().map(String::from))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| QueryError("id must be a string or a list of strings".into()))?,
            _ => return bad("id must be a string or a list of strings"),
        };
        // With a type, a bare id is that type's (`{"type": "opportunity", "id": "006…"}` is
        // `opportunity:006…`), as a CRM's own ids are given; a passage's id is its episode's.
        let ty = o.get("type").and_then(Value::as_str).map(String::from);
        let ids = match (&ty, find) {
            (Some(t), f) if f != Find::Passages => ids
                .into_iter()
                .map(|id| {
                    if id.contains(':') {
                        id
                    } else {
                        format!("{t}:{id}")
                    }
                })
                .collect(),
            _ => ids,
        };
        let name = match o.get("match") {
            None | Some(Value::Null) => None,
            Some(m) => match m.get("name").and_then(Value::as_str) {
                Some(n) if !n.trim().is_empty() => Some(n.trim().to_string()),
                _ => return bad("match is {\"name\": \"…\"}"),
            },
        };
        let clauses = match o.get("where") {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(a)) => a
                .iter()
                .map(|c| parse_clause(c, now))
                .collect::<Result<_>>()?,
            Some(c @ Value::Object(_)) => vec![parse_clause(c, now)?],
            _ => return bad("where is a list of clauses"),
        };
        let time = match o.get("time") {
            None | Some(Value::Null) => Time::Now,
            Some(Value::String(s)) if s == "now" => Time::Now,
            Some(Value::String(s)) if s == "all" => Time::All,
            Some(Value::Object(t)) if t.contains_key("as_of") => {
                Time::AsOf(instant(&t["as_of"], now)?)
            }
            Some(Value::Object(t)) if t.contains_key("between") => match t["between"].as_array() {
                Some(b) if b.len() == 2 => {
                    let (a, z) = (instant(&b[0], now)?, window_end(&b[1], now)?);
                    if a > z {
                        return bad("time.between must run from earlier to later");
                    }
                    Time::Between(a, z)
                }
                _ => return bad("time.between is [from, to]"),
            },
            Some(Value::Object(t)) if t.contains_key("during") => match t["during"].as_array() {
                Some(b) if b.len() == 2 => {
                    let (a, z) = (instant(&b[0], now)?, window_end(&b[1], now)?);
                    if a > z {
                        return bad("time.during must run from earlier to later");
                    }
                    Time::During(a, z)
                }
                _ => return bad("time.during is [from, to]"),
            },
            _ => {
                return bad(
                    "time is \"now\", \"all\", {\"as_of\": T}, {\"between\": [T1, T2]} or {\"during\": [T1, T2]}",
                );
            }
        };
        let strings = |k: &str| -> Result<Option<Vec<String>>> {
            match o.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|x| x.as_str().map(String::from))
                    .collect::<Option<Vec<_>>>()
                    .map(Some)
                    .ok_or_else(|| QueryError(format!("{k} is a list of strings")).into()),
                _ => bad(format!("{k} is a list of strings")),
            }
        };
        let metrics = match o.get("metrics") {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(a)) => a.iter().map(parse_metric).collect::<Result<_>>()?,
            _ => return bad("metrics is a list"),
        };
        let order = match o.get("order") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(match s.as_str() {
                "relevance" => Order::Relevance,
                "time" => Order::Time,
                "id" => Order::Id,
                _ => {
                    return bad(
                        "order is relevance, time, id or [{\"by\": attr, \"dir\": asc|desc}]",
                    );
                }
            }),
            Some(Value::Array(a)) if a.len() == 1 => {
                let by = a[0]["by"]
                    .as_str()
                    .ok_or_else(|| QueryError("order needs \"by\"".into()))?;
                let desc = match a[0]["dir"].as_str().unwrap_or("asc") {
                    "asc" => false,
                    "desc" => true,
                    _ => return bad("order dir is asc or desc"),
                };
                Some(Order::By {
                    attr: by.to_string(),
                    desc,
                })
            }
            _ => return bad("order is relevance, time, id or [{\"by\": attr, \"dir\": asc|desc}]"),
        };
        let include = strings("include")?.unwrap_or_default();
        if let Some(i) = include.iter().find(|i| *i != "evidence") {
            return bad(format!("include knows only \"evidence\", not {i:?}"));
        }
        let limit = match o.get("limit") {
            None | Some(Value::Null) if find == Find::Passages => PASSAGE_LIMIT,
            None | Some(Value::Null) => DEFAULT_LIMIT,
            Some(l) => match l.as_u64() {
                Some(n) if n >= 1 => (n as usize).min(MAX_LIMIT),
                _ => return bad("limit is a positive integer"),
            },
        };
        let fingerprint = {
            use sha2::Digest;
            let mut rest = o.clone();
            rest.remove("cursor");
            rest.remove("limit");
            let h = sha2::Sha256::digest(Value::Object(rest).to_string().as_bytes());
            h.iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let offset = match o.get("cursor") {
            None | Some(Value::Null) => 0,
            Some(c) => {
                if !matches!(
                    find,
                    Find::Entities | Find::Facts | Find::Changes | Find::Passages
                ) {
                    return bad("cursor pages entities, facts, changes and passages");
                }
                let parsed = c
                    .as_str()
                    .map(|c| c.splitn(3, '.').collect::<Vec<_>>())
                    .filter(|p| p.len() >= 2)
                    .and_then(|p| Some((p[0].parse::<usize>().ok()?, p[1])));
                match parsed {
                    Some((n, fp)) if fp == fingerprint => n,
                    Some(_) => return bad("cursor belongs to a different query"),
                    None => return bad("cursor is the value a previous page returned"),
                }
            }
        };
        let about = match o.get("about") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(About::Ids(vec![s.clone()])),
            Some(Value::Array(a)) => Some(About::Ids(
                a.iter()
                    .map(|x| x.as_str().map(String::from))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| QueryError("about is an id, a list of ids or a query".into()))?,
            )),
            Some(sub @ Value::Object(_)) => Some(About::Query(Box::new(Query::parse(sub, now)?))),
            _ => return bad("about is an id, a list of ids or a query"),
        };
        let evidence_filter = match o.get("evidence") {
            None | Some(Value::Null) => EvidenceFilter::default(),
            Some(Value::Object(e)) => {
                if let Some(k) = e.keys().find(|k| *k != "source" && *k != "occurred") {
                    return bad(format!("evidence knows source and occurred, not {k:?}"));
                }
                let sources = match e.get("source") {
                    None | Some(Value::Null) => vec![],
                    Some(Value::String(s)) => vec![s.to_lowercase()],
                    Some(Value::Array(a)) => a
                        .iter()
                        .map(|x| x.as_str().map(str::to_lowercase))
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| QueryError("evidence.source is a list of kinds".into()))?,
                    _ => return bad("evidence.source is a list of kinds"),
                };
                let (mut from, mut to) = (None, None);
                match e.get("occurred") {
                    None | Some(Value::Null) => {}
                    Some(Value::Object(r)) => {
                        for (k, v) in r {
                            match k.as_str() {
                                "gte" => from = Some(instant(v, now)?),
                                "gt" => from = Some(instant(v, now)? + 1),
                                "lte" => to = Some(instant(v, now)?),
                                "lt" => to = Some(instant(v, now)? - 1),
                                "within" => from = Some(now - duration(v)?),
                                _ => {
                                    return bad(format!(
                                        "evidence.occurred takes gte, gt, lte, lt or within, not {k:?}"
                                    ));
                                }
                            }
                        }
                    }
                    _ => return bad("evidence.occurred is {\"gte\": T, \"lte\": T}"),
                }
                EvidenceFilter { sources, from, to }
            }
            _ => return bad("evidence is {\"source\": [...], \"occurred\": {...}}"),
        };
        let flag = |key: &str| match o.get(key) {
            None => Ok(true),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(QueryError(format!("{key} is true or false"))),
        };
        let (recency, diverse) = (flag("recency")?, flag("diverse")?);
        Ok(Query {
            find,
            ty,
            ids,
            name,
            clauses,
            time,
            text: o
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from),
            select: strings("select")?,
            group_by: parse_group_by(o.get("group_by"))?,
            metrics,
            tags: strings("tags")?.unwrap_or_default(),
            names: strings("names")?.unwrap_or_default(),
            refresh: match o.get("refresh") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return bad("refresh is true or false"),
            },
            filters: match o.get("where") {
                Some(Value::Array(a)) => a.clone(),
                Some(c @ Value::Object(_)) => vec![c.clone()],
                _ => vec![],
            },
            order,
            evidence: !include.is_empty(),
            about,
            evidence_filter,
            recency,
            diverse,
            limit,
            offset,
            fingerprint,
            now,
            pin: None,
        })
    }

    /// How text searches order their hits: recency as of the query's time (its `as_of`, the end
    /// of its window, or now; no later than the evidence window's end), and diversity for passages.
    pub fn rank(&self) -> crate::search::Rank {
        crate::search::Rank {
            as_of: self.recency.then_some(self.reference_time()),
            diverse: self.diverse,
            group: 0,
            classic: false,
        }
    }

    /// The time a text search is judged as of: the query's `as_of`, the end of its `between`
    /// window, or now, and no later than an `evidence.occurred` upper bound.
    pub fn reference_time(&self) -> i64 {
        let t = match self.time {
            Time::AsOf(t) => t,
            Time::Between(_, to) | Time::During(_, to) => to,
            Time::Now | Time::All => self.now,
        };
        self.evidence_filter.to.map_or(t, |to| t.min(to))
    }
}

fn parse_metric(m: &Value) -> Result<Metric> {
    if m.as_str() == Some("count") {
        return Ok(Metric::Count);
    }
    let Some(o) = m.as_object().filter(|o| o.len() == 1) else {
        return bad("a metric is \"count\" or {\"sum\"|\"avg\"|\"min\"|\"max\": attr}");
    };
    let (k, v) = o.iter().next().unwrap();
    let attr = v
        .as_str()
        .ok_or_else(|| QueryError(format!("{k} takes an attribute name")))?
        .to_string();
    Ok(match k.as_str() {
        "sum" => Metric::Sum(attr),
        "avg" => Metric::Avg(attr),
        "min" => Metric::Min(attr),
        "max" => Metric::Max(attr),
        _ => return bad(format!("unknown metric {k}")),
    })
}

fn parse_clause(c: &Value, now: i64) -> Result<Clause> {
    let Some(o) = c.as_object() else {
        return bad("a where clause is an object");
    };
    if let Some(a) = o.get("any") {
        let Some(a) = a.as_array() else {
            return bad("any is a list of clauses");
        };
        return Ok(Clause::Any(
            a.iter()
                .map(|x| parse_clause(x, now))
                .collect::<Result<_>>()?,
        ));
    }
    if let Some(n) = o.get("not") {
        return Ok(Clause::Not(Box::new(parse_clause(n, now)?)));
    }
    if let Some(attr) = o.get("rel").and_then(Value::as_str) {
        let to = o
            .get("to")
            .ok_or_else(|| QueryError("rel needs \"to\": a query".into()))?;
        return Ok(Clause::Rel {
            attr: attr.into(),
            to: Box::new(Query::parse(to, now)?),
        });
    }
    if let Some(attr) = o.get("rel_from").and_then(Value::as_str) {
        let from = o
            .get("from")
            .ok_or_else(|| QueryError("rel_from needs \"from\": a query".into()))?;
        return Ok(Clause::RelFrom {
            attr: attr.into(),
            from: Box::new(Query::parse(from, now)?),
        });
    }
    let Some(attr) = o.get("attr").and_then(Value::as_str) else {
        return bad("a where clause has attr, rel, rel_from, any or not");
    };
    let mut conds = vec![];
    for (k, v) in o.iter().filter(|(k, _)| *k != "attr") {
        conds.push(match k.as_str() {
            "eq" => Cond::Eq(v.clone()),
            "ne" => Cond::Ne(v.clone()),
            "in" => Cond::In(
                v.as_array()
                    .ok_or_else(|| QueryError("in takes a list".into()))?
                    .clone(),
            ),
            "gt" => Cond::Gt(v.clone()),
            "gte" => Cond::Gte(v.clone()),
            "lt" => Cond::Lt(v.clone()),
            "lte" => Cond::Lte(v.clone()),
            "between" => match v.as_array() {
                Some(b) if b.len() == 2 => Cond::Between(b[0].clone(), b[1].clone()),
                _ => return bad("between takes [low, high]"),
            },
            "exists" if v.as_bool() == Some(true) => Cond::Exists,
            "exists" if v.as_bool() == Some(false) => Cond::Missing,
            "missing" if v.as_bool() == Some(true) => Cond::Missing,
            "contains" => Cond::Contains(
                v.as_str()
                    .ok_or_else(|| QueryError("contains takes text".into()))?
                    .to_string(),
            ),
            "older_than" => Cond::OlderThan(duration(v)?),
            "within" => Cond::Within(duration(v)?),
            other => {
                return bad(format!(
                    "unknown operator {other:?} on {attr} (eq ne in gt gte lt lte between exists missing contains older_than within)"
                ));
            }
        });
    }
    if conds.is_empty() {
        conds.push(Cond::Exists);
    }
    let _ = now;
    Ok(Clause::Attr {
        attr: attr.into(),
        conds,
    })
}

/// `"now"`, microseconds since the epoch, a date or an RFC 3339 instant.
fn instant(v: &Value, now: i64) -> Result<i64> {
    match v {
        Value::String(s) if s == "now" => Ok(now),
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| QueryError("times are integer microseconds".into()).into()),
        Value::String(s) => schema::parse_instant(s)
            .map_err(|_| QueryError(format!("{s:?} is not a date or RFC 3339 time")).into()),
        _ => bad("a time is \"now\", microseconds, a date or an RFC 3339 instant"),
    }
}

/// The end of a `between` window: a plain date covers that whole day, so a window ending on
/// a date includes what happened on it.
fn window_end(v: &Value, now: i64) -> Result<i64> {
    match v {
        Value::String(s) if chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").is_ok() => {
            let day = schema::parse_day(s)?;
            Ok(schema::day_micros(day + chrono::Days::new(1)) - 1)
        }
        _ => instant(v, now),
    }
}

/// `group_by`: an attribute, a one-hop dotted path (`owner.name`), or a list of up to three.
fn parse_group_by(v: Option<&Value>) -> Result<Vec<String>> {
    let attrs: Vec<String> = match v {
        None | Some(Value::Null) => vec![],
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().map(String::from))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| QueryError("group_by is a list of attribute names".into()))?,
        _ => {
            return bad(
                "group_by is an attribute, a dotted path through a reference (ref.attr), or a list of them",
            );
        }
    };
    if attrs.len() > 3 {
        return bad("group_by takes at most three attributes");
    }
    if let Some(a) = attrs
        .iter()
        .find(|a| a.trim().is_empty() || a.matches('.').count() > 1)
    {
        return bad(format!(
            "group_by {a:?}: one hop at most: an attribute or a dotted path through one reference (ref.attr)"
        ));
    }
    Ok(attrs)
}

/// `"14d"`, `"6h"`, `"2w"`, `"30m"` as microseconds.
fn duration(v: &Value) -> Result<i64> {
    let s = v.as_str().unwrap_or_default().trim();
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = n
        .parse()
        .map_err(|_| QueryError(format!("{s:?} is not a duration like 14d, 6h, 2w or 30m")))?;
    let us = match unit {
        "m" => 60_000_000,
        "h" => 3_600_000_000,
        "d" => 86_400_000_000,
        "w" => 7 * 86_400_000_000,
        _ => return bad(format!("{s:?} is not a duration like 14d, 6h, 2w or 30m")),
    };
    Ok(n * us)
}

/// Run a query against a pinned session as `who`. Returns the JSON answer.
pub async fn run(store: &Store, session: &Session, who: &Principal, q: &Query) -> Result<Value> {
    let x = Exec {
        store,
        session,
        who,
        schema: session.schema(),
        now: q.now,
        files: Default::default(),
    };
    if q.find != Find::Passages
        && (q.about.is_some() || q.evidence_filter != EvidenceFilter::default())
    {
        return bad("about and evidence apply to find: passages");
    }
    let mut out = match q.find {
        Find::Passages => Box::pin(x.passages(q)).await?,
        Find::Schema => json!({
            "schema": x.schema.as_deref().map(|s| serde_json::to_value(s).unwrap_or(Value::Null)),
        }),
        // Boxed: these futures are large (index scans), and callers may poll them on a small stack.
        Find::Entities => Box::pin(x.entities(q)).await?,
        Find::Facts => Box::pin(x.facts(q)).await?,
        Find::Count => Box::pin(x.count(q)).await?,
        Find::Changes => Box::pin(x.changes(q)).await?,
        Find::Metrics => Box::pin(x.named_metrics(q)).await?,
    };
    if let Some(o) = out.as_object_mut() {
        if o.get("total").and_then(Value::as_u64) == Some(0) && o.get("note").is_none() {
            o.insert("note".into(), json!(NOTHING_VISIBLE));
        }
        o.insert(
            "schema_version".into(),
            json!(
                x.schema
                    .as_ref()
                    .map(|s| format!("{}@{}", s.name, s.version))
            ),
        );
        o.insert(
            "as_of".into(),
            json!(schema::format_instant(x.reference(q.time))),
        );
    }
    Ok(out)
}

/// Parse and run in one go (what the HTTP, MCP and CLI surfaces call).
pub async fn run_json(
    store: &Store,
    session: &Session,
    who: &Principal,
    v: &Value,
) -> Result<Value> {
    let v = expand_metric(v, session.schema().as_deref())?;
    // A cursor carries what its first page read: a later page reads that again, whatever landed since.
    // The first page pins what it reads the same way, so its cursor names exactly that.
    let pin = match cursor_pin(&v)? {
        Some(p) => p,
        None => session.page_pin(store, session.now()).await?,
    };
    let session = session.resume(store, &pin).await?;
    let mut q = Query::parse(&v, pin.at)?;
    q.pin = Some(pin.encode());
    run(store, &session, who, &q).await
}

/// The pin a query's cursor carries (its third part), if it has one; a cursor from before pins had none.
fn cursor_pin(v: &Value) -> Result<Option<crate::PagePin>> {
    let Some(c) = v.get("cursor").and_then(Value::as_str) else {
        return Ok(None);
    };
    match c.splitn(3, '.').nth(2) {
        None => Ok(None),
        Some(p) => match crate::PagePin::decode(p) {
            Some(pin) => Ok(Some(pin)),
            None => bad("cursor is the value a previous page returned"),
        },
    }
}

/// `{"metric": name, ...}`: the schema's stored count, plus the caller's extra `where` clauses
/// and its `time`, `group_by`, `limit` and `include`. Anything else is passed through unchanged.
fn expand_metric(v: &Value, schema: Option<&Schema>) -> Result<Value> {
    let Some(name) = v.get("metric") else {
        return Ok(v.clone());
    };
    let Some(name) = name.as_str() else {
        return bad("metric is a name");
    };
    let known = || {
        schema
            .map(|s| s.metrics.keys().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_default()
    };
    let Some(def) = schema.and_then(|s| s.metrics.get(name)) else {
        return bad(format!("no metric {name:?} (metrics: {})", known()));
    };
    let Some(mut q) = def.query.clone() else {
        // A SQL metric is answered as written: only how much it cites can change.
        let mut q = json!({"find": "metrics", "names": [name]});
        for (k, extra) in v.as_object().into_iter().flatten() {
            match k.as_str() {
                "metric" => {}
                "limit" | "include" | "where" => q[k.as_str()] = extra.clone(),
                other => {
                    return bad(format!(
                        "metric {name} is SQL: it takes where, limit and include, not {other:?}"
                    ));
                }
            }
        }
        return Ok(q);
    };
    for (k, extra) in v.as_object().into_iter().flatten() {
        match k.as_str() {
            "metric" => {}
            "where" => {
                let Some(extra) = extra.as_array() else {
                    return bad("where is a list");
                };
                let base = q
                    .as_object_mut()
                    .unwrap()
                    .entry("where")
                    .or_insert_with(|| json!([]));
                match base.as_array_mut() {
                    Some(b) => b.extend(extra.iter().cloned()),
                    None => return bad(format!("metric {name}: stored where is not a list")),
                }
            }
            "time" | "group_by" | "limit" | "include" => {
                q[k.as_str()] = extra.clone();
            }
            other => {
                return bad(format!(
                    "a metric query takes where, time, group_by, limit and include, not {other:?}"
                ));
            }
        }
    }
    Ok(q)
}

/// One value of an attribute, as grouping and metrics read it.
struct Val {
    entity: String,
    value: String,
    value_ref: Option<String>,
    value_num: Option<f64>,
}

struct Exec<'a> {
    store: &'a Store,
    session: &'a Session,
    who: &'a Principal,
    schema: Option<Arc<Schema>>,
    now: i64,
    /// The facts table's files when this query first asked (`crate::search::files_stamp`).
    files: tokio::sync::OnceCell<u64>,
}

impl Exec<'_> {
    /// The instant relative times count back from.
    fn reference(&self, t: Time) -> i64 {
        match t {
            Time::AsOf(t) | Time::Between(_, t) | Time::During(_, t) => t,
            _ => self.now,
        }
    }

    fn card(&self, f: &Fact) -> Card {
        self.schema.as_ref().map_or(Card::Many, |s| {
            s.card(f.entity_type.as_deref(), &f.predicate)
        })
    }

    /// The value type of `attr` on `ty`, or on whichever types declare it when they agree.
    fn value_type(&self, ty: Option<&str>, attr: &str) -> Option<ValueType> {
        let s = self.schema.as_ref()?;
        if attr == schema::NAME || attr == schema::ALIAS {
            return Some(ValueType::String);
        }
        match ty {
            Some(t) => s.predicate(t, attr).map(|p| p.value),
            None => {
                let kinds: BTreeSet<_> = s
                    .types
                    .values()
                    .filter_map(|d| d.predicates.get(attr).map(|p| p.value))
                    .collect();
                (kinds.len() == 1).then(|| *kinds.iter().next().unwrap())
            }
        }
    }

    fn check_type(&self, ty: Option<&str>) -> Result<()> {
        if let (Some(t), Some(s)) = (ty, &self.schema)
            && !s.types.contains_key(t)
        {
            return bad(format!(
                "schema {} has no type {t:?} (types: {})",
                s.name,
                s.types.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if ty.is_some() && self.schema.is_none() {
            return bad("this scope has no schema, so it has no types; leave out \"type\"");
        }
        Ok(())
    }

    fn check_attr(&self, ty: Option<&str>, attr: &str) -> Result<()> {
        if let (Some(t), Some(s)) = (ty, &self.schema)
            && s.predicate(t, attr).is_none()
            && s.mode == schema::Mode::Strict
        {
            return bad(format!(
                "type {t} has no attribute {attr:?} (attributes: {})",
                s.types[t]
                    .predicates
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(())
    }

    /// Facts matching `filter` that the caller may see under `time` (history read when the time
    /// needs it), with backfilled quotes filled in.
    async fn visible(&self, filter: &Filter, time: Time) -> Result<Vec<Fact>> {
        let history = !matches!(time, Time::Now);
        let rows = crate::search::scan(self.store, self.session.scope(), filter, history).await?;
        let vis = self.session.vis();
        let valid_at = match time {
            Time::Now => Some(i64::MAX),
            Time::AsOf(t) => Some(t),
            Time::All | Time::Between(..) | Time::During(..) => None,
        };
        Ok(rows
            .into_iter()
            .filter(|(c, f)| vis.allows(c, f, self.who, valid_at))
            // as this reader sees it, so `valid_to` includes any closure they can see
            .map(|(_, f)| vis.annotate(f, self.who))
            .filter(|f| match time {
                // overlaps the window
                Time::Between(a, z) => f.valid_from <= z && f.valid_to.is_none_or(|e| e > a),
                // first stated within it
                Time::During(a, z) => a <= f.valid_from && f.valid_from <= z,
                _ => true,
            })
            .collect())
    }

    /// `visible` over `ids` in chunks, each with `base` and `col IN chunk`.
    async fn visible_in(
        &self,
        base: &[Filter],
        col: Col,
        ids: &BTreeSet<String>,
        time: Time,
    ) -> Result<Vec<Fact>> {
        let ids: Vec<&String> = ids.iter().collect();
        let mut out = vec![];
        for chunk in ids.chunks(CHUNK) {
            let mut f = base.to_vec();
            f.push(Filter::In(
                col,
                chunk.iter().map(|s| Lit::Str((*s).clone())).collect(),
            ));
            out.extend(self.visible(&Filter::and(f), time).await?);
        }
        Ok(out)
    }

    /// Under `now` and `as_of`, keep only the current value of each one-valued (entity,
    /// predicate): the newest version this reader can see that began by then, if it is among
    /// `facts` (still valid then). An older version stays superseded after the newer one ends.
    async fn current(&self, facts: Vec<Fact>, time: Time) -> Result<Vec<Fact>> {
        let t = match time {
            Time::Now => i64::MAX,
            Time::AsOf(t) => t,
            _ => return Ok(facts),
        };
        let mut latest: HashMap<(String, String), Fact> = HashMap::new();
        let mut out = vec![];
        for f in facts {
            if self.card(&f) == Card::Many {
                out.push(f);
                continue;
            }
            let key = (f.entity.clone(), f.predicate.clone());
            match latest.get(&key) {
                Some(g) if (g.valid_from, &g.fact_id) >= (f.valid_from, &f.fact_id) => {}
                _ => {
                    latest.insert(key, f);
                }
            }
        }
        if latest.is_empty() {
            return Ok(out);
        }
        let keys: BTreeSet<(String, String)> = latest.keys().cloned().collect();
        let ty = self.one_type(keys.iter().map(|(e, _)| e));
        let newest = crate::search::newest_versions(
            self.store,
            self.session.scope(),
            self.session.vis(),
            self.who,
            &keys,
            t,
            ty.as_deref(),
        )
        .await?;
        out.extend(latest.into_iter().filter_map(|(key, f)| {
            let superseded = newest
                .get(&key)
                .is_some_and(|(from, id)| (*from, id) > (f.valid_from, &f.fact_id));
            (!superseded).then_some(f)
        }));
        Ok(out)
    }

    /// The column and literal a condition compares, by the attribute's value type.
    fn lit(&self, vt: Option<ValueType>, v: &Value, reference: i64) -> Result<(Col, Lit)> {
        let text = || match v {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) => Ok(n.to_string()),
            Value::Bool(b) => Ok(b.to_string()),
            _ => bad(format!("{v} is not a value to compare with")),
        };
        Ok(match vt {
            Some(t) if t.numeric() => {
                let n = match v {
                    Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
                    Value::String(s) => s
                        .trim_start_matches(['$', '€', '£'])
                        .replace(',', "")
                        .trim()
                        .parse()
                        .unwrap_or(f64::NAN),
                    _ => f64::NAN,
                };
                if !n.is_finite() {
                    return bad(format!("{v} is not a number"));
                }
                (Col::ValueNum, Lit::Num(n))
            }
            Some(t) if t.temporal() => (Col::ValueTs, Lit::Int(instant(v, reference)?)),
            Some(ValueType::Ref) => (Col::ValueRef, Lit::Str(text()?)),
            Some(ValueType::Bool) => (
                Col::Value,
                Lit::Str(match v {
                    Value::Bool(b) => b.to_string(),
                    _ => text()?.to_ascii_lowercase(),
                }),
            ),
            _ => (Col::Value, Lit::Str(text()?)),
        })
    }

    /// The fact-level filter for one condition on an attribute of value type `vt`.
    fn cond_filter(
        &self,
        vt: Option<ValueType>,
        c: &Cond,
        reference: i64,
    ) -> Result<Option<Filter>> {
        let cmp = |op: Op, v: &Value| -> Result<Filter> {
            let (col, lit) = self.lit(vt, v, reference)?;
            if col == Col::Value && op != Op::Ne {
                return bad("ranges need a number, money, date or timestamp attribute");
            }
            Ok(Filter::Cmp(col, op, lit))
        };
        let relative = |op: Op, d: i64| -> Result<Filter> {
            if !vt.is_some_and(ValueType::temporal) {
                return bad("older_than and within need a date or timestamp attribute");
            }
            Ok(Filter::Cmp(Col::ValueTs, op, Lit::Int(reference - d)))
        };
        Ok(Some(match c {
            Cond::Eq(v) => {
                let (col, lit) = self.lit(vt, v, reference)?;
                Filter::Eq(col, lit)
            }
            Cond::Ne(v) => cmp(Op::Ne, v)?,
            Cond::In(vs) => {
                let lits = vs
                    .iter()
                    .map(|v| self.lit(vt, v, reference))
                    .collect::<Result<Vec<_>>>()?;
                let col = lits.first().map_or(Col::Value, |(c, _)| *c);
                Filter::In(col, lits.into_iter().map(|(_, l)| l).collect())
            }
            Cond::Gt(v) => cmp(Op::Gt, v)?,
            Cond::Gte(v) => cmp(Op::Ge, v)?,
            Cond::Lt(v) => cmp(Op::Lt, v)?,
            Cond::Lte(v) => cmp(Op::Le, v)?,
            Cond::Between(a, z) => Filter::And(vec![cmp(Op::Ge, a)?, cmp(Op::Le, z)?]),
            Cond::Contains(s) => Filter::Contains(Col::Value, s.clone()),
            Cond::OlderThan(d) => relative(Op::Lt, *d)?,
            Cond::Within(d) => Filter::And(vec![
                relative(Op::Ge, *d)?,
                Filter::Cmp(Col::ValueTs, Op::Le, Lit::Int(reference)),
            ]),
            Cond::RefIn(set) => Filter::In(
                if vt == Some(ValueType::Ref) {
                    Col::ValueRef
                } else {
                    Col::Value
                },
                set.iter().map(|s| Lit::Str(s.clone())).collect(),
            ),
            Cond::Exists | Cond::Missing => return Ok(None),
        }))
    }

    /// The key of the kept view of `what` for this reader and the state this query reads.
    async fn view_key(&self, what: What) -> Result<views::Key> {
        let files = *self
            .files
            .get_or_try_init(|| crate::search::files_stamp(self.store, self.session.scope()))
            .await?;
        Ok(views::Key::new(
            self.session.vis_arc(),
            files,
            self.who.tokens(),
            what,
        ))
    }

    /// The entities of `ty` the caller may see a fact of now (`universe` under now), kept between queries.
    async fn universe_now(&self, ty: &str) -> Result<Arc<BTreeSet<String>>> {
        let key = self.view_key(What::Universe(ty.into())).await?;
        let view = views::get_or_build(key, self.session.vis_arc(), || async {
            let filter = Filter::Eq(Col::EntityType, Lit::Str(ty.into()));
            let set = self
                .visible(&filter, Time::Now)
                .await?
                .into_iter()
                .map(|f| f.entity)
                .collect();
            Ok(View::Universe(Arc::new(set)))
        })
        .await?;
        match view {
            View::Universe(s) => Ok(s),
            _ => unreachable!("a universe key holds a universe"),
        }
    }

    /// Each entity of `ty`'s current values of `pred` as the caller sees them now (what `facts_of` gives
    /// under now: the current one of a one-valued attribute, every value of a many-valued one), kept
    /// between queries. Facts typed `ty`, and untyped ones (written before the scope had a schema) of
    /// entities of `ty`.
    async fn values_now(&self, ty: &str, pred: &str) -> Result<Arc<HashMap<String, Vec<Cur>>>> {
        let key = self.view_key(What::Values(ty.into(), pred.into())).await?;
        let view = views::get_or_build(key, self.session.vis_arc(), || async {
            let filter = Filter::And(vec![
                Filter::Eq(Col::Predicate, Lit::Str(pred.into())),
                crate::search::of_type(ty),
            ]);
            let facts: Vec<Fact> = self
                .visible(&filter, Time::Now)
                .await?
                .into_iter()
                .filter(|f| {
                    f.entity_type.as_deref() == Some(ty)
                        || self.schema.as_ref().and_then(|s| s.entity_type(&f.entity)) == Some(ty)
                })
                .collect();
            let mut by_entity: HashMap<String, Vec<Cur>> = HashMap::new();
            for f in self.current(facts, Time::Now).await? {
                by_entity.entry(f.entity).or_default().push(Cur {
                    typed: f.entity_type.as_deref() == Some(ty),
                    value: f.value,
                    value_ref: f.value_ref,
                    value_num: f.value_num,
                    value_ts: f.value_ts,
                });
            }
            Ok(View::Values(Arc::new(by_entity)))
        })
        .await?;
        match view {
            View::Values(m) => Ok(m),
            _ => unreachable!("a values key holds values"),
        }
    }

    /// `ids`' values of `pred` under `time`, as `facts_of` gives them, for grouping and metrics: under now,
    /// for ids of one type, from the kept view (built if need be).
    async fn values_of(&self, ids: &BTreeSet<String>, pred: &str, time: Time) -> Result<Vec<Val>> {
        if let (Time::Now, Some(ty)) = (time, self.one_type(ids)) {
            let values = self.values_now(&ty, pred).await?;
            let mut out = vec![];
            for id in ids {
                for c in values.get(id).into_iter().flatten() {
                    out.push(Val {
                        entity: id.clone(),
                        value: c.value.clone(),
                        value_ref: c.value_ref.clone(),
                        value_num: c.value_num,
                    });
                }
            }
            return Ok(out);
        }
        Ok(self
            .facts_of(ids, Some(&[pred.to_string()]), time)
            .await?
            .into_iter()
            .map(|f| Val {
                entity: f.entity,
                value: f.value,
                value_ref: f.value_ref,
                value_num: f.value_num,
            })
            .collect())
    }

    /// The names of every entity with an alias the caller may see now, kept between queries.
    async fn names_now(&self) -> Result<Arc<BTreeMap<String, Vec<String>>>> {
        let key = self.view_key(What::Names).await?;
        let view = views::get_or_build(key, self.session.vis_arc(), || async {
            let aliased: BTreeSet<String> = self
                .visible(
                    &Filter::Eq(Col::Predicate, Lit::Str(schema::ALIAS.into())),
                    Time::Now,
                )
                .await?
                .into_iter()
                .map(|f| f.entity)
                .collect();
            let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
            if !aliased.is_empty() {
                for f in self
                    .facts_of(
                        &aliased,
                        Some(&[schema::NAME.to_string(), schema::ALIAS.to_string()]),
                        Time::Now,
                    )
                    .await?
                {
                    names.entry(f.entity).or_default().push(f.value);
                }
            }
            Ok(View::Names(Arc::new(names)))
        })
        .await?;
        match view {
            View::Names(m) => Ok(m),
            _ => unreachable!("a names key holds names"),
        }
    }

    /// Every entity (of `ty`) with any fact the caller may see under `time`.
    async fn universe(&self, ty: Option<&str>, time: Time) -> Result<BTreeSet<String>> {
        if let (Some(t), Time::Now) = (ty, time) {
            return Ok((*self.universe_now(t).await?).clone());
        }
        let filter = match ty {
            Some(t) => Filter::Eq(Col::EntityType, Lit::Str(t.into())),
            None => Filter::And(vec![]),
        };
        Ok(self
            .visible(&filter, time)
            .await?
            .into_iter()
            .map(|f| f.entity)
            .collect())
    }

    /// Entities of `ty` the query's id, match and where clauses select.
    async fn select(&self, q: &Query, depth: usize) -> Result<BTreeSet<String>> {
        if depth > MAX_DEPTH {
            return bad(format!("rel and rel_from nest at most {MAX_DEPTH} deep"));
        }
        let ty = q.ty.as_deref();
        self.check_type(ty)?;
        let mut set: Option<BTreeSet<String>> = None;
        let narrow = |s: BTreeSet<String>, set: &mut Option<BTreeSet<String>>| {
            *set = Some(match set.take() {
                None => s,
                Some(prev) => prev.intersection(&s).cloned().collect(),
            });
        };
        if !q.ids.is_empty() {
            let ids: BTreeSet<String> = q
                .ids
                .iter()
                .filter(|id| {
                    ty.is_none()
                        || self
                            .schema
                            .as_ref()
                            .and_then(|s| s.entity_type(id))
                            .is_some_and(|t| Some(t) == ty)
                })
                .cloned()
                .collect();
            narrow(ids, &mut set);
        }
        if let Some(name) = &q.name {
            let mut f = vec![
                Filter::In(
                    Col::Predicate,
                    vec![
                        Lit::Str(schema::NAME.into()),
                        Lit::Str(schema::ALIAS.into()),
                    ],
                ),
                Filter::Contains(Col::Value, name.clone()),
            ];
            if let Some(t) = ty {
                f.push(Filter::Eq(Col::EntityType, Lit::Str(t.into())));
            }
            let found = self.visible(&Filter::and(f), q.time).await?;
            narrow(found.into_iter().map(|f| f.entity).collect(), &mut set);
        }
        for c in &q.clauses {
            let s = Box::pin(self.clause(c, ty, q.time, depth)).await?;
            narrow(s, &mut set);
        }
        match set {
            Some(s) => Ok(s),
            None if ty.is_some() || q.text.is_some() || self.schema.is_none() => {
                self.universe(ty, q.time).await
            }
            None => bad("say which entities: give a type, an id, match or where"),
        }
    }

    /// Entities of `ty` a clause holds for.
    async fn clause(
        &self,
        c: &Clause,
        ty: Option<&str>,
        time: Time,
        depth: usize,
    ) -> Result<BTreeSet<String>> {
        match c {
            Clause::Any(cs) => {
                let mut out = BTreeSet::new();
                for c in cs {
                    out.extend(Box::pin(self.clause(c, ty, time, depth)).await?);
                }
                Ok(out)
            }
            Clause::Not(c) => {
                let all = self.universe(ty, time).await?;
                let hit = Box::pin(self.clause(c, ty, time, depth)).await?;
                Ok(all.difference(&hit).cloned().collect())
            }
            Clause::Rel { attr, to } => {
                self.check_attr(ty, attr)?;
                let mut to = (**to).clone();
                to.time = time;
                if to.ty.is_none() {
                    to.ty = self.ref_target(ty, attr);
                }
                let targets = Box::pin(self.select(&to, depth + 1)).await?;
                let cond = Cond::RefIn(Arc::new(targets));
                self.attr(ty, attr, std::slice::from_ref(&cond), time).await
            }
            Clause::RelFrom { attr, from } => {
                let mut from = (**from).clone();
                from.time = time;
                let sources = Box::pin(self.select(&from, depth + 1)).await?;
                self.check_attr(from.ty.as_deref(), attr)?;
                let facts = self
                    .visible_in(
                        &[Filter::Eq(Col::Predicate, Lit::Str(attr.clone()))],
                        Col::Entity,
                        &sources,
                        time,
                    )
                    .await?;
                Ok(self
                    .current(facts, time)
                    .await?
                    .into_iter()
                    .map(|f| f.value_ref.unwrap_or(f.value))
                    .filter(|target| {
                        ty.is_none()
                            || self
                                .schema
                                .as_ref()
                                .and_then(|s| s.entity_type(target))
                                .is_some_and(|t| Some(t) == ty)
                    })
                    .collect())
            }
            Clause::Attr { attr, conds } => {
                self.check_attr(ty, attr)?;
                self.attr(ty, attr, conds, time).await
            }
        }
    }

    /// The values each of `ids` groups under for `attr` (current under now/as_of). A dotted path
    /// (`owner.name`) follows the reference one hop and groups by the target's attribute.
    async fn group_values(
        &self,
        q: &Query,
        ids: &BTreeSet<String>,
        attr: &str,
    ) -> Result<HashMap<String, Vec<String>>> {
        let ty = q.ty.as_deref();
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        match attr.split_once('.') {
            None => {
                self.check_attr(ty, &alias_name(attr))?;
                for v in self.values_of(ids, &alias_name(attr), q.time).await? {
                    out.entry(v.entity).or_default().push(v.value);
                }
            }
            Some((head, tail)) => {
                self.check_attr(ty, head)?;
                if ty.is_some_and(|t| {
                    self.value_type(Some(t), head)
                        .is_some_and(|v| v != ValueType::Ref)
                }) {
                    return bad(format!(
                        "group_by {attr}: {head} is not a reference, so it has no {tail}"
                    ));
                }
                if let Some(target) = self.ref_target(ty, head) {
                    self.check_attr(Some(&target), &alias_name(tail))?;
                }
                let mut targets_of: HashMap<String, Vec<String>> = HashMap::new();
                for v in self.values_of(ids, head, q.time).await? {
                    targets_of
                        .entry(v.entity)
                        .or_default()
                        .push(v.value_ref.unwrap_or(v.value));
                }
                let targets: BTreeSet<String> = targets_of.values().flatten().cloned().collect();
                let mut value_of: HashMap<String, Vec<String>> = HashMap::new();
                for v in self.values_of(&targets, &alias_name(tail), q.time).await? {
                    value_of.entry(v.entity).or_default().push(v.value);
                }
                for (entity, targets) in targets_of {
                    let values = out.entry(entity).or_default();
                    for t in targets {
                        values.extend(value_of.get(&t).cloned().unwrap_or_default());
                    }
                }
            }
        }
        for v in out.values_mut() {
            v.sort();
            v.dedup();
        }
        Ok(out)
    }

    /// The single type a ref attribute points at, if the schema says so.
    fn ref_target(&self, ty: Option<&str>, attr: &str) -> Option<String> {
        let s = self.schema.as_ref()?;
        let p = s.predicate(ty?, attr)?;
        (p.to.len() == 1).then(|| p.to[0].clone())
    }

    /// Entities of `ty` whose `attr` satisfies every condition: for a one-valued attribute under
    /// `now`/`as_of`, its current value must; otherwise any of its values.
    async fn attr(
        &self,
        ty: Option<&str>,
        attr: &str,
        conds: &[Cond],
        time: Time,
    ) -> Result<BTreeSet<String>> {
        let vt = self.value_type(ty, attr);
        let reference = self.reference(time);
        if conds.iter().any(|c| matches!(c, Cond::Missing)) {
            let rest: Vec<Cond> = conds
                .iter()
                .filter(|c| !matches!(c, Cond::Missing))
                .cloned()
                .collect();
            let has = Box::pin(self.attr(ty, attr, &[Cond::Exists], time)).await?;
            let all = self.universe(ty, time).await?;
            let missing: BTreeSet<String> = all.difference(&has).cloned().collect();
            if rest.is_empty() {
                return Ok(missing);
            }
            let other = Box::pin(self.attr(ty, attr, &rest, time)).await?;
            return Ok(missing.intersection(&other).cloned().collect());
        }
        // Now, on a type: the kept view of each entity's current values (the current one of a one-valued
        // attribute, any of a many-valued one's), its facts typed `ty`.
        if let (Some(t), Time::Now) = (ty, time) {
            let mut check = vec![];
            let mut refs: Option<&BTreeSet<String>> = None;
            for c in conds {
                match c {
                    // A resolved rel can name thousands of targets: looked up, not compared one by one.
                    Cond::RefIn(set) => refs = Some(set),
                    c => check.extend(self.cond_filter(vt, c, reference)?),
                }
            }
            let check = Filter::and(check);
            let held = |c: &Cur| match refs {
                None => true,
                Some(set) if vt == Some(ValueType::Ref) => {
                    c.value_ref.as_ref().is_some_and(|r| set.contains(r))
                }
                Some(set) => set.contains(&c.value),
            };
            let values = self.values_now(t, attr).await?;
            return Ok(values
                .iter()
                .filter(|(_, cs)| {
                    cs.iter()
                        .any(|c| c.typed && held(c) && check.matches_with(&|col| c.col(col)))
                })
                .map(|(e, _)| e.clone())
                .collect());
        }
        let mut base = vec![Filter::Eq(Col::Predicate, Lit::Str(attr.into()))];
        if let Some(t) = ty {
            base.push(Filter::Eq(Col::EntityType, Lit::Str(t.into())));
        }
        let mut wanted = vec![];
        for c in conds {
            if let Some(f) = self.cond_filter(vt, c, reference)? {
                wanted.push(f);
            }
        }
        // A resolved rel can name thousands of targets: scan them in chunks.
        let refs = conds.iter().find_map(|c| match c {
            Cond::RefIn(s) => Some(s.clone()),
            _ => None,
        });
        let scan_filters: Vec<Filter> = wanted
            .iter()
            .filter(|f| !matches!(f, Filter::In(Col::ValueRef | Col::Value, _)) || refs.is_none())
            .cloned()
            .collect();
        let hits: Vec<Fact> = match &refs {
            Some(set) => {
                let col = if vt == Some(ValueType::Ref) {
                    Col::ValueRef
                } else {
                    Col::Value
                };
                let mut b = base.clone();
                b.extend(scan_filters.iter().cloned());
                self.visible_in(&b, col, set, time).await?
            }
            None => {
                let mut b = base.clone();
                b.extend(scan_filters.iter().cloned());
                self.visible(&Filter::and(b), time).await?
            }
        };
        let candidates: BTreeSet<String> = hits.iter().map(|f| f.entity.clone()).collect();
        let one = self
            .schema
            .as_ref()
            .is_some_and(|s| ty.is_some_and(|t| s.card(Some(t), attr) == Card::One))
            && matches!(time, Time::Now | Time::AsOf(_));
        if !one {
            return Ok(candidates);
        }
        // One-valued: a candidate matched some value, but only its current value counts.
        let all = self
            .visible_in(&base, Col::Entity, &candidates, time)
            .await?;
        let check = Filter::and(wanted);
        Ok(self
            .current(all, time)
            .await?
            .into_iter()
            .filter(|f| check.matches(f))
            .map(|f| f.entity)
            .collect())
    }

    /// The schema type every one of `ids` is of, if they share one.
    fn one_type<'a>(&self, ids: impl IntoIterator<Item = &'a String>) -> Option<String> {
        let schema = self.schema.as_ref()?;
        let mut ty = None;
        for id in ids {
            let t = schema.entity_type(id)?;
            match ty {
                None => ty = Some(t),
                Some(u) if u != t => return None,
                Some(_) => {}
            }
        }
        ty.map(str::to_string)
    }

    /// The facts of `ids` (only `preds` when given), visible and current under `time`.
    async fn facts_of(
        &self,
        ids: &BTreeSet<String>,
        preds: Option<&[String]>,
        time: Time,
    ) -> Result<Vec<Fact>> {
        let mut base = vec![];
        if let Some(p) = preds {
            base.push(Filter::In(
                Col::Predicate,
                p.iter().map(|s| Lit::Str(s.clone())).collect(),
            ));
        }
        // Many ids of one type: one scan of that type's values of `preds`, kept to `ids` here, is
        // much cheaper than a scan per chunk of ids.
        let facts = match self.one_type(ids) {
            Some(ty) if preds.is_some() && ids.len() > CHUNK => {
                base.push(crate::search::of_type(&ty));
                self.visible(&Filter::and(base), time)
                    .await?
                    .into_iter()
                    .filter(|f| ids.contains(&f.entity))
                    .collect()
            }
            _ => self.visible_in(&base, Col::Entity, ids, time).await?,
        };
        self.current(facts, time).await
    }

    /// A fact's value as JSON: a number for numbers and money, a bool for bools, else its text.
    fn json_value(&self, f: &Fact, evidence: bool) -> Value {
        let value = match (f.value_num, f.value_type.as_deref()) {
            (Some(n), _) => json!(n),
            (_, Some("bool")) => json!(f.value == "true"),
            _ => json!(f.value),
        };
        let mut v = json!({"value": value, "valid_from": schema::format_instant(f.valid_from)});
        if let Some(u) = &f.unit {
            v["unit"] = json!(u);
        }
        if let Some(t) = f.valid_to {
            v["valid_to"] = json!(schema::format_instant(t));
        }
        if let Some(b) = &f.time_basis {
            v["time_basis"] = json!(b);
        }
        if evidence {
            v["evidence"] = json!({
                "fact_id": f.fact_id, "source_episodes": f.source_episodes, "quote": f.quote,
            });
            if let Some(c) = crate::cite_json(f) {
                v["evidence"]["cite"] = c;
            }
        }
        v
    }

    /// Read-time freshness marks on `facts` (`crate::freshness`), under `now` or `as_of` and where the scope asks
    /// for them, by fact id. `whole`: the facts are their entities' every current fact; else each entity's plans
    /// and statuses are read beside them, so a status can read overdue.
    async fn marks(
        &self,
        facts: &[Fact],
        time: Time,
        whole: bool,
    ) -> Result<HashMap<String, Mark>> {
        let at = match time {
            Time::Now => self.now,
            Time::AsOf(t) => t,
            _ => return Ok(HashMap::new()),
        };
        let Some(schema) = self.schema.as_deref() else {
            return Ok(HashMap::new());
        };
        let schemas = [schema];
        if facts.is_empty()
            || !freshness::any(&schemas)
            || !freshness::on(self.store, self.session.scope()).await
        {
            return Ok(HashMap::new());
        }
        let mut all: Vec<&Fact> = facts.iter().collect();
        let mut beside = vec![];
        if !whole {
            let (ids, preds) = freshness::beside(&schemas, &all);
            if !ids.is_empty() {
                let preds: Vec<String> = preds.into_iter().collect();
                beside = self.facts_of(&ids, Some(&preds), time).await?;
            }
        }
        let have: BTreeSet<&str> = facts.iter().map(|f| f.fact_id.as_str()).collect();
        all.extend(beside.iter().filter(|f| !have.contains(f.fact_id.as_str())));
        Ok(freshness::marks(&schemas, &all, at))
    }

    /// What another fact says against each of `facts` (`crate::conflicts`), under `now` or `as_of` and where the
    /// scope keeps conflicts, by fact id.
    async fn conflicts(&self, facts: &[Fact], time: Time) -> Result<HashMap<String, Conflict>> {
        let valid_at = match time {
            Time::Now => i64::MAX,
            Time::AsOf(t) => t,
            _ => return Ok(HashMap::new()),
        };
        if facts.is_empty() || !conflicts::on(self.store, self.session.scope()).await {
            return Ok(HashMap::new());
        }
        let facts: Vec<&Fact> = facts.iter().collect();
        conflicts::marks(self.store, self.session, self.who, &facts, Some(valid_at)).await
    }

    /// Entity cards: each entity's (selected) values, one-valued as an object, many as a list.
    async fn cards(&self, ids: &[String], q: &Query) -> Result<Vec<Value>> {
        let set: BTreeSet<String> = ids.iter().cloned().collect();
        // `owner.name`: the owner's own `_name` (`name` is shorthand for `_name`).
        let (plain, dotted): (Vec<String>, Vec<String>) = q
            .select
            .clone()
            .unwrap_or_default()
            .into_iter()
            .partition(|s| !s.contains('.'));
        let mut preds: Option<Vec<String>> = q.select.as_ref().map(|_| {
            let mut p: Vec<String> = plain.iter().map(|s| alias_name(s)).collect();
            p.extend(
                dotted
                    .iter()
                    .map(|d| d.split('.').next().unwrap().to_string()),
            );
            p.push(schema::NAME.into());
            p
        });
        if let Some(p) = &mut preds {
            p.sort();
            p.dedup();
        }
        let facts = self.facts_of(&set, preds.as_deref(), q.time).await?;
        // What has gone stale or overdue, said beside the value rather than served as current.
        let marks = self.marks(&facts, q.time, preds.is_none()).await?;
        let disputed = self.conflicts(&facts, q.time).await?;
        let mut by_entity: BTreeMap<String, Vec<Fact>> = BTreeMap::new();
        for f in facts {
            by_entity.entry(f.entity.clone()).or_default().push(f);
        }
        // Dotted selects: the referenced entities' values.
        let mut hop: HashMap<(String, String), Value> = HashMap::new();
        for d in &dotted {
            let (head, tail) = d.split_once('.').unwrap();
            let tail = alias_name(tail);
            let targets: BTreeSet<String> = by_entity
                .values()
                .flatten()
                .filter(|f| f.predicate == head)
                .filter_map(|f| f.value_ref.clone())
                .collect();
            for f in self
                .facts_of(&targets, Some(std::slice::from_ref(&tail)), q.time)
                .await?
            {
                hop.insert((f.entity.clone(), tail.clone()), json!(f.value));
            }
        }
        let mut cards = vec![];
        for id in ids {
            let facts = by_entity.remove(id).unwrap_or_default();
            let mut values = Map::new();
            let mut name = Value::Null;
            let mut sorted = facts.clone();
            sorted.sort_by_key(|f| std::cmp::Reverse(f.valid_from));
            for f in &sorted {
                if f.predicate == schema::NAME {
                    name = json!(f.value);
                    if q.select
                        .as_ref()
                        .is_none_or(|s| !s.iter().any(|x| x == "name" || x == schema::NAME))
                    {
                        continue;
                    }
                }
                let mut v = self.json_value(f, q.evidence);
                if let Some(m) = marks.get(&f.fact_id) {
                    v["freshness"] = m.to_json(&f.predicate);
                }
                if let Some(c) = disputed.get(&f.fact_id) {
                    v["conflict"] = c.to_json();
                }
                if self.card(f) == Card::Many {
                    values
                        .entry(f.predicate.clone())
                        .or_insert_with(|| json!([]))
                        .as_array_mut()
                        .unwrap()
                        .push(v);
                } else {
                    values.entry(f.predicate.clone()).or_insert(v);
                }
            }
            for d in &dotted {
                let (head, tail) = d.split_once('.').unwrap();
                let targets: Vec<String> = sorted
                    .iter()
                    .filter(|f| f.predicate == head)
                    .filter_map(|f| f.value_ref.clone())
                    .collect();
                let got: Vec<Value> = targets
                    .iter()
                    .filter_map(|t| hop.get(&(t.clone(), alias_name(tail))).cloned())
                    .collect();
                if !got.is_empty() {
                    values.insert(
                        d.clone(),
                        if got.len() == 1 {
                            got.into_iter().next().unwrap()
                        } else {
                            json!(got)
                        },
                    );
                }
            }
            let ty = sorted
                .iter()
                .find_map(|f| f.entity_type.clone())
                .or_else(|| {
                    self.schema
                        .as_ref()
                        .and_then(|s| s.entity_type(id).map(String::from))
                });
            cards.push(json!({"id": id, "type": ty, "name": name, "values": values}));
        }
        Ok(cards)
    }

    /// Order entity ids: by relevance (scores), latest activity, id, or an attribute's value.
    async fn order(
        &self,
        q: &Query,
        ids: BTreeSet<String>,
        scores: Option<&HashMap<String, f32>>,
    ) -> Result<Vec<String>> {
        let mut ids: Vec<String> = ids.into_iter().collect();
        let order = q.order.clone().unwrap_or(if scores.is_some() {
            Order::Relevance
        } else {
            Order::Id
        });
        match order {
            Order::Id => ids.sort(),
            Order::Relevance => {
                let Some(s) = scores else {
                    return bad("order relevance needs text");
                };
                ids.retain(|i| s.contains_key(i));
                ids.sort_by(|a, b| s[b].total_cmp(&s[a]).then(a.cmp(b)));
            }
            Order::Time => {
                let set: BTreeSet<String> = ids.iter().cloned().collect();
                let mut latest: HashMap<String, i64> = HashMap::new();
                for f in self.facts_of(&set, None, q.time).await? {
                    let e = latest.entry(f.entity).or_insert(i64::MIN);
                    *e = (*e).max(f.valid_from);
                }
                ids.sort_by(|a, b| {
                    latest
                        .get(b)
                        .unwrap_or(&i64::MIN)
                        .cmp(latest.get(a).unwrap_or(&i64::MIN))
                        .then(a.cmp(b))
                });
            }
            Order::By { attr, desc } => {
                let attr = alias_name(&attr);
                let set: BTreeSet<String> = ids.iter().cloned().collect();
                let mut key: HashMap<String, Lit> = HashMap::new();
                for f in self
                    .facts_of(&set, Some(std::slice::from_ref(&attr)), q.time)
                    .await?
                {
                    let k = match (f.value_num, f.value_ts) {
                        (Some(n), _) => Lit::Num(n),
                        (_, Some(t)) => Lit::Int(t),
                        _ => Lit::Str(f.value.clone()),
                    };
                    key.insert(f.entity, k);
                }
                // Entities without the attribute sort last either way.
                ids.sort_by(|a, b| match (key.get(a), key.get(b)) {
                    (Some(x), Some(y)) => {
                        let o = x.partial_cmp_lit(y);
                        (if desc { o.reverse() } else { o }).then(a.cmp(b))
                    }
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => a.cmp(b),
                });
            }
        }
        Ok(ids)
    }

    /// Text relevance per entity: the best hit among its facts, restricted to `ids` when given.
    async fn text_scores(
        &self,
        q: &Query,
        ids: Option<&BTreeSet<String>>,
    ) -> Result<HashMap<String, f32>> {
        let text = q.text.as_deref().unwrap_or_default();
        let mut base = vec![];
        if let Some(t) = &q.ty {
            base.push(Filter::Eq(Col::EntityType, Lit::Str(t.clone())));
        }
        let valid_at = match q.time {
            Time::Now => Some(i64::MAX),
            Time::AsOf(t) => Some(t),
            _ => None,
        };
        let k = (q.limit * 4).clamp(20, 200);
        let mut scores = HashMap::new();
        let mut run = |hits: Vec<crate::search::Hit>| {
            for h in hits {
                let s = scores.entry(h.fact.entity.clone()).or_insert(0.0_f32);
                *s = s.max(h.score);
            }
        };
        match ids {
            Some(set) if set.len() <= CHUNK => {
                let mut f = base.clone();
                f.push(Filter::In(
                    Col::Entity,
                    set.iter().map(|s| Lit::Str(s.clone())).collect(),
                ));
                run(self
                    .session
                    .search_filtered(
                        self.store,
                        self.who,
                        text,
                        k,
                        valid_at,
                        &Filter::and(f),
                        q.rank(),
                    )
                    .await?);
            }
            _ => {
                let hits = self
                    .session
                    .search_filtered(
                        self.store,
                        self.who,
                        text,
                        k,
                        valid_at,
                        &Filter::and(base),
                        q.rank(),
                    )
                    .await?;
                run(hits
                    .into_iter()
                    .filter(|h| ids.is_none_or(|s| s.contains(&h.fact.entity)))
                    .collect());
            }
        }
        Ok(scores)
    }

    async fn passages(&self, q: &Query) -> Result<Value> {
        if !q.ids.is_empty() {
            return self.read_passages(q).await;
        }
        let group = match q.group_by.as_slice() {
            [] => None,
            [t] if !t.contains('.') => {
                self.check_type(Some(t))?;
                Some(t.clone())
            }
            _ => {
                return bad("find passages groups by one entity type, not a list or a dotted path");
            }
        };
        let about = match &q.about {
            None => None,
            Some(About::Ids(ids)) => Some(ids.iter().cloned().collect::<BTreeSet<_>>()),
            Some(About::Query(sub)) => {
                let mut sub = (**sub).clone();
                if sub.time == Time::Now {
                    sub.time = q.time;
                }
                Some(Box::pin(self.select(&sub, 1)).await?)
            }
        };
        let about = match about {
            Some(ids) if ids.is_empty() => {
                return Ok(json!({"find": "passages", "total": 0, "results": []}));
            }
            Some(ids) => Some(self.with_parts(ids, q.time).await?),
            None => None,
        };
        let by_time = match &q.order {
            None => q.text.is_none(),
            Some(Order::Time) => true,
            Some(Order::Relevance) => false,
            Some(_) => return bad("passages order by relevance or time"),
        };
        if !by_time && q.text.is_none() {
            return bad("order relevance needs text");
        }
        let variants = match &q.text {
            Some(t) => self.variants(t).await?,
            None => BTreeMap::new(),
        };
        let extra: Vec<String> = variants.values().flatten().cloned().collect();
        let filter = crate::search::passages::PassageFilter {
            about,
            source_kinds: q.evidence_filter.sources.clone(),
            occurred_from: q.evidence_filter.from,
            occurred_to: q.evidence_filter.to,
        };
        let want = q.offset + q.limit;
        // A page spreads over source kinds; a pool for grouping or time order is the best matches.
        let mut rank = q.rank();
        rank.diverse &= group.is_none() && !by_time;
        // Ordered by time with text: the most relevant few, newest first.
        let k = match (&group, by_time && q.text.is_some()) {
            (Some(_), _) => GROUP_POOL.max(want * 4),
            (None, true) => want * 4,
            // one more than the page, to tell whether there is a next one
            (None, false) => want + 1,
        };
        let mut hits = crate::search::passages::search(
            self.store,
            self.session.scope(),
            self.session.vis(),
            self.session.index_pin(),
            self.who,
            q.text.as_deref(),
            &extra,
            &filter,
            k,
            rank,
        )
        .await?;
        if by_time {
            hits.sort_by_key(|h| std::cmp::Reverse(h.passage.occurred_at));
        }
        let terms = crate::search::passages::query_terms(q.text.as_deref().unwrap_or(""), &extra);
        let mut out = match group {
            Some(ty) => self.group_passages(q, &ty, &hits, &terms).await?,
            None => {
                let (page, next) = q.page(hits);
                let titles = self.titles(page.iter().map(|h| &h.passage)).await?;
                let results: Vec<Value> = page
                    .iter()
                    .map(|h| {
                        hit_json(
                            &h.passage,
                            &titles,
                            &terms,
                            crate::search::passages::SNIPPET_BYTES,
                        )
                    })
                    .collect();
                json!({"find": "passages", "total": results.len(), "results": results, "cursor": next})
            }
        };
        if !variants.is_empty() {
            out["variants"] = json!(variants);
        }
        // How fresh the memory behind the answer is: when the newest source the reader may see
        // occurred by the time the search is judged as of.
        if let Some(t) = crate::search::passages::newest_visible(
            self.store,
            self.session.scope(),
            self.session.vis(),
            self.who,
            q.reference_time(),
        )
        .await?
        {
            out["newest_source"] = json!(schema::format_instant(t));
        }
        Ok(out)
    }

    /// Titles of these passages' episodes.
    async fn titles<'p>(
        &self,
        ps: impl Iterator<Item = &'p crate::search::passages::Passage>,
    ) -> Result<HashMap<String, String>> {
        let ids: Vec<String> = ps.map(|p| p.episode_id.clone()).collect();
        crate::search::passages::titles(self.store, self.session.scope(), &ids).await
    }

    /// `find: passages` with `id`: the text of those passages' episodes.
    async fn read_passages(&self, q: &Query) -> Result<Value> {
        if q.text.is_some() || q.about.is_some() || !q.group_by.is_empty() {
            return bad("read passages by id, or search them with text and about, not both");
        }
        if q.ids.len() > MAX_READ {
            return bad(format!("read at most {MAX_READ} passages at a time"));
        }
        let (mut wanted, mut missing) = (vec![], vec![]);
        for id in &q.ids {
            let (episode, chunk) = match id.split_once('#') {
                Some((e, c)) => (
                    e,
                    Some(c.trim().parse::<i32>().map_err(|_| {
                        QueryError(format!("{id:?}: a passage id is <episode>#<chunk number>"))
                    })?),
                ),
                None => (id.as_str(), None),
            };
            match crate::search::passages::resolve(self.session.vis(), self.who, episode)
                .map_err(QueryError)?
            {
                Some(e) => wanted.push((e, chunk)),
                None => missing.push(id.clone()),
            }
        }
        let reads =
            crate::search::passages::read(self.store, self.session.scope(), &wanted).await?;
        let results: Vec<Value> = reads
            .into_iter()
            .map(|r| {
                let mut v = json!({
                    "id": crate::search::passages::short_id(&r.episode_id, r.shown.0 as i32),
                    "source": r.source, "occurred_at": schema::format_instant(r.occurred_at),
                    "about": r.about, "text": r.text,
                });
                if r.shown != (0, r.chunks - 1) {
                    v["chunks"] = json!(r.chunks);
                    v["shown"] = json!(format!(
                        "chunks {} to {} of {}",
                        r.shown.0, r.shown.1, r.chunks
                    ));
                }
                v
            })
            .collect();
        let mut out = json!({"find": "passages", "total": results.len(), "results": results});
        if !missing.is_empty() {
            out["not_found"] = json!(missing);
        }
        Ok(out)
    }

    /// Search hits rolled up into the entities of type `ty` they are about (through `part_of`):
    /// one row per entity with how many hits it has and its best one.
    async fn group_passages(
        &self,
        q: &Query,
        ty: &str,
        hits: &[crate::search::passages::PassageHit],
        terms: &std::collections::HashSet<String>,
    ) -> Result<Value> {
        let about: BTreeSet<String> = hits
            .iter()
            .flat_map(|h| h.passage.about.iter().cloned())
            .collect();
        let up = self.roll_up(&about, ty, q.time).await?;
        // groups in order of their best hit: (entity, hits)
        let mut groups: Vec<(String, Vec<&crate::search::passages::Passage>)> = vec![];
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut ungrouped = 0;
        for h in hits {
            let targets: BTreeSet<&String> = h
                .passage
                .about
                .iter()
                .filter_map(|a| up.get(a))
                .flatten()
                .collect();
            if targets.is_empty() {
                ungrouped += 1;
            }
            for t in targets {
                let i = *index.entry(t.clone()).or_insert_with(|| {
                    groups.push((t.clone(), vec![]));
                    groups.len() - 1
                });
                groups[i].1.push(&h.passage);
            }
        }
        let total = groups.len();
        let (page, next) = q.page(groups);
        let ids: BTreeSet<String> = page.iter().map(|(e, _)| e.clone()).collect();
        let names: HashMap<String, String> = self
            .facts_of(&ids, Some(&[schema::NAME.to_string()]), q.time)
            .await?
            .into_iter()
            .map(|f| (f.entity, f.value))
            .collect();
        let titles = self.titles(page.iter().map(|(_, ps)| ps[0])).await?;
        let results: Vec<Value> = page
            .iter()
            .map(|(e, ps)| {
                let with_terms = ps
                    .iter()
                    .filter(|p| {
                        crate::search::terms(&p.text)
                            .iter()
                            .any(|t| terms.contains(t))
                    })
                    .count();
                let times = ps.iter().map(|p| p.occurred_at);
                // The group says what the hit is about.
                let mut best = hit_json(ps[0], &titles, terms, GROUP_SNIPPET_BYTES);
                best.as_object_mut().map(|o| o.remove("about"));
                let mut v = json!({
                    "id": e, "name": names.get(e), "passages": ps.len(),
                    "first": schema::format_instant(times.clone().min().unwrap_or_default()),
                    "last": schema::format_instant(times.max().unwrap_or_default()),
                    "best": best,
                });
                if !terms.is_empty() {
                    v["with_query_words"] = json!(with_terms);
                }
                v
            })
            .collect();
        Ok(json!({
            "find": "passages", "group_by": ty, "ranked": hits.len(), "ungrouped": ungrouped,
            "total": total, "results": results, "cursor": next,
        }))
    }

    /// `ids` and every entity that is part of one of them (an account's deals and contacts, a
    /// deal's meetings), following `part_of` references up to [`PART_HOPS`] deep.
    async fn with_parts(&self, ids: BTreeSet<String>, time: Time) -> Result<BTreeSet<String>> {
        let part_of = self
            .schema
            .as_ref()
            .map(|s| s.part_of())
            .unwrap_or_default();
        if part_of.is_empty() {
            return Ok(ids);
        }
        let preds: BTreeSet<&String> = part_of.iter().map(|(_, p)| p).collect();
        let base = [Filter::In(
            Col::Predicate,
            preds.iter().map(|p| Lit::Str((*p).clone())).collect(),
        )];
        let mut all = ids.clone();
        let mut frontier = ids;
        for _ in 0..PART_HOPS {
            if frontier.is_empty() {
                break;
            }
            let facts = self
                .visible_in(&base, Col::ValueRef, &frontier, time)
                .await?;
            let next: BTreeSet<String> = self
                .current(facts, time)
                .await?
                .into_iter()
                .filter(|f| {
                    f.entity_type
                        .as_ref()
                        .is_some_and(|t| part_of.contains(&(t.clone(), f.predicate.clone())))
                })
                .map(|f| f.entity)
                .filter(|e| !all.contains(e))
                .collect();
            all.extend(next.iter().cloned());
            frontier = next;
        }
        Ok(all)
    }

    /// For each of `ids`, the entities of type `ty` it is or is part of (within [`PART_HOPS`]).
    async fn roll_up(
        &self,
        ids: &BTreeSet<String>,
        ty: &str,
        time: Time,
    ) -> Result<HashMap<String, BTreeSet<String>>> {
        let Some(s) = self.schema.clone() else {
            return Ok(HashMap::new());
        };
        let part_of = s.part_of();
        let preds: Vec<String> = part_of
            .iter()
            .map(|(_, p)| p.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut out: HashMap<String, BTreeSet<String>> = HashMap::new();
        // a node still to climb from, and the ids it was reached from
        let mut frontier: HashMap<String, BTreeSet<String>> = HashMap::new();
        for id in ids {
            if s.entity_type(id) == Some(ty) {
                out.entry(id.clone()).or_default().insert(id.clone());
            } else {
                frontier.insert(id.clone(), [id.clone()].into());
            }
        }
        for _ in 0..PART_HOPS {
            if frontier.is_empty() || preds.is_empty() {
                break;
            }
            let nodes: BTreeSet<String> = frontier.keys().cloned().collect();
            let mut next: HashMap<String, BTreeSet<String>> = HashMap::new();
            for f in self.facts_of(&nodes, Some(&preds), time).await? {
                if !f
                    .entity_type
                    .as_ref()
                    .is_some_and(|t| part_of.contains(&(t.clone(), f.predicate.clone())))
                {
                    continue;
                }
                let target = f.value_ref.clone().unwrap_or(f.value.clone());
                let origins = frontier.get(&f.entity).cloned().unwrap_or_default();
                if s.entity_type(&target) == Some(ty) {
                    for o in origins {
                        out.entry(o).or_default().insert(target.clone());
                    }
                } else {
                    next.entry(target).or_default().extend(origins);
                }
            }
            frontier = next;
        }
        Ok(out)
    }

    /// Other names of the entities `text` names, keyed by the name it used: keyword search adds
    /// them, so a passage that spells a name another way (an alias) still matches. A name that
    /// only adds words to the one used is left out, since its extra words match too widely.
    async fn variants(&self, text: &str) -> Result<BTreeMap<String, Vec<String>>> {
        let names = self.names_now().await?;
        if names.is_empty() {
            return Ok(BTreeMap::new());
        }
        let words = |s: &str| -> Vec<String> {
            s.split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .map(str::to_lowercase)
                .collect()
        };
        let said = words(text);
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for ns in names.values() {
            let Some(used) = ns.iter().find(|n| {
                let w = words(n);
                !w.is_empty() && said.windows(w.len()).any(|x| x == w.as_slice())
            }) else {
                continue;
            };
            // Other spellings, not longer forms: "Jump AI" for "Jump" would only add "ai".
            let base: BTreeSet<String> = words(used).into_iter().collect();
            let mut others: Vec<String> = ns
                .iter()
                .filter(|n| !base.is_subset(&words(n).into_iter().collect()))
                .cloned()
                .collect();
            others.sort();
            others.dedup();
            if !others.is_empty() {
                out.entry(used.clone()).or_default().extend(others);
            }
        }
        Ok(out)
    }

    async fn entities(&self, q: &Query) -> Result<Value> {
        let constrained = !q.ids.is_empty() || q.name.is_some() || !q.clauses.is_empty();
        let (ids, scores) = match &q.text {
            Some(_) if !constrained => {
                self.check_type(q.ty.as_deref())?;
                let s = self.text_scores(q, None).await?;
                (s.keys().cloned().collect(), Some(s))
            }
            Some(_) => {
                let ids = self.select(q, 0).await?;
                let s = self.text_scores(q, Some(&ids)).await?;
                (ids, Some(s))
            }
            None => (self.select(q, 0).await?, None),
        };
        let total = ids.len();
        let ordered = self.order(q, ids, scores.as_ref()).await?;
        let (ordered, next) = q.page(ordered);
        let mut cards = self.cards(&ordered, q).await?;
        if let Some(s) = &scores {
            for c in &mut cards {
                if let Some(x) = c["id"].as_str().and_then(|id| s.get(id)) {
                    c["score"] = json!(x);
                }
            }
        }
        Ok(json!({"find": "entities", "total": total, "results": cards, "cursor": next}))
    }

    async fn facts(&self, q: &Query) -> Result<Value> {
        let constrained = !q.ids.is_empty() || q.name.is_some() || !q.clauses.is_empty();
        let ids = if constrained {
            Some(self.select(q, 0).await?)
        } else {
            None
        };
        self.check_type(q.ty.as_deref())?;
        let preds: Option<Vec<String>> = q
            .select
            .as_ref()
            .map(|s| s.iter().map(|p| alias_name(p)).collect());
        let mut base = vec![];
        if let Some(t) = &q.ty {
            base.push(Filter::Eq(Col::EntityType, Lit::Str(t.clone())));
        }
        if let Some(p) = &preds {
            base.push(Filter::In(
                Col::Predicate,
                p.iter().map(|s| Lit::Str(s.clone())).collect(),
            ));
        }
        let out: Vec<(Fact, Option<f32>)> = match &q.text {
            Some(text) => {
                let valid_at = match q.time {
                    Time::Now => Some(i64::MAX),
                    Time::AsOf(t) => Some(t),
                    _ => None,
                };
                let mut f = base.clone();
                if let Some(set) = ids.as_ref().filter(|s| s.len() <= CHUNK) {
                    f.push(Filter::In(
                        Col::Entity,
                        set.iter().map(|s| Lit::Str(s.clone())).collect(),
                    ));
                }
                let hits = self
                    .session
                    .search_filtered(
                        self.store,
                        self.who,
                        text,
                        q.limit,
                        valid_at,
                        &Filter::and(f),
                        q.rank(),
                    )
                    .await?;
                // Search leaves out superseded one-valued facts under now/as_of itself.
                hits.into_iter()
                    .filter(|h| ids.as_ref().is_none_or(|s| s.contains(&h.fact.entity)))
                    .map(|h| (h.fact, Some(h.score)))
                    .collect()
            }
            None => {
                let facts = match &ids {
                    Some(set) => self.visible_in(&base, Col::Entity, set, q.time).await?,
                    None => self.visible(&Filter::and(base), q.time).await?,
                };
                let mut facts = self.current(facts, q.time).await?;
                facts.sort_by(|a, b| {
                    b.valid_from
                        .cmp(&a.valid_from)
                        .then(a.fact_id.cmp(&b.fact_id))
                });
                facts.into_iter().map(|f| (f, None)).collect()
            }
        };
        let total = out.len();
        let (out, next) = q.page(out);
        let shown: Vec<Fact> = out.iter().map(|(f, _)| f.clone()).collect();
        let marks = self.marks(&shown, q.time, false).await?;
        let disputed = self.conflicts(&shown, q.time).await?;
        let results: Vec<Value> = out
            .into_iter()
            .map(|(f, score)| {
                let mut v = self.json_value(&f, true);
                v["entity"] = json!(f.entity);
                v["type"] = json!(f.entity_type);
                v["predicate"] = json!(f.predicate);
                if let Some(s) = score {
                    v["score"] = json!(s);
                }
                if let Some(m) = marks.get(&f.fact_id) {
                    v["freshness"] = m.to_json(&f.predicate);
                }
                if let Some(c) = disputed.get(&f.fact_id) {
                    v["conflict"] = c.to_json();
                }
                v
            })
            .collect();
        Ok(json!({"find": "facts", "total": total, "results": results, "cursor": next}))
    }

    async fn count(&self, q: &Query) -> Result<Value> {
        let ids = self.select(q, 0).await?;
        let total = ids.len();
        let numeric: BTreeSet<String> = q
            .metrics
            .iter()
            .filter_map(|m| match m {
                Metric::Sum(a) | Metric::Avg(a) | Metric::Min(a) | Metric::Max(a) => {
                    Some(a.clone())
                }
                Metric::Count => None,
            })
            .collect();
        for a in &numeric {
            if !self
                .value_type(q.ty.as_deref(), a)
                .is_some_and(ValueType::numeric)
            {
                return bad(format!("{a} is not a number or money attribute"));
            }
        }
        let mut nums: HashMap<(String, String), f64> = HashMap::new();
        for a in &numeric {
            for v in self.values_of(&ids, a, q.time).await? {
                if let Some(n) = v.value_num {
                    nums.insert((v.entity, a.clone()), n);
                }
            }
        }
        // Per group attribute: the values each entity groups under.
        let mut group_values = Vec::with_capacity(q.group_by.len());
        for g in &q.group_by {
            group_values.push(Box::pin(self.group_values(q, &ids, g)).await?);
        }
        // An entity with several values (a many-valued attribute) counts in each of its groups;
        // one with none counts under "(none)".
        let none = ["(none)".to_string()];
        let mut buckets: BTreeMap<Vec<&str>, Vec<&String>> = BTreeMap::new();
        for id in &ids {
            let mut keys: Vec<Vec<&str>> = vec![vec![]];
            for values in &group_values {
                let vals = values
                    .get(id)
                    .filter(|v| !v.is_empty())
                    .map_or(&none[..], Vec::as_slice);
                keys = keys
                    .into_iter()
                    .flat_map(|k| {
                        vals.iter().map(move |v| {
                            let mut k = k.clone();
                            k.push(v.as_str());
                            k
                        })
                    })
                    .collect();
            }
            for k in keys {
                match buckets.get_mut(&k) {
                    Some(b) => b.push(id),
                    None => {
                        buckets.insert(k, vec![id]);
                    }
                }
            }
        }
        let stat = |members: &[&String], m: &Metric| -> (String, Value) {
            let vals = |a: &String| -> Vec<f64> {
                members
                    .iter()
                    .filter_map(|e| nums.get(&((*e).clone(), a.clone())).copied())
                    .collect()
            };
            match m {
                Metric::Count => ("count".into(), json!(members.len())),
                Metric::Sum(a) => (format!("sum_{a}"), json!(vals(a).iter().sum::<f64>())),
                Metric::Avg(a) => {
                    let v = vals(a);
                    (
                        format!("avg_{a}"),
                        if v.is_empty() {
                            Value::Null
                        } else {
                            json!(v.iter().sum::<f64>() / v.len() as f64)
                        },
                    )
                }
                Metric::Min(a) => (
                    format!("min_{a}"),
                    vals(a)
                        .into_iter()
                        .reduce(f64::min)
                        .map_or(Value::Null, |x| json!(x)),
                ),
                Metric::Max(a) => (
                    format!("max_{a}"),
                    vals(a)
                        .into_iter()
                        .reduce(f64::max)
                        .map_or(Value::Null, |x| json!(x)),
                ),
            }
        };
        let metrics: Vec<Metric> = if q.metrics.is_empty() {
            vec![Metric::Count]
        } else {
            q.metrics.clone()
        };
        let mut groups: Vec<Value> = buckets
            .iter()
            .map(|(k, members)| {
                let mut g = Map::new();
                for (attr, value) in q.group_by.iter().zip(k) {
                    g.insert(attr.clone(), json!(value));
                }
                g.insert("count".into(), json!(members.len()));
                for m in &metrics {
                    let (name, v) = stat(members, m);
                    g.insert(name, v);
                }
                Value::Object(g)
            })
            .collect();
        if q.evidence {
            let cited = self.count_citations(q, &buckets, &numeric).await?;
            for (g, c) in groups.iter_mut().zip(cited) {
                g["members"] = c;
            }
        }
        groups.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
        Ok(json!({"find": "count", "total": total, "groups": groups}))
    }

    /// Per group (in `buckets`' order), the first `limit` entities it counted, each with the facts that
    /// put it there: its `where` and `group_by` attributes and the numbers summed, each with its source
    /// episodes and quote. A `group_by` path (`owner.name`) cites the reference and the target's value.
    async fn count_citations(
        &self,
        q: &Query,
        buckets: &BTreeMap<Vec<&str>, Vec<&String>>,
        numeric: &BTreeSet<String>,
    ) -> Result<Vec<Value>> {
        let mut preds: BTreeSet<String> = numeric.clone();
        for c in &q.clauses {
            if let Clause::Attr { attr, .. } | Clause::Rel { attr, .. } = c {
                preds.insert(attr.clone());
            }
        }
        // (head on the counted entity, tail on its target) per group_by; tail None when not a path.
        let paths: Vec<(String, Option<String>)> = q
            .group_by
            .iter()
            .map(|g| match g.split_once('.') {
                None => (alias_name(g), None),
                Some((h, t)) => (h.to_string(), Some(alias_name(t))),
            })
            .collect();
        preds.extend(paths.iter().map(|(h, _)| h.clone()));
        let cited: BTreeSet<String> = buckets
            .values()
            .flat_map(|m| m.iter().take(q.limit).map(|e| (*e).clone()))
            .collect();
        let preds: Vec<String> = preds.into_iter().collect();
        let mut facts_of: HashMap<String, Vec<Fact>> = HashMap::new();
        for f in self.facts_of(&cited, Some(&preds), q.time).await? {
            facts_of.entry(f.entity.clone()).or_default().push(f);
        }
        let tails: BTreeSet<String> = paths.iter().filter_map(|(_, t)| t.clone()).collect();
        let mut target_facts: HashMap<String, Vec<Fact>> = HashMap::new();
        if !tails.is_empty() {
            let heads: BTreeSet<&str> = paths
                .iter()
                .filter(|(_, t)| t.is_some())
                .map(|(h, _)| h.as_str())
                .collect();
            let targets: BTreeSet<String> = facts_of
                .values()
                .flatten()
                .filter(|f| heads.contains(f.predicate.as_str()))
                .map(|f| f.value_ref.clone().unwrap_or_else(|| f.value.clone()))
                .collect();
            let tails: Vec<String> = tails.into_iter().collect();
            for f in self.facts_of(&targets, Some(&tails), q.time).await? {
                target_facts.entry(f.entity.clone()).or_default().push(f);
            }
        }
        let cite = |f: &Fact| {
            let mut v = self.json_value(f, true);
            v["entity"] = json!(f.entity);
            v["predicate"] = json!(f.predicate);
            v
        };
        Ok(buckets
            .iter()
            .map(|(key, members)| {
                let out: Vec<Value> = members
                    .iter()
                    .take(q.limit)
                    .map(|e| {
                        let mut facts = vec![];
                        for f in facts_of.get(*e).into_iter().flatten() {
                            // A many-valued group_by attribute: only the value this group is for.
                            let other_group = paths.iter().zip(key).any(|((h, t), k)| {
                                t.is_none() && *h == f.predicate && f.value != *k
                            });
                            if other_group {
                                continue;
                            }
                            facts.push(cite(f));
                            let target = f.value_ref.as_ref().unwrap_or(&f.value);
                            for ((h, t), k) in paths.iter().zip(key) {
                                let Some(t) = t else { continue };
                                if *h != f.predicate {
                                    continue;
                                }
                                for tf in target_facts.get(target).into_iter().flatten() {
                                    if tf.predicate == *t && tf.value == *k {
                                        facts.push(cite(tf));
                                    }
                                }
                            }
                        }
                        json!({"id": e, "facts": facts})
                    })
                    .collect();
                json!(out)
            })
            .collect())
    }

    /// The schema's named metrics with one of `q.tags` (all when none), each answered as a count under now
    /// (with citations when `q.evidence`, `q.limit` members a group), and which metrics each tag holds.
    /// Each answer is kept for the reader past new writes, until `q.refresh` asks for it anew or memory
    /// loses something it may have read (`Vis::removals`): a page of metrics reads in a moment while the
    /// facts keep coming.
    async fn named_metrics(&self, q: &Query) -> Result<Value> {
        let Some(schema) = self.schema.clone() else {
            return bad("this memory has no schema, so no named metrics");
        };
        let mut metrics = vec![];
        let mut by_tag: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let (seq, files) = self.store.fact_files_at(self.session.scope()).await?;
        let facts_generation =
            views::facts_generation(&self.session.scope().namespace(), seq, files);
        for (name, def) in &schema.metrics {
            if !q.tags.is_empty() && !def.tags.iter().any(|t| q.tags.contains(t)) {
                continue;
            }
            if !q.names.is_empty() && !q.names.contains(name) {
                continue;
            }
            for t in &def.tags {
                by_tag.entry(t).or_default().push(name);
            }
            let mut call = json!({"metric": name, "limit": q.limit});
            if q.evidence {
                call["include"] = json!(["evidence"]);
            }
            if !q.filters.is_empty() {
                call["where"] = json!(q.filters);
            }
            // The definition too: an edited metric is answered anew, though the facts it reads didn't change.
            let keyed = format!("{call} {}", serde_json::to_string(def)?);
            let key = views::Key::lasting(
                self.session.scope().namespace(),
                self.session.vis().removals,
                facts_generation,
                self.who.tokens(),
                keyed,
            );
            if q.refresh {
                views::forget(&key);
            }
            let view = views::get_or_build(key, self.session.vis_arc(), || async {
                let mut answer = match &def.sql {
                    Some(sql) => Box::pin(self.sql_metric(sql, &def.types, &q.filters, q)).await?,
                    None => {
                        let mq = Query::parse(&expand_metric(&call, Some(&schema))?, self.now)?;
                        Box::pin(self.count(&mq)).await?
                    }
                };
                answer["computed_at"] = json!(schema::format_instant(self.now));
                Ok(View::Answer(Arc::new(answer)))
            })
            .await?;
            let View::Answer(answer) = view else {
                unreachable!("a metric key holds an answer")
            };
            metrics.push(json!({
                "name": name,
                "title": if def.title.is_empty() { name } else { &def.title },
                "description": def.description,
                "tags": def.tags,
                "answer": *answer,
            }));
        }
        Ok(json!({"find": "metrics", "total": metrics.len(), "tags": by_tag, "metrics": metrics}))
    }

    /// A SQL metric's answer: its rows over the caller's visible facts of `types` (`sql::run`), each row's
    /// `fact_ids` turned into citations (up to `q.limit`, with `q.evidence`) from those same facts only.
    /// `filters` (`where` clauses) narrow its tables: each holds for the types that have its attribute, and
    /// those types' entities it doesn't hold for leave the tables, every fact of them.
    async fn sql_metric(
        &self,
        sql: &str,
        types: &[String],
        filters: &[Value],
        q: &Query,
    ) -> Result<Value> {
        let schema = self.schema.clone();
        let ty_of = |f: &Fact| -> Option<String> {
            f.entity_type.clone().or_else(|| {
                schema
                    .as_deref()
                    .and_then(|s| s.entity_type(&f.entity))
                    .map(String::from)
            })
        };
        let wanted = |f: &Fact| ty_of(f).is_some_and(|t| types.contains(&t));
        let mut filter = types
            .iter()
            .map(|t| Filter::Eq(Col::EntityType, Lit::Str(t.clone())))
            .collect::<Vec<_>>();
        // Facts written before the scope had a schema carry no type: their ids say it.
        filter.push(Filter::Null(Col::EntityType));
        let filter = Filter::Or(filter);
        let kept = self.filtered(types, filters).await?;
        let wanted = |f: &Fact| {
            wanted(f)
                && ty_of(f).is_none_or(|t| kept.get(&t).is_none_or(|set| set.contains(&f.entity)))
        };
        // Every visible version only for a metric that reads them: most read `current` alone.
        let mut history = match sql::reads_facts(sql) {
            true => self.visible(&filter, Time::All).await?,
            false => vec![],
        };
        history.retain(wanted);
        let mut now = self.visible(&filter, Time::Now).await?;
        now.retain(wanted);
        let current = self.current(now, Time::Now).await?;
        // A query that fails (too long, too big) is answered with why, and that answer is kept like any
        // other: asking again before the facts change doesn't run it again.
        let (columns, rows) = match sql::run(sql, &history, &current, &ty_of).await {
            Ok(r) => r,
            Err(e) => return Ok(json!({"find": "sql", "error": format!("{e:#}")})),
        };
        let by_id: HashMap<&str, &Fact> = history
            .iter()
            .chain(&current)
            .map(|f| (f.fact_id.as_str(), f))
            .collect();
        let mut out = Vec::with_capacity(rows.len());
        for mut row in rows {
            let cited = sql::take_cites(&mut row)?;
            let mut row = Value::Object(row);
            if q.evidence {
                let facts: Vec<Value> = cited
                    .iter()
                    .filter_map(|id| by_id.get(id.as_str()))
                    .take(q.limit)
                    .map(|f| {
                        let mut v = self.json_value(f, true);
                        v["entity"] = json!(f.entity);
                        v["predicate"] = json!(f.predicate);
                        v
                    })
                    .collect();
                row["cites"] = json!(facts);
            }
            out.push(row);
        }
        let columns: Vec<String> = columns.into_iter().filter(|c| c != sql::CITES).collect();
        Ok(json!({"find": "sql", "total": out.len(), "columns": columns, "rows": out}))
    }

    /// Per type of `types` a filter applies to, the entities of it every such filter holds for, as the
    /// caller sees them now. A filter is an `attr` or `rel` clause; one no type has the attribute of is a
    /// mistake.
    async fn filtered(
        &self,
        types: &[String],
        filters: &[Value],
    ) -> Result<HashMap<String, BTreeSet<String>>> {
        let mut out = HashMap::new();
        if filters.is_empty() {
            return Ok(out);
        }
        let schema = self.schema.as_deref();
        let attr = |c: &Value| -> Result<String> {
            match c.get("attr").or(c.get("rel")).and_then(Value::as_str) {
                Some(a) => Ok(alias_name(a.split('.').next().unwrap_or(a))),
                None => bad("a metric filter is an attr or rel clause"),
            }
        };
        let has = |t: &str, a: &str| schema.is_some_and(|s| s.predicate(t, a).is_some());
        for c in filters {
            let a = attr(c)?;
            if !types.iter().any(|t| has(t, &a)) {
                return bad(format!(
                    "filter on {a}: none of the metric's types ({}) has it",
                    types.join(", ")
                ));
            }
        }
        for t in types {
            let mut own = vec![];
            for c in filters {
                if has(t, &attr(c)?) {
                    own.push(c.clone());
                }
            }
            if own.is_empty() {
                continue;
            }
            let sel = Query::parse(&json!({"type": t, "where": own}), self.now)?;
            out.insert(t.clone(), Box::pin(self.select(&sel, 0)).await?);
        }
        Ok(out)
    }

    async fn changes(&self, q: &Query) -> Result<Value> {
        let (from, to) = match q.time {
            Time::Between(from, to) | Time::During(from, to) => (from, to),
            Time::All => (i64::MIN, i64::MAX),
            Time::AsOf(t) => (i64::MIN, t),
            Time::Now => {
                return bad(
                    "find changes needs a window: time {\"between\": [from, to]}, or \"all\" for the whole history",
                );
            }
        };
        // Select entities as of the window's end, then read their whole history.
        let mut sel = q.clone();
        sel.time = Time::All;
        let ids = self.select(&sel, 0).await?;
        let preds: Option<Vec<String>> = q
            .select
            .as_ref()
            .map(|s| s.iter().map(|p| alias_name(p)).collect());
        let mut base = vec![];
        if let Some(p) = &preds {
            base.push(Filter::In(
                Col::Predicate,
                p.iter().map(|s| Lit::Str(s.clone())).collect(),
            ));
        }
        let facts = self.visible_in(&base, Col::Entity, &ids, Time::All).await?;
        let mut by_key: BTreeMap<(String, String), Vec<Fact>> = BTreeMap::new();
        for f in facts {
            by_key
                .entry((f.entity.clone(), f.predicate.clone()))
                .or_default()
                .push(f);
        }
        let mut out: Vec<(i64, Value)> = vec![];
        for ((entity, predicate), mut vs) in by_key {
            vs.sort_by(|a, b| {
                a.valid_from
                    .cmp(&b.valid_from)
                    .then(a.fact_id.cmp(&b.fact_id))
            });
            let one = self.card(&vs[0]) == Card::One;
            for (i, f) in vs.iter().enumerate() {
                if f.valid_from >= from && f.valid_from <= to {
                    let mut v = self.json_value(f, q.evidence);
                    v["entity"] = json!(entity);
                    v["predicate"] = json!(predicate);
                    v["change"] = json!(if one && i > 0 { "changed" } else { "added" });
                    if one && i > 0 {
                        v["previous"] = self.json_value(&vs[i - 1], false)["value"].clone();
                    }
                    out.push((f.valid_from, v));
                }
                if let Some(end) = f.valid_to
                    && end >= from
                    && end <= to
                    && !(one && vs.get(i + 1).is_some_and(|n| n.valid_from == end))
                {
                    let mut v = self.json_value(f, q.evidence);
                    v["entity"] = json!(entity);
                    v["predicate"] = json!(predicate);
                    v["change"] = json!("ended");
                    out.push((end, v));
                }
            }
        }
        out.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        let total = out.len();
        let (out, next) = q.page(out);
        Ok(
            json!({"find": "changes", "total": total, "cursor": next, "results": out.into_iter().map(|(_, v)| v).collect::<Vec<_>>()}),
        )
    }
}

/// A passage search hit: its id, where and when it's from, what it's about, its episode's
/// title and the part of it that best matches the query.
fn hit_json(
    p: &crate::search::passages::Passage,
    titles: &HashMap<String, String>,
    terms: &std::collections::HashSet<String>,
    snippet_bytes: usize,
) -> Value {
    let mut v = json!({
        "id": crate::search::passages::short_id(&p.episode_id, p.chunk),
        "source": p.source, "occurred_at": schema::format_instant(p.occurred_at),
        "about": p.about,
    });
    if let Some(t) = titles.get(&p.episode_id).filter(|t| !t.is_empty()) {
        v["title"] = json!(t);
    }
    v["snippet"] = json!(crate::search::passages::snippet(
        &p.text,
        terms,
        snippet_bytes
    ));
    v
}

/// `name` is shorthand for the built-in `_name`.
fn alias_name(s: &str) -> String {
    if s == "name" {
        schema::NAME.into()
    } else {
        s.to_string()
    }
}

trait LitOrd {
    fn partial_cmp_lit(&self, other: &Lit) -> std::cmp::Ordering;
}

impl LitOrd for Lit {
    fn partial_cmp_lit(&self, other: &Lit) -> std::cmp::Ordering {
        use std::cmp::Ordering::Equal;
        match (self, other) {
            (Lit::Str(a), Lit::Str(b)) => a.cmp(b),
            (Lit::Num(a), Lit::Num(b)) => a.partial_cmp(b).unwrap_or(Equal),
            (Lit::Int(a), Lit::Int(b)) => a.cmp(b),
            _ => Equal,
        }
    }
}
