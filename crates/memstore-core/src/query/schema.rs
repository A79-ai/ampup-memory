//! Pluggable schemas: the entity types a scope holds, their predicates, and each predicate's value
//! type and cardinality. A schema is data (JSON), not code: it is committed into the scope like any
//! write (`Store::set_schema`), so one engine serves scopes with different schemas, a branch can
//! try a new schema without touching main, and nothing in the engine knows about any one domain.
//! `schemas/` ships three: `sales`, `engineering` (the kinds the warehouse extraction emits) and
//! `corpus` (the synthetic test corpus). A scope without a schema stays untyped, as before.
//!
//! A typed fact's entity names its type: `deal:42` (type `deal`), an id starting with a type's
//! `id_prefix` followed by a digit (`p000123` with prefix `p`), or the schema's `default_type`.
//! On write, each fact's value is checked against its predicate and its typed columns are filled
//! (`value_num`, `value_ts`, `value_ref`), which is what filters and ranges query.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::{Episode, Fact};

/// Every type has these: a display name and other names it goes by (`match` searches both).
pub const NAME: &str = "_name";
pub const ALIAS: &str = "_alias";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    pub name: String,
    #[serde(default = "first_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// `strict` (default): a fact's predicate must be declared for its entity's type.
    /// `open`: undeclared predicates are stored untyped (no value checks, many-valued).
    #[serde(default)]
    pub mode: Mode,
    /// The type of an entity id that names none (no `<type>:` prefix, no matching `id_prefix`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_type: Option<String>,
    pub types: BTreeMap<String, TypeDef>,
    /// Named counts agents call by name (`{"metric": "pricing_objections_by_stage"}`) instead of
    /// composing `group_by` themselves: the questions that matter most, written once.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metrics: BTreeMap<String, MetricDef>,
    /// Advice for agents querying a scope with this schema, shown with its summary: how this
    /// domain's questions map onto its types (the engine itself knows no domain).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guidance: Vec<String>,
    /// What the engine extracts from the scope's episodes with this schema (goal-oriented extraction): which
    /// episodes, and at most how many facts each. Each is read with the schema and its episode type's catalog
    /// offered. None: nothing is extracted with the schema (episodes are read goal-blind where that is on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<Extract>,
}

/// A schema's `extract` section: which of the scope's episodes are read with the schema, and how much each may
/// write. Changing it is never a breaking change: it says what is read, not how facts are stored.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Extract {
    /// Source prefixes, entity types the episodes are about, and when they occurred; each check passes when
    /// unset, so `{}` is every episode.
    #[serde(default)]
    pub sources: Sources,
    /// Facts each episode may write at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts_per_episode: Option<u32>,
    /// How what it extracts is summarised per subject (GM4): the entity type summarised (`deal`), which
    /// predicates feed which elements of a methodology, what statuses items take. Status is derived there, never
    /// extracted; the brief (`get_brief`) reads the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregates: Option<Aggregates>,
    /// How facts are written: absent, as the episode type's catalog's typed statements and items; `statements`,
    /// as one self-contained sentence per fact under one subject, its kind as the predicate, the same subject
    /// types for every source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Whether the prompt asks for what the text reports happened: dated results, decisions and changes (a
    /// personal assistant's memory needs the record), never the status or progress of what is still pending.
    /// Off, the prompt asks for no outcomes, as a deal's memory wants, whose status `aggregates` derives; a model
    /// may still write some where the schema's own predicates ask for them. `true` asks for every episode, source
    /// prefixes (`["slack:"]`) for those episodes alone: a scope that holds calls about deals and threads about
    /// its own work records what the threads say happened, and leaves the deals to the aggregation.
    #[serde(default, skip_serializing_if = "Outcomes::is_off")]
    pub outcomes: Outcomes,
    /// Who writes the episodes, when one person writes them all (a personal memory's notes): the id their facts
    /// are about, and the names the text may call them by. Facts the model files under an id made from one of those
    /// names (`person:alex` for `"names": ["Alex Moreno"]`) are filed under `id`, so the writer is one subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer: Option<Writer>,
    /// A cap once set on how many subjects not yet in memory one episode may name:
    /// read so a schema version that set it still parses, never applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_subjects_per_episode: Option<u32>,
}

/// `extract.writer`: the id the episodes' writer is known by (`person:me`), and the names they go by.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Writer {
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
}

impl Writer {
    /// `id` when `other` is an id of the writer's type made from one of their names: a name's words joined with
    /// `_`, or its first word (`person:alex`, `person:alex_moreno`), in any case. None for any other id, and for
    /// the writer's own.
    pub fn of(&self, other: &str) -> Option<&str> {
        let (ty, _) = self.id.split_once(':')?;
        let (t, name) = other.trim().split_once(':')?;
        if t != ty || other.trim() == self.id {
            return None;
        }
        let slug = |s: &str| -> Vec<String> {
            s.split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .map(str::to_lowercase)
                .collect()
        };
        let name = slug(name).join("_");
        self.names
            .iter()
            .map(|n| slug(n))
            .filter(|w| !w.is_empty())
            .any(|w| name == w.join("_") || name == w[0])
            .then_some(self.id.as_str())
    }
}

/// `extract.outcomes`: every episode (`true`), none (`false`, the default), or the episodes whose source starts
/// with one of the prefixes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Outcomes {
    All(bool),
    Sources(Vec<String>),
}

impl Default for Outcomes {
    fn default() -> Self {
        Outcomes::All(false)
    }
}

impl Outcomes {
    pub fn is_off(&self) -> bool {
        match self {
            Outcomes::All(on) => !on,
            Outcomes::Sources(prefixes) => prefixes.is_empty(),
        }
    }

    /// Whether an episode from `source` is asked for outcomes.
    pub fn asked_for(&self, source: &str) -> bool {
        match self {
            Outcomes::All(on) => *on,
            Outcomes::Sources(prefixes) => prefixes.iter().any(|p| source.starts_with(p.as_str())),
        }
    }
}

#[cfg(test)]
mod outcomes_tests {
    use super::Outcomes;

    #[test]
    fn a_flag_or_source_prefixes() {
        let parse = |j: &str| serde_json::from_str::<Outcomes>(j).unwrap();
        assert!(parse("true").asked_for("ampup:meeting:1"));
        assert!(!parse("false").asked_for("slack:acme:thread:C1:2"));
        let threads = parse(r#"["slack:"]"#);
        assert!(threads.asked_for("slack:acme:thread:C1:2"));
        assert!(!threads.asked_for("ampup:meeting:1"));
        assert!(!threads.is_off() && parse("[]").is_off() && Outcomes::default().is_off());
    }

    /// A schema version that set the since-dropped subject cap still parses.
    #[test]
    fn a_schema_that_set_the_old_subject_cap_still_parses() {
        let e: super::Extract =
            serde_json::from_str(r#"{"facts_per_episode": 40, "new_subjects_per_episode": 3}"#)
                .unwrap();
        assert_eq!(e.facts_per_episode, Some(40));
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MetricDef {
    /// What a page shows over the number ("Open pipeline by stage"); the name when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The sections a page groups its metrics under (`pipeline`, `objections`), lower_snake_case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// A `find: count` query. A caller may add `where` clauses and set `time`, `group_by` and
    /// `limit`; the rest stays as written. A metric has this or `sql`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<serde_json::Value>,
    /// A SQL `SELECT` over the reader's visible facts of `types` (see `crate::query::sql`): tables
    /// `facts` (their whole history) and `current` (what holds now). A `fact_ids` column (a list of fact
    /// ids) cites each row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    /// The entity types `sql` reads: only their facts are loaded into its tables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
}

fn first_version() -> u32 {
    1
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Strict,
    Open,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TypeDef {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Ids such as `p000123`: this prefix followed by a digit names this type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_prefix: Option<String>,
    #[serde(default)]
    pub predicates: BTreeMap<String, PredicateDef>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PredicateDef {
    #[serde(rename = "type")]
    pub value: ValueType,
    /// `one` (default): a newer value supersedes the older one. `many`: values accumulate.
    #[serde(default)]
    pub card: Card,
    /// `enum`: the allowed values. Absent means any value (an open enum, such as an org's stages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
    /// `ref`: the entity types a value may name. Empty means any type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
    /// `number` / `money`: the unit values are in, such as `USD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// `ref`: the entity is part of the one it names (a meeting of its deal, a deal of its
    /// account), so passages about the part count as about the whole, and passages roll up
    /// along it when grouped.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub part_of: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Days a value holds without being stated again. A read past that, where the scope marks freshness
    /// (`crate::freshness`), marks it stale ("as of <date>, not confirmed since") instead of serving it as
    /// current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness_days: Option<u32>,
    /// `date` / `timestamp`: the date something is planned for (a due date, a close date). Once it has passed
    /// while nothing says its entity is settled (a `pending` status, or no status), reads mark it, and that
    /// status, overdue.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan: bool,
    /// `enum` / `string`, a status: the values that mean its entity is still pending (`open`). One of them
    /// after the entity's plan date has passed reads overdue; any other value says it is settled, and a settled
    /// value never reads stale.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<String>,
    /// `many`: the values are items of one list, all held at once (the tools a customer uses, its product lines,
    /// the steps of how it works). A newer one joins the others and never replaces one, so the dream
    /// (`memstore::dream`) never ends one; an item dropped is said by a fact of its own. It changes no read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub list: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum ValueType {
    /// A short label: compared exactly.
    String,
    /// Free text: searched, compared with `contains`.
    Text,
    Enum,
    Number,
    Money,
    /// A calendar day, `YYYY-MM-DD`.
    Date,
    /// An instant, RFC 3339.
    Timestamp,
    Bool,
    /// Another entity's id.
    Ref,
}

impl ValueType {
    pub fn as_str(self) -> &'static str {
        match self {
            ValueType::String => "string",
            ValueType::Text => "text",
            ValueType::Enum => "enum",
            ValueType::Number => "number",
            ValueType::Money => "money",
            ValueType::Date => "date",
            ValueType::Timestamp => "timestamp",
            ValueType::Bool => "bool",
            ValueType::Ref => "ref",
        }
    }

    /// Ordered by number (`value_num`).
    pub fn numeric(self) -> bool {
        matches!(self, ValueType::Number | ValueType::Money)
    }

    /// Ordered by time (`value_ts`).
    pub fn temporal(self) -> bool {
        matches!(self, ValueType::Date | ValueType::Timestamp)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Card {
    #[default]
    One,
    Many,
}

/// The schemas in `schemas/`, built in so `memstore schema set org sales` needs no file.
pub fn builtin(name: &str) -> Option<&'static str> {
    match name {
        "sales" => Some(include_str!("../../../../schemas/sales.json")),
        "engineering" => Some(include_str!("../../../../schemas/engineering.json")),
        "corpus" => Some(include_str!("../../../../schemas/corpus.json")),
        _ => None,
    }
}

pub const BUILTIN: &[&str] = &["sales", "engineering", "corpus"];

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !s.starts_with('_')
}

impl Schema {
    /// Parse and check a schema.
    pub fn parse(json: &str) -> Result<Schema> {
        let s: Schema =
            serde_json::from_str(json).map_err(|e| anyhow::anyhow!("parse schema: {e}"))?;
        s.check()?;
        Ok(s)
    }

    /// A built-in schema by name, or a schema file by path.
    pub fn load(name_or_path: &str) -> Result<Schema> {
        match builtin(name_or_path) {
            Some(json) => Schema::parse(json),
            None => {
                let json = std::fs::read_to_string(name_or_path).with_context(|| {
                    format!(
                        "{name_or_path} is neither a built-in schema ({}) nor a readable file",
                        BUILTIN.join(", ")
                    )
                })?;
                Schema::parse(&json)
            }
        }
    }

    /// Names are identifiers, references name declared types, prefixes are unambiguous.
    pub fn check(&self) -> Result<()> {
        ensure!(!self.name.trim().is_empty(), "schema needs a name");
        if let Some(x) = &self.extract {
            for t in [&x.sources.from, &x.sources.to].into_iter().flatten() {
                ensure!(
                    parse_time(t).is_some(),
                    "schema {}: extract.sources time {t:?} is not RFC 3339 or YYYY-MM-DD",
                    self.name
                );
            }
            ensure!(
                x.facts_per_episode != Some(0),
                "schema {}: extract.facts_per_episode must be at least 1",
                self.name
            );
            if let Some(a) = &x.aggregates {
                a.check(&format!("schema {} extract", self.name))?;
            }
            if let Some(w) = &x.writer {
                ensure!(
                    w.id.split_once(':')
                        .is_some_and(|(t, n)| self.types.contains_key(t) && !n.trim().is_empty()),
                    "schema {}: extract.writer.id {:?} must be <type>:<name> of a declared type",
                    self.name,
                    w.id
                );
            }
        }
        ensure!(
            !self.types.is_empty(),
            "schema {} declares no types",
            self.name
        );
        if let Some(d) = &self.default_type {
            ensure!(
                self.types.contains_key(d),
                "default_type {d} is not a declared type"
            );
        }
        let mut prefixes: Vec<(&str, &str)> = vec![];
        for (ty, def) in &self.types {
            ensure!(
                is_ident(ty),
                "type name {ty:?} must be lower_snake_case and not start with _"
            );
            if let Some(p) = &def.id_prefix {
                ensure!(
                    !p.is_empty() && !p.ends_with(|c: char| c.is_ascii_digit()),
                    "id_prefix of {ty} must be non-empty and not end in a digit"
                );
                if let Some((other, _)) = prefixes.iter().find(|(_, q)| q == p) {
                    bail!("types {other} and {ty} share id_prefix {p:?}");
                }
                prefixes.push((ty, p));
            }
            for (p, pd) in &def.predicates {
                ensure!(
                    is_ident(p),
                    "predicate {ty}.{p}: name must be lower_snake_case and not start with _"
                );
                match pd.value {
                    ValueType::Ref => {
                        for t in &pd.to {
                            ensure!(
                                self.types.contains_key(t),
                                "predicate {ty}.{p} refers to undeclared type {t}"
                            );
                        }
                    }
                    _ => ensure!(
                        pd.to.is_empty(),
                        "predicate {ty}.{p}: `to` is only for ref values"
                    ),
                }
                if let Some(vs) = &pd.values {
                    ensure!(
                        pd.value == ValueType::Enum,
                        "predicate {ty}.{p}: `values` is only for enum values"
                    );
                    ensure!(!vs.is_empty(), "predicate {ty}.{p}: empty `values`");
                }
                if pd.unit.is_some() {
                    ensure!(
                        pd.value.numeric(),
                        "predicate {ty}.{p}: `unit` is only for number and money values"
                    );
                }
                ensure!(
                    !pd.part_of || pd.value == ValueType::Ref,
                    "predicate {ty}.{p}: `part_of` is only for ref values"
                );
                ensure!(
                    pd.freshness_days != Some(0),
                    "predicate {ty}.{p}: `freshness_days` must be at least 1"
                );
                ensure!(
                    !pd.plan || pd.value.temporal(),
                    "predicate {ty}.{p}: `plan` is only for date and timestamp values"
                );
                ensure!(
                    !pd.list || pd.card == Card::Many,
                    "predicate {ty}.{p}: `list` is only for card many (a list holds several values)"
                );
                if !pd.pending.is_empty() {
                    ensure!(
                        matches!(pd.value, ValueType::Enum | ValueType::String),
                        "predicate {ty}.{p}: `pending` is only for enum and string values"
                    );
                    if let Some(vs) = &pd.values {
                        let unknown: Vec<&String> =
                            pd.pending.iter().filter(|v| !vs.contains(v)).collect();
                        ensure!(
                            unknown.is_empty(),
                            "predicate {ty}.{p}: `pending` names values it doesn't take: {unknown:?}"
                        );
                    }
                }
            }
        }
        for (m, def) in &self.metrics {
            ensure!(
                is_ident(m),
                "metric name {m:?} must be lower_snake_case and not start with _"
            );
            for t in &def.tags {
                ensure!(
                    is_ident(t),
                    "metric {m}: tag {t:?} must be lower_snake_case and not start with _"
                );
            }
            match (&def.query, &def.sql) {
                (Some(_), None) => ensure!(
                    def.types.is_empty(),
                    "metric {m}: types is for a sql metric (a query names its type)"
                ),
                (None, Some(sql)) => {
                    ensure!(
                        !def.types.is_empty(),
                        "metric {m}: sql needs the types it reads"
                    );
                    for t in &def.types {
                        ensure!(
                            self.types.contains_key(t),
                            "metric {m}: type {t:?} is not one this schema declares"
                        );
                    }
                    super::sql::check(sql).map_err(|e| anyhow::anyhow!("metric {m}: {e:#}"))?;
                    continue;
                }
                _ => anyhow::bail!("metric {m}: give it a query or sql, not both"),
            }
            let q = def.query.as_ref().and_then(|q| q.as_object());
            ensure!(
                q.and_then(|q| q.get("find")).and_then(|f| f.as_str()) == Some("count"),
                "metric {m}: query must be an object with \"find\": \"count\""
            );
            let ty = q.and_then(|q| q.get("type")).and_then(|t| t.as_str());
            ensure!(
                ty.is_some_and(|t| self.types.contains_key(t)),
                "metric {m}: query needs a \"type\" this schema declares"
            );
            // The attributes it names must exist, or the metric fails the first time it's called.
            let ty = ty.unwrap();
            let q = q.unwrap();
            let mut attrs: Vec<&str> = vec![];
            if let Some(g) = q.get("group_by").and_then(|g| g.as_str()) {
                attrs.push(g.split('.').next().unwrap());
            }
            for c in q
                .get("where")
                .and_then(|w| w.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(a) = c.get("attr").or(c.get("rel")).and_then(|a| a.as_str()) {
                    attrs.push(a);
                }
            }
            for a in attrs {
                let a = if a == "name" { NAME } else { a };
                ensure!(
                    self.mode == Mode::Open || self.predicate(ty, a).is_some(),
                    "metric {m}: {ty} has no attribute {a:?}"
                );
            }
        }
        Ok(())
    }

    /// The type an entity id names, if any.
    pub fn entity_type(&self, entity: &str) -> Option<&str> {
        if let Some((ty, rest)) = entity.split_once(':')
            && !rest.is_empty()
            && let Some((name, _)) = self.types.get_key_value(ty)
        {
            return Some(name);
        }
        let by_prefix = self
            .types
            .iter()
            .filter_map(|(ty, d)| Some((ty, d.id_prefix.as_deref()?)))
            .filter(|(_, p)| {
                entity.len() > p.len()
                    && entity.starts_with(p)
                    && entity.as_bytes()[p.len()].is_ascii_digit()
            })
            .max_by_key(|(_, p)| p.len());
        if let Some((ty, _)) = by_prefix {
            return Some(ty);
        }
        self.default_type.as_deref()
    }

    /// The definition of `predicate` on `ty`, including the built-in `_name` and `_alias`.
    pub fn predicate(&self, ty: &str, predicate: &str) -> Option<PredicateDef> {
        let plain = |card| PredicateDef {
            value: ValueType::String,
            card,
            values: None,
            to: vec![],
            unit: None,
            part_of: false,
            description: String::new(),
            freshness_days: None,
            plan: false,
            pending: vec![],
            list: false,
        };
        match predicate {
            NAME => Some(plain(Card::One)),
            ALIAS => Some(plain(Card::Many)),
            p => self.types.get(ty)?.predicates.get(p).cloned(),
        }
    }

    /// The (type, predicate) pairs that make an entity part of another.
    pub fn part_of(&self) -> BTreeSet<(String, String)> {
        self.types
            .iter()
            .flat_map(|(ty, d)| {
                d.predicates
                    .iter()
                    .filter(|(_, p)| p.part_of)
                    .map(move |(p, _)| (ty.clone(), p.clone()))
            })
            .collect()
    }

    /// Each type that others are part of, with those parts; the one with the most parts first.
    pub fn wholes(&self) -> Vec<(String, Vec<String>)> {
        let mut parts: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (ty, p) in self.part_of() {
            for whole in &self.types[&ty].predicates[&p].to {
                parts.entry(whole.clone()).or_default().insert(ty.clone());
            }
        }
        let mut out: Vec<(String, Vec<String>)> = parts
            .into_iter()
            .map(|(w, ps)| (w, ps.into_iter().collect()))
            .collect();
        out.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// A query in this schema's own terms, to show agents the syntax: the first type with a
    /// closed enum (preferring one that also has a number), filtered on both and selecting them
    /// plus a reference's name. A type without one asks for its first attribute. None when no
    /// type has an attribute.
    pub fn example_query(&self) -> Option<serde_json::Value> {
        let (ty, d) = self
            .types
            .iter()
            .find(|(_, d)| closed(d).is_some() && number(d).is_some())
            .or_else(|| self.types.iter().find(|(_, d)| closed(d).is_some()))
            .or_else(|| self.types.iter().find(|(_, d)| !d.predicates.is_empty()))?;
        let mut clauses = vec![];
        let mut select = vec![];
        match closed(d) {
            Some((p, vs)) => {
                clauses.push(
                    serde_json::json!({"attr": p, "in": vs.iter().take(2).collect::<Vec<_>>()}),
                );
                select.push(p.clone());
                if let Some(n) = number(d) {
                    clauses.push(serde_json::json!({"attr": n, "gte": 1000}));
                    select.push(n.clone());
                }
            }
            None => {
                let p = d.predicates.keys().next()?;
                clauses.push(serde_json::json!({"attr": p, "exists": true}));
                select.push(p.clone());
            }
        }
        if let Some(r) = one_ref(d) {
            select.push(format!("{r}.name"));
        }
        Some(serde_json::json!({"type": ty, "where": clauses, "select": select}))
    }

    /// A dotted path through a one-valued reference (`<attribute>.name`), if any type has one.
    pub fn reference_path(&self) -> Option<String> {
        self.types
            .values()
            .find_map(one_ref)
            .map(|r| format!("{r}.name"))
    }

    /// Cardinality of a predicate; anything undeclared accumulates (never superseded).
    pub fn card(&self, ty: Option<&str>, predicate: &str) -> Card {
        ty.and_then(|t| self.predicate(t, predicate))
            .map_or(Card::Many, |p| p.card)
    }

    /// Check a fact against the schema and fill its typed columns. Values are canonicalised
    /// (`2026-10-12`, `50000`, `true`) so equal values compare equal.
    pub fn normalize(&self, f: &mut Fact) -> Result<()> {
        let ty = self
            .entity_type(&f.entity)
            .with_context(|| {
                format!(
                    "entity {:?} names no type of schema {} (use <type>:<id>, one of: {})",
                    f.entity,
                    self.name,
                    self.types.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            })?
            .to_string();
        f.entity_type = Some(ty.clone());
        let Some(pd) = self.predicate(&ty, &f.predicate) else {
            ensure!(
                self.mode == Mode::Open,
                "{ty} has no predicate {:?} in schema {} (declared: {})",
                f.predicate,
                self.name,
                self.types[&ty]
                    .predicates
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Ok(());
        };
        let what = || format!("{ty}.{} = {:?}", f.predicate, f.value);
        f.value_type = Some(pd.value.as_str().into());
        match pd.value {
            ValueType::String | ValueType::Text => {
                ensure!(!f.value.trim().is_empty(), "{}: empty value", what())
            }
            ValueType::Enum => {
                if let Some(vs) = &pd.values {
                    ensure!(
                        vs.contains(&f.value),
                        "{}: not one of {}",
                        what(),
                        vs.join(", ")
                    );
                }
            }
            ValueType::Number | ValueType::Money => {
                let n = parse_number(&f.value, pd.unit.as_deref())
                    .with_context(|| format!("{}: not a number", what()))?;
                f.value = format_number(n);
                f.value_num = Some(n);
                f.unit = pd.unit.clone();
            }
            ValueType::Date => {
                let d = parse_day(&f.value).with_context(|| format!("{}: not a date", what()))?;
                f.value = d.format("%Y-%m-%d").to_string();
                f.value_ts = Some(day_micros(d));
            }
            ValueType::Timestamp => {
                let t = parse_instant(&f.value)
                    .with_context(|| format!("{}: not a timestamp", what()))?;
                f.value = format_instant(t);
                f.value_ts = Some(t);
            }
            ValueType::Bool => {
                let b = match f.value.trim().to_ascii_lowercase().as_str() {
                    "true" | "yes" | "1" => true,
                    "false" | "no" | "0" => false,
                    _ => bail!("{}: not true or false", what()),
                };
                f.value = b.to_string();
            }
            ValueType::Ref => {
                let target = self
                    .entity_type(&f.value)
                    .with_context(|| format!("{}: names no entity of a declared type", what()))?;
                ensure!(
                    pd.to.is_empty() || pd.to.iter().any(|t| t == target),
                    "{}: is a {target}, expected {}",
                    what(),
                    pd.to.join(" or ")
                );
                f.value_ref = Some(f.value.clone());
            }
        }
        Ok(())
    }

    /// What moving from `old` to this schema would break for facts already written. Facts keep
    /// the typed columns and the type they were written with, so a change that reads them
    /// differently (a new value type, cardinality, unit or id rule, a narrower enum or ref, a
    /// removed type or predicate) silently changes answers. Additive changes (new types and
    /// predicates, more enum values, an enum opened, a ref widened, descriptions, a `list` mark, strict to
    /// open) return nothing.
    pub fn breaking_changes(&self, old: &Schema) -> Vec<String> {
        let mut out = vec![];
        if self.name != old.name {
            out.push(format!("schema {} replaced by {}", old.name, self.name));
        }
        if self.mode == Mode::Strict && old.mode == Mode::Open {
            out.push("mode open -> strict".into());
        }
        if self.default_type != old.default_type {
            out.push(format!(
                "default_type {:?} -> {:?}",
                old.default_type, self.default_type
            ));
        }
        for (ty, od) in &old.types {
            let Some(nd) = self.types.get(ty) else {
                out.push(format!("type {ty} removed"));
                continue;
            };
            if nd.id_prefix != od.id_prefix {
                out.push(format!(
                    "{ty}.id_prefix {:?} -> {:?}",
                    od.id_prefix, nd.id_prefix
                ));
            }
            for (p, op) in &od.predicates {
                let Some(np) = nd.predicates.get(p) else {
                    out.push(format!("{ty}.{p} removed"));
                    continue;
                };
                if np.value != op.value {
                    out.push(format!(
                        "{ty}.{p} type {} -> {}",
                        op.value.as_str(),
                        np.value.as_str()
                    ));
                }
                if np.card != op.card {
                    out.push(format!("{ty}.{p} card {:?} -> {:?}", op.card, np.card));
                }
                if np.unit != op.unit {
                    out.push(format!("{ty}.{p} unit {:?} -> {:?}", op.unit, np.unit));
                }
                match (&op.values, &np.values) {
                    (None, Some(_)) => out.push(format!("{ty}.{p} enum closed")),
                    (Some(ov), Some(nv)) => {
                        let gone: Vec<&String> = ov.iter().filter(|v| !nv.contains(v)).collect();
                        if !gone.is_empty() {
                            out.push(format!("{ty}.{p} enum values removed: {gone:?}"));
                        }
                    }
                    _ => {}
                }
                let narrowed = match (op.to.is_empty(), np.to.is_empty()) {
                    (_, true) => false,
                    (true, false) => true,
                    (false, false) => op.to.iter().any(|t| !np.to.contains(t)),
                };
                if narrowed {
                    out.push(format!("{ty}.{p} ref to {:?} -> {:?}", op.to, np.to));
                }
            }
        }
        out
    }

    /// A compact listing for agents: one line per type with its predicates and their types.
    pub fn summary(&self) -> String {
        let mut out = format!("schema {} v{}", self.name, self.version);
        if !self.description.is_empty() {
            out.push_str(&format!(": {}", self.description));
        }
        for (ty, def) in &self.types {
            let preds: Vec<String> = def
                .predicates
                .iter()
                .map(|(p, d)| {
                    let mut t = d.value.as_str().to_string();
                    if !d.to.is_empty() {
                        t = format!("ref {}", d.to.join("|"));
                    }
                    if let Some(vs) = &d.values {
                        t = format!("enum {}", vs.join("|"));
                    }
                    if d.card == Card::Many {
                        t.push_str(", many");
                    }
                    if d.part_of {
                        t.push_str(", part of");
                    }
                    format!("{p} ({t})")
                })
                .collect();
            out.push_str(&format!("\n- {ty}: {}", preds.join("; ")));
        }
        for (m, def) in &self.metrics {
            out.push_str(&format!("\n- metric {m}: {}", def.description));
            if !def.tags.is_empty() {
                out.push_str(&format!(" [{}]", def.tags.join(", ")));
            }
        }
        for g in &self.guidance {
            out.push_str(&format!("\n{g}"));
        }
        out
    }
}

/// A type's closed enum with the fewest values, and those values.
fn closed(d: &TypeDef) -> Option<(&String, &Vec<String>)> {
    d.predicates
        .iter()
        .filter_map(|(p, pd)| Some((p, pd.values.as_ref().filter(|v| !v.is_empty())?)))
        .min_by_key(|(_, vs)| vs.len())
}

/// The first number or money attribute a type declares.
fn number(d: &TypeDef) -> Option<&String> {
    d.predicates
        .iter()
        .find(|(_, pd)| pd.value.numeric())
        .map(|(p, _)| p)
}

/// The first one-valued reference a type declares.
fn one_ref(d: &TypeDef) -> Option<&String> {
    d.predicates
        .iter()
        .find(|(_, pd)| pd.value == ValueType::Ref && pd.card == Card::One)
        .map(|(p, _)| p)
}

/// A number, optionally with thousands separators, a leading currency symbol, or the unit after it.
fn parse_number(s: &str, unit: Option<&str>) -> Result<f64> {
    let mut t = s.trim();
    if let Some(u) = unit {
        t = t.strip_suffix(u).map_or(t, str::trim_end);
    }
    let t = t.trim_start_matches(['$', '€', '£']).replace(',', "");
    let n: f64 = t.trim().parse()?;
    ensure!(n.is_finite(), "not finite");
    Ok(n)
}

/// `add` laid over `base`, additively: new types, and new predicates on existing types. Where both declare a
/// predicate, `base`'s stands: what is laid over a scope's schema adds to it, never changes or retypes what it
/// declares. A merge that doesn't check, or would still break `base`, is refused.
pub fn overlay(base: Option<&Schema>, add: &Schema) -> Result<Schema> {
    let Some(base) = base else {
        return Ok(add.clone());
    };
    let mut merged = base.clone();
    for (ty, def) in &add.types {
        match merged.types.get_mut(ty) {
            None => {
                merged.types.insert(ty.clone(), def.clone());
            }
            Some(have) => {
                for (p, pd) in &def.predicates {
                    have.predicates
                        .entry(p.clone())
                        .or_insert_with(|| pd.clone());
                }
            }
        }
    }
    merged.check()?;
    let breaks = merged.breaking_changes(base);
    if !breaks.is_empty() {
        bail!(
            "{} would change schema {}: {}",
            add.name,
            base.name,
            breaks.join("; ")
        );
    }
    Ok(merged)
}

pub fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// `YYYY-MM-DD`, or the day of an RFC 3339 instant.
pub fn parse_day(s: &str) -> Result<chrono::NaiveDate> {
    let s = s.trim();
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d);
    }
    Ok(chrono::DateTime::parse_from_rfc3339(s)?.date_naive())
}

/// Microseconds at the start of a day, UTC.
pub fn day_micros(d: chrono::NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_micros()
}

/// An RFC 3339 instant, or a day (its start, UTC), as microseconds since the epoch.
pub fn parse_instant(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_micros());
    }
    // A date and time with no zone is read as UTC.
    if let Some(t) = parse_naive(s) {
        return Ok(t.and_utc().timestamp_micros());
    }
    Ok(day_micros(parse_day(s)?))
}

/// A date and time with no zone (`2026-10-21T09:30:00`, `2026-10-21 09:30`).
pub fn parse_naive(s: &str) -> Option<chrono::NaiveDateTime> {
    let s = s.trim();
    [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ]
    .into_iter()
    .find_map(|f| chrono::NaiveDateTime::parse_from_str(s, f).ok())
}

/// Whether a timestamp written as `value` drops the clock time its `quote` states: a bare date
/// (`2026-10-21`) would be stored as midnight UTC for "9:30 am this Wednesday".
pub fn drops_stated_time(value: &str, quote: &str) -> bool {
    let v = value.trim();
    chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").is_ok() && states_clock_time(quote)
}

/// Whether `text` gives a time of day: `9:30`, `15:00`, `3pm`, `9 a.m.`, `3pmEST`, `noon`, `midnight`.
pub fn states_clock_time(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if ["noon", "midnight", "o'clock"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return true;
    }
    // Byte offsets agree: lowering ASCII changes no length.
    let (b, raw) = (lower.as_bytes(), text.as_bytes());
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    let mut i = 0;
    while i < b.len() {
        if !digit(i) || (i > 0 && digit(i - 1)) {
            i += 1;
            continue;
        }
        let start = i;
        while digit(i) {
            i += 1;
        }
        if i - start > 2 {
            continue;
        }
        // H:MM or HH:MM.
        if b.get(i) == Some(&b':') && digit(i + 1) && digit(i + 2) && !digit(i + 3) {
            return true;
        }
        // H am / Hpm / H a.m., not "3 amazing" (a lower-case letter after it); "3pmEST" names a zone.
        let mut j = i;
        while b.get(j) == Some(&b' ') {
            j += 1;
        }
        for m in ["a.m", "p.m", "am", "pm"] {
            if lower[j..].starts_with(m)
                && !raw.get(j + m.len()).is_some_and(u8::is_ascii_lowercase)
            {
                return true;
            }
        }
    }
    false
}

pub fn format_instant(us: i64) -> String {
    chrono::DateTime::from_timestamp_micros(us)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
        .unwrap_or_else(|| us.to_string())
}

/// Which episodes a schema's `extract` section reads (a backfill reads them again).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sources {
    /// Source prefixes (`github:`, `slack:`, `meeting:`); none is every source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefixes: Vec<String>,
    /// Entity types an episode must be about; none is any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub about: Vec<String>,
    /// When the episodes occurred: RFC 3339 times or dates, either end open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

impl Sources {
    pub fn is_empty(&self) -> bool {
        self == &Sources::default()
    }

    /// Whether this episode is one of the sources: its source starts with one of the prefixes, it is about an
    /// entity of one of the types, and it occurred in the window (each check passes when unset). The engine's
    /// own documents never are.
    pub fn matches(&self, ep: &Episode) -> bool {
        let micros = |t: &Option<String>| {
            t.as_deref()
                .and_then(parse_time)
                .map(|t| t.timestamp_micros())
        };
        // The engine's own documents (goals once, compiled skills, learned advice) are about the memory, not sources.
        !crate::episode_types::engine_source(&ep.source)
            && !ep
                .document
                .as_deref()
                .is_some_and(|d| d.starts_with("goals/"))
            && (self.prefixes.is_empty()
                || self
                    .prefixes
                    .iter()
                    .any(|p| ep.source.starts_with(p.as_str())))
            && (self.about.is_empty()
                || ep.about.iter().any(|a| {
                    a.split_once(':')
                        .is_some_and(|(t, _)| self.about.iter().any(|x| x == t))
                }))
            && micros(&self.from).is_none_or(|t| ep.occurred_at >= t)
            && micros(&self.to).is_none_or(|t| ep.occurred_at <= t)
    }
}

/// How what a schema's `extract` section extracts is summarised, per subject, in batches (GM4, `crate::aggregate`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Aggregates {
    /// The entity type summarised per subject (`deal`): a batch is every fact of every episode about one.
    pub subject: String,
    /// Scored elements (`meddpicc.metrics`) and the predicates that feed each.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub elements: BTreeMap<String, Vec<String>>,
    /// Scale an element is scored on; default `none | identified | partial | confirmed`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scale: Vec<String>,
    /// Item types and the statuses each takes (`commitment`: `open`, `done`, `dropped`); the first is where
    /// an item stays while nothing shows otherwise.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub items: BTreeMap<String, Vec<String>>,
    /// Days after which a fact of a predicate is stale (shown, flagged, never presented as current).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub freshness_days: BTreeMap<String, u32>,
    /// Date predicates that are plans (`next_meeting`, `due`): flagged once their date has passed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plans: Vec<String>,
    /// Rules the summariser follows, in plain words.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    /// New episodes about a subject before it is summarised again (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<u32>,
    /// Longest a subject's new episodes wait for a batch, in minutes (default 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wait_minutes: Option<u32>,
}

impl Aggregates {
    /// A subject type, statuses for every item type, predicates for every element, `every` at least 1. `owner`
    /// names what holds it in an error (`schema deals extract`).
    pub fn check(&self, owner: &str) -> Result<()> {
        ensure!(
            !self.subject.trim().is_empty() && !self.subject.contains(':'),
            "{owner}: aggregates.subject names an entity type, such as deal"
        );
        for (item, statuses) in &self.items {
            ensure!(
                !statuses.is_empty(),
                "{owner}: aggregates.items.{item} lists no statuses"
            );
        }
        for (el, preds) in &self.elements {
            ensure!(
                !el.trim().is_empty() && !preds.is_empty(),
                "{owner}: aggregates.elements.{el:?} needs predicates"
            );
        }
        ensure!(
            self.every.is_none_or(|n| n > 0),
            "{owner}: aggregates.every must be at least 1"
        );
        Ok(())
    }

    /// The element scale, or the default one.
    pub fn scale(&self) -> Vec<String> {
        if self.scale.is_empty() {
            ["none", "identified", "partial", "confirmed"]
                .map(String::from)
                .to_vec()
        } else {
            self.scale.clone()
        }
    }
}

/// An RFC 3339 time, or a date (its start, UTC).
pub fn parse_time(t: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(t)
        .map(|d| d.with_timezone(&chrono::Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|d| d.and_utc())
        })
}
