//! Read-time freshness: what a read shouldn't serve as plainly current. The scope's schema says, per predicate,
//! how long a value holds without being stated again (`freshness_days`), which dates are plans (`plan`: a due
//! date, a close date) and which values of a status mean its entity is still pending (`pending`: `open`). A
//! schema's `extract.aggregates` `freshness_days` and `plans` count too, for every type that has the predicate.
//!
//! Given an entity's current facts and the time a read is as of, [`marks`] says which of them are:
//!
//! - **later**: restated afterwards with other figures: a newer current fact of the same entity says the same thing
//!   in the same words with different numbers ("later on <date>: <its value>"). It says what replaced a value and
//!   when, which neither the dates beside each value nor a `stale` mark do. A word match, not an understanding;
//! - **stale** (only where [`KINDS_ENV`] names it): not stated again within the predicate's `freshness_days` ("as
//!   of <date>, not confirmed since"); a status with `pending` values ages only while pending. Stated again means by
//!   a source independent of the ones that stated it ([`Stated`]): the same source or an agent saying it again,
//!   or an echo of it, confirms nothing;
//! - **overdue**: a plan date that has passed while nothing says its entity is settled (no status, or a status
//!   still at a `pending` value), and that pending status. The note says what is known, not what to conclude:
//!   "passed; nothing since says it happened or was dropped". A settled status (`done`, `won`) marks nothing.
//!
//! A fact both later and stale is later. Nothing is written, no model is asked and no job runs: the marks come from
//! the facts' own times and words (and the restatements reconcile recorded, [`crate::corroboration`]) on every read,
//! so a value stated again is fresh again at once, and history
//! (`as_of`, `changes`) reads as it did. Nothing is dropped and nothing ranks differently either: a mark is a note on
//! what is served. Reads mark facts only where the scope asks ([`on`]), so the two can be compared.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Value, json};

use crate::Fact;
use crate::query::schema::{Schema, ValueType, parse_time};

const DAY_US: i64 = 86_400_000_000;

/// The environment variable that turns marks on for every scope that doesn't set `freshness` itself.
pub const ENV: &str = "AMPUP_MEMORY_FRESHNESS";

/// Which kinds of mark reads give where marks are on (`AMPUP_MEMORY_FRESHNESS_MARKS=later,overdue,stale`): `later` and
/// `overdue` unless it says. `stale` is asked for by name: on LongMemEval it
/// marked most facts a reader sees, said nothing their dates don't, and cost answers.
pub const KINDS_ENV: &str = "AMPUP_MEMORY_FRESHNESS_MARKS";

/// How much of a newer value a `later` mark quotes.
const LATER_CHARS: usize = 160;

/// What a read says about a current fact instead of serving it as plainly current.
#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    /// Restated at `since` by `fact` with other figures: `value` is what it says now.
    Later {
        since: i64,
        fact: String,
        value: String,
    },
    /// Last stated at `since`, longer ago than its predicate's `freshness_days`.
    Stale { since: i64 },
    /// Its entity's plan `plan` (a predicate such as `due`) passed at `since` while nothing says the entity is
    /// settled: the plan fact itself, or a status still pending.
    Overdue { since: i64, plan: String },
}

impl Mark {
    pub fn kind(&self) -> &'static str {
        match self {
            Mark::Later { .. } => "later",
            Mark::Stale { .. } => "stale",
            Mark::Overdue { .. } => "overdue",
        }
    }

    /// What a reader is told after a value of `predicate`: `later on 2026-05-30: "two free nights"`,
    /// `stale: as of 2026-04-02, not confirmed since`, `passed; nothing since says it happened or was dropped` (the
    /// plan itself), `its due date, 2026-04-15, passed; nothing since says it happened or was dropped` (its status).
    pub fn note(&self, predicate: &str) -> String {
        const UNSETTLED: &str = "nothing since says it happened or was dropped";
        match self {
            Mark::Later { since, value, .. } => {
                let mut v: String = value.chars().take(LATER_CHARS).collect();
                if v.len() < value.len() {
                    v.push('…');
                }
                format!("later on {}: \"{v}\"", day(*since))
            }
            Mark::Stale { since } => format!("stale: as of {}, not confirmed since", day(*since)),
            Mark::Overdue { plan, .. } if plan == predicate => format!("passed; {UNSETTLED}"),
            Mark::Overdue { since, plan } => {
                format!("its {plan} date, {}, passed; {UNSETTLED}", day(*since))
            }
        }
    }

    /// `{"mark", "since", "plan"?, "fact_id"?, "value"?, "note"}`, as JSON answers carry it (`freshness`) on a
    /// value of `predicate`. A `later` mark names the newer fact and its value.
    pub fn to_json(&self, predicate: &str) -> Value {
        let mut v = json!({"mark": self.kind(), "note": self.note(predicate)});
        match self {
            Mark::Later { since, fact, value } => {
                v["since"] = json!(day(*since));
                v["fact_id"] = json!(fact);
                v["value"] = json!(value);
            }
            Mark::Stale { since } => v["since"] = json!(day(*since)),
            Mark::Overdue { since, plan } => {
                v["since"] = json!(day(*since));
                v["plan"] = json!(plan);
            }
        }
        v
    }
}

/// The kinds of mark reads give ([`KINDS_ENV`]): `later` and `overdue`, unless the environment names others.
fn kinds() -> BTreeSet<String> {
    match std::env::var(KINDS_ENV) {
        Ok(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|k| k.trim().to_ascii_lowercase())
            .filter(|k| !k.is_empty())
            .collect(),
        _ => ["later", "overdue"].into_iter().map(String::from).collect(),
    }
}

/// `2026-04-15`, UTC.
pub fn day(us: i64) -> String {
    chrono::DateTime::from_timestamp_micros(us)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| us.to_string())
}

/// Whether the environment turns marks on (`AMPUP_MEMORY_FRESHNESS=1`, `true` or `on`) where no setting says.
pub fn from_env() -> bool {
    std::env::var(ENV).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        )
    })
}

/// Whether reads in `scope` mark freshness: its team's `freshness` setting, else its org's, else [`ENV`]. Off
/// unless one of them turns it on, until an A/B shows it helps. A directory that can't be read now answers as this
/// server last read it, and off before it ever has: never [`ENV`] alone, which a scope's own setting may
/// contradict, so an error can't turn the setting over for one read.
pub async fn on(store: &crate::Store, scope: &crate::Scope) -> bool {
    match store.directory().await {
        Ok(d) => d.dir.marks_freshness(scope),
        Err(_) => store
            .directory_held()
            .await
            .is_some_and(|d| d.dir.marks_freshness(scope)),
    }
}

/// Whether any of `schemas` says anything about freshness, so a read has anything to mark.
pub fn any(schemas: &[&Schema]) -> bool {
    schemas.iter().any(|s| {
        s.types.values().any(|t| {
            t.predicates
                .values()
                .any(|p| p.freshness_days.is_some() || p.plan || !p.pending.is_empty())
        }) || aggregates(s).is_some_and(|a| !a.freshness_days.is_empty() || !a.plans.is_empty())
    })
}

/// What a read of `facts` reads beside them so [`marks`] sees each entity's plans and statuses: the entities
/// whose type has a plan or a pending status, and those predicates. Both empty when none has.
pub fn beside(schemas: &[&Schema], facts: &[&Fact]) -> (BTreeSet<String>, BTreeSet<String>) {
    let (mut ids, mut preds) = (BTreeSet::new(), BTreeSet::new());
    for f in facts {
        let Some(ty) = type_of(schemas, f) else {
            continue;
        };
        let mut has = false;
        for s in schemas {
            let Some(t) = s.types.get(ty) else {
                continue;
            };
            let plans = aggregates(s).map_or(&[][..], |a| a.plans.as_slice());
            for (p, d) in &t.predicates {
                if d.plan || !d.pending.is_empty() || plans.contains(p) {
                    preds.insert(p.clone());
                    has = true;
                }
            }
        }
        if has {
            ids.insert(f.entity.clone());
        }
    }
    (ids, preds)
}

/// The type of `f`'s entity, as written or as the first schema that names one gives it.
pub fn type_of<'a>(schemas: &[&'a Schema], f: &'a Fact) -> Option<&'a str> {
    f.entity_type
        .as_deref()
        .or_else(|| schemas.iter().find_map(|s| s.entity_type(&f.entity)))
}

fn aggregates(s: &Schema) -> Option<&crate::query::schema::Aggregates> {
    s.extract.as_ref().and_then(|x| x.aggregates.as_ref())
}

/// What the schemas say of `predicate` on `ty`: the first that declares it, or whose aggregates name it. None
/// when that says nothing about freshness.
struct Rule<'a> {
    days: Option<u32>,
    plan: bool,
    pending: &'a [String],
}

impl Rule<'_> {
    /// Whether `f`'s value is one of the pending ones (case and surrounding space aside).
    fn pends(&self, f: &Fact) -> bool {
        let v = f.value.trim();
        self.pending
            .iter()
            .any(|p| p.trim().eq_ignore_ascii_case(v))
    }
}

fn rule<'a>(schemas: &[&'a Schema], ty: Option<&str>, predicate: &str) -> Option<Rule<'a>> {
    for s in schemas {
        let def = ty
            .and_then(|t| s.types.get(t))
            .and_then(|t| t.predicates.get(predicate));
        let agg = aggregates(s);
        let agg_days = agg.and_then(|a| a.freshness_days.get(predicate).copied());
        let agg_plan = agg.is_some_and(|a| a.plans.iter().any(|p| p == predicate));
        if def.is_none() && agg_days.is_none() && !agg_plan {
            continue;
        }
        let r = Rule {
            days: def.and_then(|d| d.freshness_days).or(agg_days),
            plan: def.is_some_and(|d| d.plan) || agg_plan,
            pending: def.map_or(&[][..], |d| d.pending.as_slice()),
        };
        return (r.days.is_some() || r.plan || !r.pending.is_empty()).then_some(r);
    }
    None
}

/// When `f`'s plan date passed, if it has by `at`: a date at the end of its day, a time at once.
fn passed(f: &Fact, at: i64) -> Option<i64> {
    let ts = f
        .value_ts
        .or_else(|| parse_time(f.value.trim()).map(|t| t.timestamp_micros()))?;
    let date = match f.value_type.as_deref() {
        Some(t) => t == ValueType::Date.as_str(),
        None => chrono::NaiveDate::parse_from_str(f.value.trim(), "%Y-%m-%d").is_ok(),
    };
    let due = if date { ts.saturating_add(DAY_US) } else { ts };
    (due <= at).then_some(ts)
}

/// The latest of `facts`' passed plans (each plan's newest statement, of whichever entity), as (when, predicate):
/// what an item a summary still holds open is overdue since. None when no plan has passed by `at`.
pub fn passed_plan(schemas: &[&Schema], facts: &[&Fact], at: i64) -> Option<(i64, String)> {
    let mut newest: HashMap<(&str, &str), &Fact> = HashMap::new();
    for f in facts {
        let ty = type_of(schemas, f);
        if rule(schemas, ty, &f.predicate).is_some_and(|r| r.plan)
            && newest
                .get(&(f.entity.as_str(), f.predicate.as_str()))
                .is_none_or(|g| (g.valid_from, &g.fact_id) < (f.valid_from, &f.fact_id))
        {
            newest.insert((f.entity.as_str(), f.predicate.as_str()), f);
        }
    }
    newest
        .values()
        .filter_map(|f| passed(f, at).map(|t| (t, f.predicate.clone())))
        .max()
}

/// A value as compared for "stated again": case, spacing and punctuation aside.
fn squash(v: &str) -> String {
    v.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// What tells a value stated again from one only repeated, for the stale rule ([`crate::corroboration::Said`]).
pub trait Stated {
    /// Whether `by`, a fact giving `f`'s value, states it again: a source independent of every one that stated `f`,
    /// whose words aren't an agent's.
    fn again(&self, f: &Fact, by: &Fact) -> bool;
    /// When such a source last stated `f`'s value where reconcile kept no fact of it (a repeat it dropped).
    fn restated(&self, f: &Fact) -> Option<i64>;
}

/// The marks on `facts` (current facts, each entity's plans and statuses among them) as of `at`, by fact id, of
/// the kinds reads give ([`KINDS_ENV`]). A fact later and stale is later; stale and overdue, overdue. `stated` says
/// what states a value again.
pub fn marks(
    schemas: &[&Schema],
    facts: &[&Fact],
    at: i64,
    stated: &dyn Stated,
) -> HashMap<String, Mark> {
    let kinds = kinds();
    let mut out = passed_and_stale(schemas, facts, at, stated);
    out.retain(|_, m| kinds.contains(m.kind()));
    if kinds.contains("later") {
        out.extend(later(schemas, facts, at));
    }
    out
}

/// Words that say nothing of what a statement is about.
const STOP: &str = "a about again all already also an and any are as at be been being but by can could current \
    currently did do does each few for from had has have he her him his how i in into is it its just less many \
    may me might mine more most much must my new no not now of on only or our over recent recently s she should \
    so some still such than that the their them then there these they this those to under user users very was \
    we were what when where which who whom whose why will with would yes you your";

/// Number words, counted as figures.
const NUMBERS: &str = "zero one two three four five six seven eight nine ten eleven twelve thirteen fourteen \
    fifteen sixteen seventeen eighteen nineteen twenty thirty forty fifty hundred thousand single double first \
    second third fourth fifth sixth seventh eighth ninth tenth half";

/// Whether `w` is one of `list`'s words.
fn listed(list: &str, w: &str) -> bool {
    list.split_ascii_whitespace().any(|x| x == w)
}

/// Month names, as dates write them.
const MONTHS: &str = "jan january feb february mar march apr april may jun june jul july aug august sep sept \
    september oct october nov november dec december";

/// A value as a statement: the words it is about (stop words out, a plural `s` off) and its figures (numbers and
/// number words, a time's parts each on its own). A date in it is neither: it says when, not what changed
/// ("2024-12-04: all eleven cases" and "2024-12-31: all eleven cases" say the same thing of two days).
fn statement(v: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    // Each token with the character just before it, so `2024-12-04` and `Jun 12, 2026` read as dates.
    let mut toks: Vec<(String, char)> = vec![];
    let (mut cur, mut sep) = (String::new(), ' ');
    for c in v.chars().chain([' ']) {
        if c.is_alphanumeric() {
            cur.extend(c.to_lowercase());
        } else {
            if !cur.is_empty() {
                toks.push((std::mem::take(&mut cur), sep));
            }
            sep = c;
        }
    }
    let year = |t: &str| t.len() == 4 && t.parse::<u32>().is_ok_and(|y| (1900..2100).contains(&y));
    let day = |t: &str| {
        let d = t.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        (t == d || ["st", "nd", "rd", "th"].contains(&&t[d.len()..]))
            && d.parse::<u32>().is_ok_and(|n| (1..=31).contains(&n))
    };
    let num = |t: &str| t.chars().all(|c| c.is_ascii_digit());
    let joined = |c: char| c == '-' || c == '/';
    let mut date: Vec<bool> = toks
        .iter()
        .map(|(t, _)| year(t) || listed(MONTHS, t))
        .collect();
    for i in 0..toks.len() {
        let (t, s) = (&toks[i].0, toks[i].1);
        let after_month = i > 0 && listed(MONTHS, &toks[i - 1].0) && !listed(MONTHS, t);
        let before_month = toks.get(i + 1).is_some_and(|(n, _)| listed(MONTHS, n));
        if (day(t) && (after_month || before_month))
            || (num(t) && joined(s) && i > 0 && date[i - 1])
        {
            date[i] = true;
        }
    }
    // A date's leading parts (`12/04/2024`, `04-12`), from its end.
    for i in (0..toks.len().saturating_sub(1)).rev() {
        if num(&toks[i].0) && joined(toks[i + 1].1) && date[i + 1] {
            date[i] = true;
        }
    }
    let (mut words, mut figures) = (BTreeSet::new(), BTreeSet::new());
    for ((w, _), d) in toks.into_iter().zip(date) {
        if d {
            continue;
        }
        if w.starts_with(|c: char| c.is_ascii_digit()) || listed(NUMBERS, &w) {
            figures.insert(w);
        } else if !listed(STOP, &w) {
            let w = match w.strip_suffix('s') {
                Some(s) if w.chars().count() > 3 && !s.ends_with('s') => s.to_string(),
                _ => w,
            };
            if w.chars().count() > 1 {
                words.insert(w);
            }
        }
    }
    (words, figures)
}

/// How closely a restatement must match its older statement: the words they must have in common, at least, and
/// as a share of all their words (Jaccard).
const SHARED_WORDS: usize = 2;
const CLOSE: f64 = 0.4;

/// `later` marks: each current fact of a predicate with `freshness_days` (not a plan, not a pending status) that a
/// newer current fact of the same entity restates with other figures. A restatement is said after it (a later
/// `valid_from`), of a predicate with `freshness_days` too, in the same words (at least [`SHARED_WORDS`] in common,
/// and [`CLOSE`] of all of them), with figures that differ both ways: each has a number the other lacks ("a single
/// free night", then "two free nights"; "used it for 6 months", then "for 9 months"). The newest restatement is the
/// one named. Only the same entity's: another entity's look-alike is most often another thing's figure (a second
/// plan's deductible, another rollout's error rate).
fn later(schemas: &[&Schema], facts: &[&Fact], at: i64) -> HashMap<String, Mark> {
    struct Said<'a> {
        f: &'a Fact,
        words: BTreeSet<String>,
        figures: BTreeSet<String>,
    }
    let said: Vec<Said> = facts
        .iter()
        .filter(|f| f.valid_to.is_none_or(|t| t > at) && f.valid_from <= at)
        .filter_map(|f| {
            let r = rule(schemas, type_of(schemas, f), &f.predicate)?;
            (r.days.is_some() && !r.plan && r.pending.is_empty()).then_some(f)
        })
        .map(|f| {
            let (words, figures) = statement(&f.value);
            Said { f, words, figures }
        })
        .filter(|s| !s.figures.is_empty() && s.words.len() >= SHARED_WORDS)
        .collect();
    let mut out: HashMap<String, Mark> = HashMap::new();
    for old in &said {
        let mut newest: Option<&Said> = None;
        for new in &said {
            if new.f.entity != old.f.entity
                || new.f.valid_from <= old.f.valid_from
                || old.figures.is_subset(&new.figures)
                || new.figures.is_subset(&old.figures)
            {
                continue;
            }
            let shared = old.words.intersection(&new.words).count();
            let all = old.words.union(&new.words).count();
            if shared < SHARED_WORDS || (shared as f64) < CLOSE * all as f64 {
                continue;
            }
            if newest
                .is_none_or(|n| (n.f.valid_from, &n.f.fact_id) < (new.f.valid_from, &new.f.fact_id))
            {
                newest = Some(new);
            }
        }
        if let Some(n) = newest {
            out.insert(
                old.f.fact_id.clone(),
                Mark::Later {
                    since: n.f.valid_from,
                    fact: n.f.fact_id.clone(),
                    value: n.f.value.clone(),
                },
            );
        }
    }
    out
}

/// Plans passed and values gone stale (`marks` before restatements): a fact both is overdue.
fn passed_and_stale(
    schemas: &[&Schema],
    facts: &[&Fact],
    at: i64,
    stated: &dyn Stated,
) -> HashMap<String, Mark> {
    let mut out = HashMap::new();
    let ruled: Vec<(&Fact, Rule)> = facts
        .iter()
        .filter(|f| f.valid_to.is_none_or(|t| t > at))
        .filter_map(|f| rule(schemas, type_of(schemas, f), &f.predicate).map(|r| (*f, r)))
        .collect();
    if ruled.is_empty() {
        return out;
    }
    // The newest statement of each plan and status, per entity.
    let mut newest: HashMap<(&str, &str), (&Fact, &Rule)> = HashMap::new();
    for (f, r) in &ruled {
        if (r.plan || !r.pending.is_empty())
            && newest
                .get(&(f.entity.as_str(), f.predicate.as_str()))
                .is_none_or(|(g, _)| (g.valid_from, &g.fact_id) < (f.valid_from, &f.fact_id))
        {
            newest.insert((f.entity.as_str(), f.predicate.as_str()), (f, r));
        }
    }
    // Per entity: its plans that have passed, its statuses still pending, and whether a status says it's settled.
    #[derive(Default)]
    struct Standing<'a> {
        plans: Vec<(&'a Fact, i64)>,
        pending: Vec<&'a Fact>,
        settled: bool,
    }
    let mut standing: HashMap<&str, Standing> = HashMap::new();
    for ((entity, _), (f, r)) in &newest {
        let s = standing.entry(*entity).or_default();
        if r.plan
            && let Some(ts) = passed(f, at)
        {
            s.plans.push((f, ts));
        }
        if !r.pending.is_empty() {
            match r.pends(f) {
                true => s.pending.push(f),
                false => s.settled = true,
            }
        }
    }
    for s in standing.values() {
        if s.settled {
            continue;
        }
        let Some(&(last, since)) = s
            .plans
            .iter()
            .max_by(|a, b| (a.1, &a.0.predicate).cmp(&(b.1, &b.0.predicate)))
        else {
            continue;
        };
        for (f, ts) in &s.plans {
            out.insert(
                f.fact_id.clone(),
                Mark::Overdue {
                    since: *ts,
                    plan: f.predicate.clone(),
                },
            );
        }
        for f in &s.pending {
            out.insert(
                f.fact_id.clone(),
                Mark::Overdue {
                    since,
                    plan: last.predicate.clone(),
                },
            );
        }
    }
    // Stale: last stated (this value, by the fact itself or a source independent of it) longer ago than the
    // predicate's window.
    let mut same: HashMap<(&str, &str, String), Vec<&Fact>> = HashMap::new();
    for (f, r) in &ruled {
        if r.days.is_some() {
            same.entry((f.entity.as_str(), f.predicate.as_str(), squash(&f.value)))
                .or_default()
                .push(f);
        }
    }
    for (f, r) in &ruled {
        let Some(days) = r.days else {
            continue;
        };
        // A settled status (`done`, `won`) stays settled: only a pending one ages.
        if out.contains_key(&f.fact_id) || (!r.pending.is_empty() && !r.pends(f)) {
            continue;
        }
        let last = same[&(f.entity.as_str(), f.predicate.as_str(), squash(&f.value))]
            .iter()
            .filter(|g| g.fact_id == f.fact_id || stated.again(f, g))
            .map(|g| g.valid_from)
            .chain(stated.restated(f))
            .fold(f.valid_from, i64::max);
        if at.saturating_sub(last) > days as i64 * DAY_US {
            out.insert(f.fact_id.clone(), Mark::Stale { since: last });
        }
    }
    out
}
