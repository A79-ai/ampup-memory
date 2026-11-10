//! Seeded corpus generator for the verification harness (M7.0). Emits episodes, the ground-truth
//! facts they imply (validity windows, supersessions, scope, ACL) and gold questions in six
//! categories. The same seed and size always produce byte-identical output.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const DAY: i64 = 86_400_000_000;
/// 2025-01-01T00:00:00Z in microseconds.
const START: i64 = 1_735_689_600_000_000;
const ROLES: &[&str] = &[
    "Engineer",
    "Senior Engineer",
    "Staff Engineer",
    "Engineering Manager",
    "Account Executive",
    "Sales Engineer",
    "Product Manager",
    "Designer",
];
const STAGES: &[&str] = &[
    "prospect",
    "discovery",
    "evaluation",
    "negotiation",
    "closed_won",
    "renewal",
];
const NOTES: &[&str] = &[
    "prefers async updates",
    "is interviewing elsewhere",
    "has a medical leave planned",
    "wants to move to product",
    "asked for a raise",
];
const SOURCES: &[&str] = &["chat", "call", "crm", "code", "ticket"];

/// The source a generated episode is written under: every episode is of an episode type
/// (`crate::episode_types`), so each kind of generated record gets a source its type claims. A CRM line is
/// generated as prose, not a record change, so it is a note.
fn typed_source(kind: &str) -> &'static str {
    match kind {
        "chat" => "slack:gen",
        "call" => "meeting:gen",
        "code" => "github:gen",
        "crm" => "note:crm",
        "hr" => "doc:hr",
        _ => "doc:ticket",
    }
}

/// Warehouse JSON exports write INT64 as a string (BigQuery does, to keep 64-bit precision).
fn int_or_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        I(i64),
        S(String),
    }
    match N::deserialize(d)? {
        N::I(i) => Ok(i),
        N::S(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

fn indexed() -> bool {
    true
}

fn is_indexed(b: &bool) -> bool {
    *b
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GenEpisode {
    pub episode_id: String,
    pub source: String,
    #[serde(deserialize_with = "int_or_string")]
    pub occurred_at: i64,
    /// `org`, `team:<id>` or `user:<id>`.
    pub scope: String,
    pub acl: Vec<String>,
    /// Whether passage search indexes the body (false for records that aren't prose). Absent means
    /// true, and true is never written, so generated corpora stay byte-identical.
    #[serde(default = "indexed", skip_serializing_if = "is_indexed")]
    pub index_text: bool,
    /// Entity ids the episode is about; absent in generated corpora and older exports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub about: Vec<String>,
    /// The document this episode is a version of (see [`crate::Episode::document`]); usually absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    pub body: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GenFact {
    pub fact_id: String,
    pub scope: String,
    pub entity: String,
    pub predicate: String,
    pub value: String,
    #[serde(deserialize_with = "int_or_string")]
    pub valid_from: i64,
    pub valid_to: Option<i64>,
    pub source_episodes: Vec<String>,
    pub supersedes: Option<String>,
    /// Verbatim supporting span from extraction; absent in generated corpora and older exports.
    #[serde(default)]
    pub quote: Option<String>,
    /// A number's unit; a typed scope's schema sets it on write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Where in its episode the fact comes from (an `episode_types::Cite`, as JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cite: Option<serde_json::Value>,
    /// `stated`, `as_of_source` or `inferred`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_basis: Option<String>,
}

/// An explicit end of a fact: it stopped being true at `valid_to`, as episode `episode_id` (in
/// the same scope) shows. Loaded after the scope's facts, committed with its episode.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GenClosure {
    pub fact_id: String,
    pub scope: String,
    #[serde(deserialize_with = "int_or_string")]
    pub valid_to: i64,
    pub episode_id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GenQuestion {
    pub id: String,
    /// single_hop | multi_hop | as_of | aggregation | permission | freshness
    pub category: String,
    pub scope: String,
    pub principal_user: String,
    pub principal_groups: Vec<String>,
    pub question: String,
    /// Structured form so a checker needs no language model: a chain of (predicate) hops from
    /// `entity`, or `count_by_value` for aggregation.
    pub entity: String,
    pub path: Vec<String>,
    pub as_of: Option<i64>,
    /// The gold answer, or `WITHHOLD` when the principal must not learn it.
    pub answer: String,
    pub cites: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Manifest {
    pub seed: u64,
    pub episodes: usize,
    pub facts: usize,
    pub questions: usize,
    pub entities: usize,
    /// Entities whose degree is above the 99.9th percentile.
    pub hubs_above_p999: usize,
    pub max_degree: usize,
    /// sha256 over episodes, facts and questions output, in that order.
    pub content_hash: String,
}

/// splitmix64: tiny, fast, and stable across versions (no external RNG crate whose stream
/// could change under us).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 1000 < pct
    }
    /// Zipf-like: index 0 is by far the most likely, giving a few hub entities.
    fn skewed(&mut self, n: usize) -> usize {
        let u = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        ((n as f64).powf(u) - 1.0) as usize % n
    }
}

/// One supersedable attribute's history: (value, valid_from, episode, fact_id).
type History = Vec<(String, i64, String, String)>;

struct State<'a> {
    rng: Rng,
    out: &'a mut dyn FnMut(Line) -> Result<()>,
    facts: usize,
    episodes: usize,
    /// (entity, predicate) -> history, for attributes that supersede.
    history: BTreeMap<(String, String), History>,
    /// Scope and ACL of each attribute, fixed per (entity, predicate).
    attr_scope: HashMap<(String, String), (String, Vec<String>)>,
    degree: HashMap<String, usize>,
    next_fact: u64,
    t: i64,
}

pub enum Line {
    Episode(GenEpisode),
    Fact(GenFact),
    Question(GenQuestion),
}

impl State<'_> {
    fn episode(
        &mut self,
        source: &str,
        scope: &str,
        acl: &[String],
        body: String,
    ) -> Result<String> {
        let id: String = Sha256::digest(body.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        self.episodes += 1;
        (self.out)(Line::Episode(GenEpisode {
            episode_id: id.clone(),
            source: typed_source(source).into(),
            occurred_at: self.t,
            scope: scope.into(),
            acl: acl.to_vec(),
            about: vec![],
            index_text: true,
            document: None,
            body,
        }))?;
        Ok(id)
    }

    fn fact_id(&mut self) -> String {
        self.next_fact += 1;
        format!("f{:09}", self.next_fact)
    }

    /// A fact that never supersedes (an event or an edge). Written immediately.
    fn event(
        &mut self,
        scope: &str,
        entity: &str,
        predicate: &str,
        value: &str,
        ep: &str,
    ) -> Result<()> {
        *self.degree.entry(entity.into()).or_default() += 1;
        *self.degree.entry(value.into()).or_default() += 1;
        let fact_id = self.fact_id();
        self.facts += 1;
        (self.out)(Line::Fact(GenFact {
            fact_id,
            scope: scope.into(),
            entity: entity.into(),
            predicate: predicate.into(),
            value: value.into(),
            valid_from: self.t,
            valid_to: None,
            source_episodes: vec![ep.into()],
            supersedes: None,
            quote: None,
            unit: None,
            cite: None,
            time_basis: None,
        }))
    }

    /// Set a supersedable attribute. The previous value is closed at `t` and written now; the
    /// open one is written at the end.
    fn set(
        &mut self,
        scope: &str,
        acl: &[String],
        entity: &str,
        predicate: &str,
        value: &str,
        ep: &str,
    ) -> Result<()> {
        let key = (entity.to_string(), predicate.to_string());
        self.attr_scope
            .entry(key.clone())
            .or_insert_with(|| (scope.to_string(), acl.to_vec()));
        *self.degree.entry(entity.into()).or_default() += 1;
        let fact_id = self.fact_id();
        let hist = self.history.entry(key.clone()).or_default();
        let prev = hist.last().cloned();
        hist.push((value.into(), self.t, ep.into(), fact_id));
        if let Some((pv, pfrom, pep, pid)) = prev {
            let prev_supersedes = hist.len().checked_sub(3).map(|i| hist[i].3.clone());
            self.facts += 1;
            (self.out)(Line::Fact(GenFact {
                fact_id: pid,
                scope: scope.into(),
                entity: entity.into(),
                predicate: predicate.into(),
                value: pv,
                valid_from: pfrom,
                valid_to: Some(self.t),
                source_episodes: vec![pep],
                supersedes: prev_supersedes,
                quote: None,
                unit: None,
                cite: None,
                time_basis: None,
            }))?;
        }
        Ok(())
    }

    fn current(&self, entity: &str, predicate: &str) -> Option<&(String, i64, String, String)> {
        self.history.get(&(entity.into(), predicate.into()))?.last()
    }

    fn value_at(
        &self,
        entity: &str,
        predicate: &str,
        t: i64,
    ) -> Option<&(String, i64, String, String)> {
        self.history
            .get(&(entity.into(), predicate.into()))?
            .iter()
            .rev()
            .find(|h| h.1 <= t)
    }
}

/// Generate a corpus. `out` receives every line in order; pass a sink that only hashes for a
/// size check without writing files.
pub fn generate(
    seed: u64,
    n_episodes: usize,
    out: &mut dyn FnMut(Line) -> Result<()>,
) -> Result<Manifest> {
    let people = (n_episodes / 50).max(20);
    let teams = (people / 10).max(3);
    let projects = (people / 5).max(5);
    let accounts = (people / 4).max(10);
    let span = 365 * DAY;
    let mut s = State {
        rng: Rng(seed),
        out,
        facts: 0,
        episodes: 0,
        history: BTreeMap::new(),
        attr_scope: HashMap::new(),
        degree: HashMap::new(),
        next_fact: 0,
        t: START,
    };
    let person = |i: usize| format!("p{i:06}");
    let team = |i: usize| format!("t{i:04}");
    let project = |i: usize| format!("proj{i:05}");
    let account = |i: usize| format!("acct{i:05}");
    let org = "org".to_string();
    let all = vec!["*".to_string()];
    let team_acl = |t: &str| vec![format!("group:{t}")];

    // Bootstrap: one HR import per person, one charter per team and project.
    for p in 0..people {
        let (role, tm) = (ROLES[s.rng.below(ROLES.len())], team(p % teams));
        let ep = s.episode(
            "hr",
            &org,
            &all,
            format!("HR import: {} joined {tm} as {role}.", person(p)),
        )?;
        s.set(&org, &all, &person(p), "role", role, &ep)?;
        s.set(&org, &all, &person(p), "team", &tm, &ep)?;
    }
    for t in 0..teams {
        let mgr = person(t + teams * s.rng.below(people / teams));
        let ep = s.episode(
            "hr",
            &org,
            &all,
            format!("Team charter: {} is managed by {mgr}.", team(t)),
        )?;
        s.set(&org, &all, &team(t), "manager", &mgr, &ep)?;
    }
    for r in 0..projects {
        let owner = person(s.rng.below(people));
        let tm = s.current(&owner, "team").unwrap().0.clone();
        let (sc, acl) = (format!("team:{tm}"), team_acl(&tm));
        let ep = s.episode(
            "code",
            &sc,
            &acl,
            format!("Project kickoff: {} owned by {owner}.", project(r)),
        )?;
        s.set(&sc, &acl, &project(r), "owner", &owner, &ep)?;
    }

    let mut ticket = 0usize;
    let fixed = s.episodes;
    let body_n = n_episodes.saturating_sub(fixed);
    for i in 0..body_n {
        // i128: span * i overflows i64 beyond ~290k episodes.
        s.t = START + (span as i128 * i as i128 / body_n.max(1) as i128) as i64 + 1;
        let roll = s.rng.below(1000);
        let p = person(s.rng.below(people));
        let tag = format!("#{i}");
        // One primary change per episode, then 4-7 mention edges, so ~5-8 facts per episode.
        let (ep, sc) = if roll < 30 {
            let role = ROLES[s.rng.below(ROLES.len())];
            let ep = s.episode(
                "chat",
                &org,
                &all,
                format!("{tag} Announcement: {p} is now {role}."),
            )?;
            s.set(&org, &all, &p, "role", role, &ep)?;
            (ep, org.clone())
        } else if roll < 50 {
            let tm = team(s.rng.below(teams));
            let ep = s.episode("chat", &org, &all, format!("{tag} {p} moved to {tm}."))?;
            s.set(&org, &all, &p, "team", &tm, &ep)?;
            (ep, org.clone())
        } else if roll < 60 {
            let tm = team(s.rng.below(teams));
            let ep = s.episode("chat", &org, &all, format!("{tag} {p} now manages {tm}."))?;
            s.set(&org, &all, &tm, "manager", &p, &ep)?;
            (ep, org.clone())
        } else if roll < 80 {
            let r = project(s.rng.below(projects));
            let (sc, acl) = s.attr_scope[&(r.clone(), "owner".to_string())].clone();
            let ep = s.episode(
                "code",
                &sc,
                &acl,
                format!("{tag} Ownership of {r} transferred to {p}."),
            )?;
            s.set(&sc, &acl, &r, "owner", &p, &ep)?;
            (ep, sc)
        } else if roll < 160 {
            let a = account(s.rng.skewed(accounts));
            let next = s.current(&a, "stage").map_or(0, |c| {
                (STAGES.iter().position(|x| *x == c.0).unwrap() + 1) % STAGES.len()
            });
            let ep = s.episode(
                "crm",
                &org,
                &all,
                format!("{tag} CRM: {a} moved to {} (owner {p}).", STAGES[next]),
            )?;
            s.set(&org, &all, &a, "stage", STAGES[next], &ep)?;
            s.set(&org, &all, &a, "account_owner", &p, &ep)?;
            (ep, org.clone())
        } else if roll < 210 {
            let note = NOTES[s.rng.below(NOTES.len())];
            let (sc, acl) = (format!("user:{p}"), vec![format!("user:{p}")]);
            let ep = s.episode(
                "chat",
                &sc,
                &acl,
                format!("{tag} Private note: {p} {note}."),
            )?;
            s.set(&sc, &acl, &p, "private_note", note, &ep)?;
            (ep, sc)
        } else {
            let a = account(s.rng.skewed(accounts));
            ticket += 1;
            let tk = format!("ticket{ticket:07}");
            let src = SOURCES[s.rng.below(SOURCES.len())];
            let ep = s.episode(
                src,
                &org,
                &all,
                format!("{tag} {src}: {p} spoke with {a} about {tk}."),
            )?;
            s.set(&org, &all, &a, "last_contact", &p, &ep)?;
            s.event(&org, &tk, "opened_for", &a, &ep)?;
            (ep, org.clone())
        };
        for _ in 0..4 + s.rng.below(4) {
            let other = if s.rng.chance(500) {
                account(s.rng.skewed(accounts))
            } else {
                person(s.rng.below(people))
            };
            s.event(&sc, &p, "mentioned_with", &other, &ep)?;
        }
    }

    // Close out: every attribute's open (current) value.
    let open: Vec<_> = s
        .history
        .iter()
        .map(|(k, h)| (k.clone(), h.clone()))
        .collect();
    for ((entity, predicate), hist) in &open {
        let (value, from, ep, id) = hist.last().unwrap().clone();
        let (scope, _) = s.attr_scope[&(entity.clone(), predicate.clone())].clone();
        s.facts += 1;
        (s.out)(Line::Fact(GenFact {
            fact_id: id,
            scope,
            entity: entity.clone(),
            predicate: predicate.clone(),
            value,
            valid_from: from,
            valid_to: None,
            source_episodes: vec![ep],
            supersedes: hist.len().checked_sub(2).map(|i| hist[i].3.clone()),
            quote: None,
            unit: None,
            cite: None,
            time_basis: None,
        }))?;
    }

    let questions = questions(&mut s, people, teams, projects)?;

    let mut degrees: Vec<usize> = s.degree.values().copied().collect();
    degrees.sort_unstable();
    let p999 = degrees[(degrees.len() * 999 / 1000).min(degrees.len() - 1)];
    Ok(Manifest {
        seed,
        episodes: s.episodes,
        facts: s.facts,
        questions,
        entities: degrees.len(),
        hubs_above_p999: degrees.iter().filter(|d| **d > p999).count(),
        max_degree: *degrees.last().unwrap_or(&0),
        content_hash: String::new(),
    })
}

fn questions(s: &mut State, people: usize, teams: usize, projects: usize) -> Result<usize> {
    let per = 50;
    let mut n = 0;
    let mut emit = |s: &mut State, q: GenQuestion| -> Result<()> {
        n += 1;
        (s.out)(Line::Question(GenQuestion {
            id: format!("q{n:05}"),
            ..q
        }))
    };
    let q = |category: &str,
             scope: &str,
             user: &str,
             groups: Vec<String>,
             question: String,
             entity: &str,
             path: &[&str],
             as_of: Option<i64>,
             answer: String,
             cites: Vec<String>| GenQuestion {
        id: String::new(),
        category: category.into(),
        scope: scope.into(),
        principal_user: user.into(),
        principal_groups: groups,
        question,
        entity: entity.into(),
        path: path.iter().map(|p| p.to_string()).collect(),
        as_of,
        answer,
        cites,
    };
    let person = |i: usize| format!("p{i:06}");

    for _ in 0..per {
        let p = person(s.rng.below(people));
        let c = s.current(&p, "role").unwrap().clone();
        let qq = q(
            "single_hop",
            "org",
            "p000000",
            vec![],
            format!("What is {p}'s current role?"),
            &p,
            &["role"],
            None,
            c.0,
            vec![c.2],
        );
        emit(s, qq)?;
    }
    for _ in 0..per {
        let r = format!("proj{:05}", s.rng.below(projects));
        let (sc, _) = s.attr_scope[&(r.clone(), "owner".to_string())].clone();
        let owner = s.current(&r, "owner").unwrap().clone();
        let tm = s.current(&owner.0, "team").unwrap().clone();
        let mgr = s.current(&tm.0, "manager").unwrap().clone();
        let group = sc.trim_start_matches("team:").to_string();
        let qq = q(
            "multi_hop",
            &sc,
            "p000000",
            vec![group],
            format!("Who manages the team of the owner of {r}?"),
            &r,
            &["owner", "team", "manager"],
            None,
            mgr.0,
            vec![owner.2, tm.2, mgr.2],
        );
        emit(s, qq)?;
    }
    let changed: Vec<String> = s
        .history
        .iter()
        .filter(|((_, pred), h)| pred == "role" && h.len() >= 2)
        .map(|((e, _), _)| e.clone())
        .collect();
    for _ in 0..per.min(changed.len()) {
        let p = changed[s.rng.below(changed.len())].clone();
        let h = s.history[&(p.clone(), "role".to_string())].clone();
        let k = 1 + s.rng.below(h.len() - 1);
        let at = (h[k - 1].1 + h[k].1) / 2;
        let v = s.value_at(&p, "role", at).unwrap().clone();
        let qq = q(
            "as_of",
            "org",
            "p000000",
            vec![],
            format!("What was {p}'s role at {at}?"),
            &p,
            &["role"],
            Some(at),
            v.0,
            vec![v.2],
        );
        emit(s, qq)?;
    }
    for _ in 0..per {
        let tm = format!("t{:04}", s.rng.below(teams));
        let members: Vec<String> = s
            .history
            .iter()
            .filter(|((_, pred), h)| pred == "team" && h.last().unwrap().0 == tm)
            .map(|(_, h)| h.last().unwrap().2.clone())
            .collect();
        let qq = q(
            "aggregation",
            "org",
            "p000000",
            vec![],
            format!("How many people are currently in {tm}?"),
            &tm,
            &["count_by_value", "team"],
            None,
            members.len().to_string(),
            members,
        );
        emit(s, qq)?;
    }
    let notes: Vec<String> = s
        .history
        .keys()
        .filter(|(_, pred)| pred == "private_note")
        .map(|(e, _)| e.clone())
        .collect();
    for _ in 0..per.min(notes.len()) {
        let p = notes[s.rng.below(notes.len())].clone();
        let c = s.current(&p, "private_note").unwrap().clone();
        let mut asker = person(s.rng.below(people));
        if asker == p {
            asker = format!("{p}_x");
        }
        let qq = q(
            "permission",
            &format!("user:{p}"),
            &asker,
            vec![],
            format!("What is {p}'s private note?"),
            &p,
            &["private_note"],
            None,
            "WITHHOLD".into(),
            vec![c.2],
        );
        emit(s, qq)?;
    }
    // Freshness: the most recently changed org attributes.
    let mut latest: Vec<_> = s
        .history
        .iter()
        .filter(|(k, _)| s.attr_scope[k].0 == "org")
        .map(|(k, h)| (h.last().unwrap().1, k.clone(), h.last().unwrap().clone()))
        .collect();
    latest.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, (e, pred), c) in latest.into_iter().take(per) {
        let qq = q(
            "freshness",
            "org",
            "p000000",
            vec![],
            format!("What is the current {pred} of {e}?"),
            &e,
            &[pred.as_str()],
            None,
            c.0,
            vec![c.2],
        );
        emit(s, qq)?;
    }
    Ok(n)
}

/// Write a corpus to `dir` (episodes.jsonl, facts.jsonl, questions.jsonl, manifest.json), or
/// with `dir = None` only count and hash it.
pub fn write(seed: u64, n_episodes: usize, dir: Option<&std::path::Path>) -> Result<Manifest> {
    let mut hasher = Sha256::new();
    let mut files = match dir {
        Some(d) => {
            std::fs::create_dir_all(d)?;
            let f = |n: &str| -> Result<Box<dyn Write>> {
                Ok(Box::new(std::io::BufWriter::new(std::fs::File::create(
                    d.join(n),
                )?)))
            };
            Some((
                f("episodes.jsonl")?,
                f("facts.jsonl")?,
                f("questions.jsonl")?,
            ))
        }
        None => None,
    };
    let mut sink = |line: Line| -> Result<()> {
        let (json, which) = match &line {
            Line::Episode(e) => (serde_json::to_string(e)?, 0),
            Line::Fact(f) => (serde_json::to_string(f)?, 1),
            Line::Question(q) => (serde_json::to_string(q)?, 2),
        };
        hasher.update(json.as_bytes());
        hasher.update(b"\n");
        if let Some((e, f, q)) = files.as_mut() {
            let w = [e, f, q][which].as_mut();
            w.write_all(json.as_bytes())?;
            w.write_all(b"\n")?;
        }
        Ok(())
    };
    let mut m = generate(seed, n_episodes, &mut sink)?;
    if let Some((mut e, mut f, mut q)) = files {
        e.flush()?;
        f.flush()?;
        q.flush()?;
    }
    m.content_hash = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if let Some(d) = dir {
        std::fs::write(d.join("manifest.json"), serde_json::to_string_pretty(&m)?)?;
    }
    Ok(m)
}

pub fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> Result<Vec<T>> {
    use std::io::BufRead;
    std::io::BufReader::new(std::fs::File::open(path)?)
        .lines()
        .map(|l| Ok(serde_json::from_str(&l?)?))
        .collect()
}

/// Every `<name>*.json[l]` in `dir`, in name order: one `episodes.jsonl`, or the shards a warehouse
/// export writes (BigQuery's `episodes-000000000000.json`, ...). No file at all reads as empty.
pub fn read_parts<T: for<'de> Deserialize<'de>>(
    dir: &std::path::Path,
    name: &str,
) -> Result<Vec<T>> {
    let mut parts: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let f = p.file_name().and_then(|f| f.to_str()).unwrap_or_default();
            f.starts_with(name) && (f.ends_with(".jsonl") || f.ends_with(".json"))
        })
        .collect();
    parts.sort();
    let mut out = vec![];
    for p in parts {
        out.extend(read_jsonl(&p)?);
    }
    Ok(out)
}

/// Which scopes a partial load or eval covers: `None` = all; `questions` = org plus every scope
/// a gold question is asked in (every category answerable without loading thousands of
/// per-user scopes); anything else = that one scope.
pub fn scope_filter(
    dir: &std::path::Path,
    only: Option<&str>,
) -> Result<Option<std::collections::HashSet<String>>> {
    Ok(match only {
        None => None,
        Some("questions") => {
            let qs: Vec<GenQuestion> = read_jsonl(&dir.join("questions.jsonl"))?;
            let mut s: std::collections::HashSet<String> =
                qs.into_iter().map(|q| q.scope).collect();
            s.insert("org".into());
            Some(s)
        }
        Some(one) => Some([one.to_string()].into()),
    })
}

/// Load a corpus into the engine: per scope, `batch` episodes per commit together with the
/// facts they source, then the scope's closures (`closures*.jsonl`), each committed with the
/// episode it cites. Ground-truth facts stand in for extraction, which is not built yet.
pub async fn load(
    store: &crate::Store,
    dir: &std::path::Path,
    batch: usize,
    only_scope: Option<&str>,
) -> Result<String> {
    load_into(store, dir, batch, only_scope, None, crate::MAIN, false).await
}

/// The scope a corpus's scope `name` is loaded into: itself, or in workspace `ws` its counterpart there (the
/// corpus's `org` is the workspace's own scope, its `team:<t>` the workspace's team `t`, made if missing).
async fn target_scope(store: &crate::Store, name: &str, ws: Option<&str>) -> Result<crate::Scope> {
    use crate::Scope;
    let Some(ws) = ws else {
        return Scope::parse(name);
    };
    match Scope::parse(name)? {
        Scope::Org => Scope::org(ws),
        Scope::Team(t) if !t.contains('/') => {
            let dir = crate::people::directory::Directory::load(store).await?;
            if !dir.orgs.get(ws).is_some_and(|o| o.teams.contains_key(&t)) {
                crate::people::directory::apply(
                    store,
                    &crate::people::cli::owner(),
                    &crate::people::directory::Change::CreateTeam {
                        org: ws.into(),
                        team: t.clone(),
                    },
                )
                .await?;
            }
            Scope::team(ws, &t)
        }
        other => anyhow::bail!(
            "corpus scope {other} can't go into workspace {ws}: only org and team:<team> map onto one"
        ),
    }
}

/// The scope [`target_scope`] gives, without making anything.
fn target_name(name: &str, ws: Option<&str>) -> Result<crate::Scope> {
    use crate::Scope;
    Ok(match (ws, Scope::parse(name)?) {
        (None, s) => s,
        (Some(ws), Scope::Org) => Scope::org(ws)?,
        (Some(ws), Scope::Team(t)) if !t.contains('/') => Scope::team(ws, &t)?,
        (Some(ws), other) => anyhow::bail!(
            "corpus scope {other} can't go into workspace {ws}: only org and team:<team> map onto one"
        ),
    })
}

/// [`load`] into workspace `ws` (see [`target_scope`]), if given, on `branch`.
///
/// Loading is idempotent: an episode already committed on the branch with the same ACL, a fact already
/// committed (the same id, or the same claim from the same evidence) and a closure already committed are
/// skipped, so loading a corpus again changes nothing. With `fresh`, the load is refused unless every scope
/// it would write to (and, into a workspace, every scope of the workspace) holds nothing yet.
pub async fn load_into(
    store: &crate::Store,
    dir: &std::path::Path,
    batch: usize,
    only_scope: Option<&str>,
    ws: Option<&str>,
    branch: &str,
    fresh: bool,
) -> Result<String> {
    use crate::{Episode, Fact};
    let episodes: Vec<GenEpisode> = read_parts(dir, "episodes")?;
    // A batch with nothing extracted may have no facts file (Snowflake's COPY writes none for 0 rows).
    let facts: Vec<GenFact> = read_parts(dir, "facts")?;
    let closures: Vec<GenClosure> = read_parts(dir, "closures")?;
    let wanted = scope_filter(dir, only_scope)?;
    // Keyed by scope too: the same content-addressed episode can appear in several scopes, each
    // with its own facts.
    let mut facts_by_ep: HashMap<(String, String), Vec<GenFact>> = HashMap::new();
    for f in facts {
        facts_by_ep
            .entry((f.scope.clone(), f.source_episodes[0].clone()))
            .or_default()
            .push(f);
    }
    let mut closures_by_scope: BTreeMap<String, Vec<GenClosure>> = BTreeMap::new();
    for c in closures {
        if wanted.as_ref().is_none_or(|w| w.contains(&c.scope)) {
            closures_by_scope
                .entry(c.scope.clone())
                .or_default()
                .push(c);
        }
    }
    // An episode only closures cite is committed with them, not ahead of them.
    let closing: std::collections::HashSet<(String, String)> = closures_by_scope
        .values()
        .flatten()
        .map(|c| (c.scope.clone(), c.episode_id.clone()))
        .filter(|k| !facts_by_ep.contains_key(k))
        .collect();
    let mut by_scope: BTreeMap<String, Vec<GenEpisode>> = BTreeMap::new();
    let mut episode_of: HashMap<(String, String), Episode> = HashMap::new();
    for e in episodes {
        if wanted.as_ref().is_none_or(|w| w.contains(&e.scope)) {
            let key = (e.scope.clone(), e.episode_id.clone());
            if closures_by_scope.contains_key(&e.scope) {
                episode_of.insert(key.clone(), to_episode(&e));
            }
            if !closing.contains(&key) {
                by_scope.entry(e.scope.clone()).or_default().push(e);
            }
        }
    }
    for scope in closures_by_scope.keys() {
        by_scope.entry(scope.clone()).or_default();
    }
    if fresh {
        let mut targets: Vec<crate::Scope> = by_scope
            .keys()
            .map(|n| target_name(n, ws))
            .collect::<Result<_>>()?;
        if let Some(ws) = ws {
            targets.extend(crate::workspace::scopes(store, ws).await?);
        }
        for t in targets {
            let ns = t.namespace();
            let (eps, facts) = (
                store.total_rows(&ns, crate::EPISODES).await?,
                store.total_rows(&ns, crate::FACTS).await?,
            );
            anyhow::ensure!(
                eps + facts == 0,
                "--fresh: {t} already holds {eps} episode rows and {facts} fact rows"
            );
        }
    }
    let (mut commits, mut n_eps, mut n_facts, mut n_closures) = (0, 0, 0, 0);
    let (mut skip_eps, mut skip_facts, mut skip_closures) = (0, 0, 0);
    let t0 = std::time::Instant::now();
    for (name, eps) in &by_scope {
        let scope = target_scope(store, name, ws).await?;
        store.ensure_scope(&scope).await?;
        // What the branch holds already; grown as this load commits, so a corpus that repeats itself is
        // committed once too.
        let mut have = store.committed_rows(&scope, branch).await?;
        for chunk in eps.chunks(batch) {
            let all: Vec<Episode> = chunk.iter().map(to_episode).collect();
            let n = all.len();
            let mut seen = std::collections::HashSet::new();
            let episodes: Vec<Episode> = all
                .into_iter()
                .filter(|e| {
                    !have.has_episode(e) && seen.insert((e.episode_id.clone(), e.acl.clone()))
                })
                .collect();
            skip_eps += n - episodes.len();
            let facts: Vec<Fact> = chunk
                .iter()
                .flat_map(|e| {
                    facts_by_ep
                        .get(&(e.scope.clone(), e.episode_id.clone()))
                        .into_iter()
                        .flatten()
                })
                .map(|f| Fact {
                    fact_id: f.fact_id.clone(),
                    entity: f.entity.clone(),
                    predicate: f.predicate.clone(),
                    value: f.value.clone(),
                    valid_from: f.valid_from,
                    valid_to: f.valid_to,
                    source_episodes: f.source_episodes.clone(),
                    quote: f.quote.clone(),
                    unit: f.unit.clone(),
                    cite: f.cite.as_ref().map(|c| c.to_string()),
                    time_basis: f.time_basis.clone(),
                    ..Default::default()
                })
                .collect();
            let n = facts.len();
            let mut seen = std::collections::HashSet::new();
            let facts: Vec<Fact> = facts
                .into_iter()
                .filter(|f| !have.has_fact(f) && seen.insert(f.fact_id.clone()))
                .collect();
            skip_facts += n - facts.len();
            if episodes.is_empty() && facts.is_empty() {
                continue;
            }
            n_eps += episodes.len();
            n_facts += facts.len();
            store
                .commit_on(&scope, branch, "load", &episodes, &facts)
                .await?;
            commits += 1;
            for e in &episodes {
                let mut acl = e.acl.clone();
                acl.sort();
                have.episodes.insert(e.episode_id.clone(), acl);
            }
            for f in &facts {
                have.fact_ids.insert(f.fact_id.clone());
                have.fact_keys.insert(crate::Committed::fact_key(f));
            }
        }
        for chunk in closures_by_scope
            .get(name)
            .map_or(&[][..], Vec::as_slice)
            .chunks(20_000)
        {
            let mut rows = vec![];
            let chunk: Vec<&GenClosure> = chunk
                .iter()
                .filter(|c| {
                    let done = have.closures.contains(&(
                        c.fact_id.clone(),
                        c.valid_to,
                        c.episode_id.clone(),
                    ));
                    skip_closures += usize::from(done);
                    !done
                })
                .collect();
            if chunk.is_empty() {
                continue;
            }
            for c in &chunk {
                let e = episode_of
                    .get(&(c.scope.clone(), c.episode_id.clone()))
                    .with_context(|| {
                        format!(
                            "closure of {} cites episode {}, which is not in {}'s episodes",
                            c.fact_id, c.episode_id, c.scope
                        )
                    })?;
                rows.push((c.fact_id.as_str(), c.valid_to, e));
            }
            store.close_facts(&scope, branch, "load", &rows).await?;
            n_eps += chunk
                .iter()
                .map(|c| &c.episode_id)
                .filter(|id| closing.contains(&(name.clone(), (*id).clone())))
                .collect::<std::collections::HashSet<_>>()
                .len();
            n_closures += chunk.len();
            commits += 1;
        }
    }
    let closed = match n_closures {
        0 => String::new(),
        n => format!(" (and {n} closures)"),
    };
    let skipped = match (skip_eps, skip_facts, skip_closures) {
        (0, 0, 0) => String::new(),
        (e, f, 0) => format!("; skipped {e} episodes and {f} facts already committed"),
        (e, f, c) => {
            format!("; skipped {e} episodes, {f} facts and {c} closures already committed")
        }
    };
    Ok(format!(
        "loaded {n_eps} episodes and {n_facts} facts{closed} into {} scopes in {commits} commits, \
         {:.1}s{skipped}",
        by_scope.len(),
        t0.elapsed().as_secs_f64()
    ))
}

fn to_episode(e: &GenEpisode) -> crate::Episode {
    crate::Episode {
        episode_id: e.episode_id.clone(),
        source: e.source.clone(),
        occurred_at: e.occurred_at,
        acl: e.acl.clone(),
        about: e.about.clone(),
        index_text: e.index_text,
        body: e.body.clone().into_bytes(),
        recorded_at: None,
        document: e.document.clone(),
    }
}

#[derive(Debug, Default, Serialize)]
pub struct EvalReport {
    /// category -> (correct, total)
    pub by_category: BTreeMap<String, (usize, usize)>,
    pub failures: Vec<String>,
    pub p50_ms: f64,
    pub p95_ms: f64,
    /// Lexical search with the question's own text: is the gold fact in the top 10?
    pub search_recall_at_10: (usize, usize),
    pub search_p50_ms: f64,
    pub search_p95_ms: f64,
}

/// Answer every gold question through the fact-key API, as the question's principal, and
/// compare with ground truth. No language model is involved: questions carry their key path.
pub async fn eval(
    store: &crate::Store,
    dir: &std::path::Path,
    only_scope: Option<&str>,
) -> Result<EvalReport> {
    use crate::{MAIN, Principal, Scope};
    let mut questions: Vec<GenQuestion> = read_jsonl(&dir.join("questions.jsonl"))?;
    // With a partial load, ask only what the loaded scopes can answer.
    let wanted = scope_filter(dir, only_scope)?;
    questions.retain(|q| wanted.as_ref().is_none_or(|w| w.contains(&q.scope)));
    let mut report = EvalReport::default();
    let mut ms = vec![];
    let org = Scope::Org;
    for q in &questions {
        let groups: Vec<&str> = q.principal_groups.iter().map(String::as_str).collect();
        let who = Principal::new(&q.principal_user, &groups);
        let scope = Scope::parse(&q.scope)?;
        // `as_of = None` means "current": everything still open.
        let at = Some(q.as_of.unwrap_or(i64::MAX));
        let t = std::time::Instant::now();
        let got = if q.path.first().map(String::as_str) == Some("count_by_value") {
            let members = store
                .lookup_value(&org, MAIN, &who, &q.path[1], &q.entity, at)
                .await?;
            let distinct: std::collections::HashSet<_> =
                members.iter().map(|f| f.entity.clone()).collect();
            distinct.len().to_string()
        } else {
            let mut cur = Some(q.entity.clone());
            for hop in &q.path {
                let Some(e) = cur.take() else { break };
                let mut found = store.lookup(&scope, MAIN, &who, &e, hop, at).await?;
                if found.is_empty() && scope != org {
                    found = store.lookup(&org, MAIN, &who, &e, hop, at).await?;
                }
                cur = found
                    .into_iter()
                    .max_by_key(|f| f.valid_from)
                    .map(|f| f.value);
            }
            cur.unwrap_or_else(|| "WITHHOLD".into())
        };
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        // Control: the owner of a withheld fact must see it, or "WITHHOLD" proves nothing.
        if q.category == "permission" {
            let owner = Principal::new(q.scope.trim_start_matches("user:"), &[]);
            let seen = store
                .lookup(&scope, MAIN, &owner, &q.entity, &q.path[0], at)
                .await?;
            let c = report
                .by_category
                .entry("permission_owner_control".into())
                .or_default();
            c.1 += 1;
            if seen.is_empty() {
                report
                    .failures
                    .push(format!("{}: owner cannot see their own fact", q.id));
            } else {
                c.0 += 1;
            }
        }
        let e = report.by_category.entry(q.category.clone()).or_default();
        e.1 += 1;
        if got == q.answer {
            e.0 += 1;
        } else if report.failures.len() < 20 {
            report.failures.push(format!(
                "{} {}: got {got:?}, want {:?}",
                q.id, q.question, q.answer
            ));
        }
    }
    let pct = |v: &mut Vec<f64>| -> (f64, f64) {
        v.sort_by(f64::total_cmp);
        if v.is_empty() {
            return (0.0, 0.0);
        }
        (v[v.len() / 2], v[(v.len() * 95).div_ceil(100) - 1])
    };
    (report.p50_ms, report.p95_ms) = pct(&mut ms);

    let mut sms = vec![];
    for q in questions
        .iter()
        .filter(|q| matches!(q.category.as_str(), "single_hop" | "as_of" | "freshness"))
    {
        let groups: Vec<&str> = q.principal_groups.iter().map(String::as_str).collect();
        let who = Principal::new(&q.principal_user, &groups);
        let at = Some(q.as_of.unwrap_or(i64::MAX));
        let t = std::time::Instant::now();
        let hits = crate::search::search(
            store,
            &Scope::parse(&q.scope)?,
            MAIN,
            &who,
            &q.question,
            10,
            at,
        )
        .await?;
        sms.push(t.elapsed().as_secs_f64() * 1000.0);
        report.search_recall_at_10.1 += 1;
        if hits.iter().any(|h| {
            h.fact.entity == q.entity && h.fact.predicate == q.path[0] && h.fact.value == q.answer
        }) {
            report.search_recall_at_10.0 += 1;
        } else if report.failures.len() < 40 {
            report.failures.push(format!(
                "search miss {} {:?}: top was {:?}",
                q.id,
                q.question,
                hits.first()
                    .map(|h| format!("{} {} {}", h.fact.entity, h.fact.predicate, h.fact.value))
            ));
        }
    }
    (report.search_p50_ms, report.search_p95_ms) = pct(&mut sms);
    Ok(report)
}

/// Gold traceability (M7.0): every fact and every gold answer cites episodes that exist.
pub fn verify(dir: &std::path::Path) -> Result<String> {
    let episodes: Vec<GenEpisode> = read_jsonl(&dir.join("episodes.jsonl"))?;
    let ids: std::collections::HashSet<&str> =
        episodes.iter().map(|e| e.episode_id.as_str()).collect();
    let facts: Vec<GenFact> = read_jsonl(&dir.join("facts.jsonl"))?;
    let questions: Vec<GenQuestion> = read_jsonl(&dir.join("questions.jsonl"))?;
    let dangling_facts = facts
        .iter()
        .filter(|f| f.source_episodes.iter().any(|e| !ids.contains(e.as_str())))
        .count();
    let dangling_questions = questions
        .iter()
        .filter(|q| q.cites.is_empty() || q.cites.iter().any(|e| !ids.contains(e.as_str())))
        .count();
    anyhow::ensure!(
        dangling_facts == 0 && dangling_questions == 0,
        "{dangling_facts} facts and {dangling_questions} questions cite missing episodes"
    );
    Ok(format!(
        "ok: {} facts and {} questions all cite existing episodes",
        facts.len(),
        questions.len()
    ))
}
