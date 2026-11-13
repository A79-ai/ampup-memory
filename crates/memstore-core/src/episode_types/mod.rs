//! Episode types: what kind of record an episode is (a sales call, a Slack thread, an email, a CRM change, a
//! document), and everything that depends on it. Each type lives in its own folder and implements
//! [`EpisodeType`]:
//!
//! - which episodes it claims (by source prefix);
//! - how its body is laid out ([`Format`]), split into numbered [`Unit`]s (turns, messages, paragraphs, field
//!   changes) that a fact cites ([`Cite`]): the moment in the call, the message, the field;
//! - its catalog: the predicates a model may extract from it, as a schema (`catalog.json` in its folder,
//!   checked like any schema), the item types it opens per episode (named by the engine `<type>:<episode>#<n>`),
//!   and the predicates the engine writes itself, never the model (who was invited, a CRM field's new value);
//! - what the model is told about it.
//!
//! Every type extracts onto shared subjects (`deal:hs-…`, `contact:…`) with a shared predicate vocabulary, so
//! facts from a call and from a Slack follow-up land on the same deal and aggregation can read them together.
//! Status is never extracted by any type: aggregation derives it.

use std::sync::LazyLock;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::Episode;
use crate::query::schema::Schema;

pub mod crm_update;
pub mod doc;
pub mod email;
pub mod participants;
pub mod sales_call;
pub mod slack_thread;

/// How an episode's body is laid out, which says what a citation points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// Speaker turns with times: a recorded call.
    Transcript,
    /// Messages with authors and timestamps: a chat thread.
    Messages,
    /// One message with headers: an email.
    Email,
    /// Field changes on a record: a CRM update.
    Record,
    /// Running text: a note, a doc, a pull request.
    Text,
}

/// Where in an episode a fact comes from: the key moment it cites.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Cite {
    /// A turn of a call: its number, when it starts (ms into the recording, if known), who spoke.
    Turn {
        turn: usize,
        at_ms: Option<u64>,
        speaker: String,
    },
    /// A chat message: its timestamp id, author and link, and whether a bot or an app posted it, as the message
    /// says ([`crate::origin`] reads a fact citing such a message as an agent's).
    Message {
        ts: String,
        author: String,
        permalink: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        bot: bool,
    },
    /// A paragraph of an email.
    Email {
        message_id: String,
        from: String,
        sent_at: Option<String>,
        paragraph: usize,
    },
    /// A field of a record.
    Field {
        object: String,
        field: String,
        changed_at: Option<String>,
    },
    /// A span of running text.
    Span {
        offset: usize,
        len: usize,
        heading: Option<String>,
    },
}

impl Cite {
    /// One line a person can follow back: `@03:49 Dana Reyes`, `msg 1724100000.12 U04ABC`.
    pub fn describe(&self) -> String {
        match self {
            Cite::Turn {
                turn,
                at_ms,
                speaker,
            } => match at_ms {
                Some(ms) => format!("@{:02}:{:02} {speaker}", ms / 60_000, ms / 1000 % 60),
                None => format!("turn {turn} {speaker}"),
            },
            Cite::Message { ts, author, .. } => format!("msg {ts} {author}"),
            Cite::Email {
                from, paragraph, ..
            } => format!("email from {from}, paragraph {paragraph}"),
            Cite::Field { object, field, .. } => format!("{object}.{field}"),
            Cite::Span {
                offset, heading, ..
            } => match heading {
                Some(h) => format!("“{h}” at {offset}"),
                None => format!("at {offset}"),
            },
        }
    }
}

/// A numbered piece of an episode the model reads and a fact cites.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    pub index: usize,
    /// Who said or wrote it, when the format has one.
    pub speaker: Option<String>,
    pub text: String,
    pub cite: Cite,
}

/// What an episode is, as a reader would name it: its title, the entity it is the record of (a call's
/// meeting, a CRM change's record), and the people in it by id and name. What search shows an episode as.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Header {
    pub title: Option<String>,
    pub subject: Option<String>,
    pub people: Vec<(String, String)>,
}

/// A fact the engine knows from an episode's own structure, with no model: who was invited, who spoke, the new
/// value of a CRM field. It carries its citation like any fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineFact {
    pub subject: String,
    pub predicate: String,
    pub value: String,
    pub cite: Cite,
}

/// One kind of episode. Implemented once per folder under `src/episode_types/`.
pub trait EpisodeType: Send + Sync {
    /// `sales_call`, `slack_thread`: the name catalogs and facts use.
    fn name(&self) -> &'static str;
    fn format(&self) -> Format;
    /// Sources of this type start with one of these.
    fn source_prefixes(&self) -> &'static [&'static str];
    /// Whether an episode is of this type. By default, by its source.
    fn claims(&self, ep: &Episode) -> bool {
        self.source_prefixes()
            .iter()
            .any(|p| ep.source.starts_with(p))
    }
    /// The catalog's JSON (a schema): what can be extracted from this type, and how each value is typed.
    fn catalog_json(&self) -> &'static str;
    /// Item types this episode type opens per episode (`commitment`): the model opens them under handles and
    /// the engine names them `<type>:<episode>#<n>`.
    fn items(&self) -> &'static [&'static str] {
        &[]
    }
    /// Predicates the engine writes from the episode's structure (see [`EpisodeType::engine_facts`]); they are
    /// never offered to the model.
    fn engine_predicates(&self) -> &'static [&'static str] {
        &[]
    }
    /// Whether a model reads this type at all. A CRM change is written field by field, with none.
    fn uses_model(&self) -> bool {
        true
    }
    /// What the model is told about this kind of episode, before the catalog.
    fn instructions(&self) -> &'static str;
    /// The body as numbered units the model reads and facts cite.
    fn units(&self, ep: &Episode) -> Result<Vec<Unit>>;
    /// Facts known from the episode's structure, with no model.
    fn engine_facts(&self, _ep: &Episode, _units: &[Unit]) -> Result<Vec<EngineFact>> {
        Ok(vec![])
    }
    /// The zone a clock time the text gives without one is in: an IANA name (`America/Los_Angeles`) or an
    /// offset (`-05:00`). None: UTC.
    fn time_zone(&self, _ep: &Episode) -> Option<String> {
        None
    }
    /// The episode's title, subject and people, as far as its body says them. By default, none of them.
    fn header(&self, _ep: &Episode) -> Header {
        Header::default()
    }
}

/// `text`'s first line, at most `n` characters: a title for a body that has no other.
fn first_line(text: &str, n: usize) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(match line.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    })
}

static ALL: LazyLock<Vec<&'static dyn EpisodeType>> = LazyLock::new(|| {
    vec![
        &sales_call::SalesCall,
        &slack_thread::SlackThread,
        &email::Email,
        &crm_update::CrmUpdate,
        &doc::Doc,
    ]
});

/// Every episode type. None is a catch-all: each claims its own sources.
pub fn all() -> &'static [&'static dyn EpisodeType] {
    &ALL
}

pub fn by_name(name: &str) -> Result<&'static dyn EpisodeType> {
    match all().iter().find(|t| t.name() == name) {
        Some(t) => Ok(*t),
        None => bail!(
            "no episode type {name:?}: one of {}",
            all()
                .iter()
                .map(|t| t.name())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The type that claims an episode's source by its prefix, if one does. What a caller writing episodes
/// (`/ingest`, `remember_memory`) must name ([`check`]).
pub fn claimed(ep: &Episode) -> Option<&'static dyn EpisodeType> {
    all().iter().find(|t| t.claims(ep)).copied()
}

/// The type an episode is: the one that claims its source, else a document. A corpus an operator loads
/// (`memstore load`) names its sources as it likes, such as τ²-bench's `kb:` or ORCA-bench's `file:`, and
/// its episodes are documents. None only for the engine's own documents ([`ENGINE_SOURCES`]).
pub fn of(ep: &Episode) -> Option<&'static dyn EpisodeType> {
    claimed(ep).or_else(|| (!engine_source(&ep.source)).then_some(&doc::Doc as &dyn EpisodeType))
}

/// The name of the type an episode from `source` is, as [`of`] decides it: the type claiming its prefix, else
/// `doc`; None for the engine's own documents.
pub fn name_of_source(source: &str) -> Option<&'static str> {
    all()
        .iter()
        .find(|t| t.source_prefixes().iter().any(|p| source.starts_with(p)))
        .map(|t| t.name())
        .or_else(|| (!engine_source(source)).then_some("doc"))
}

/// Sources the engine writes itself, never a caller: goals from before they were retired (`goal:`), compiled
/// skills (`compile:`), advice learned from runs (`advice:`) and a dream's record of each decision it made
/// (`dream:`), a merge's record of the conflicts it resolved (`merge:`), and reconcile's record of two facts that
/// disagree (`reconcile:`, [`crate::conflicts`]). They are the engine's documents about the memory, of no episode
/// type, and nothing extracts facts from them.
pub const ENGINE_SOURCES: [&str; 6] = [
    "goal:",
    "compile:",
    "advice:",
    "dream:",
    "merge:",
    crate::conflicts::SOURCE,
];

/// Whether `source` is one the engine writes itself ([`ENGINE_SOURCES`]).
pub fn engine_source(source: &str) -> bool {
    ENGINE_SOURCES.iter().any(|p| source.starts_with(p))
}

/// The episode types and the source prefixes each claims: `sales_call (meeting:, gong:, …); …`.
pub fn known() -> String {
    all()
        .iter()
        .map(|t| format!("{} ({})", t.name(), t.source_prefixes().join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Whether an episode may be written: a type claims its source, and its body reads as that type, in at least
/// one unit. Every path that writes episodes calls this before anything is committed. The error names the
/// source and either the types there are (with the prefixes each claims) or why the body doesn't read.
pub fn check(ep: &Episode) -> Result<&'static dyn EpisodeType> {
    let Some(t) = claimed(ep) else {
        bail!(
            "source {:?} is of no episode type: a source starts with a prefix its type claims: {}",
            ep.source,
            known()
        );
    };
    readable(ep, t)
}

/// Whether `ep`'s body reads as `t`, in at least one unit.
fn readable(ep: &Episode, t: &'static dyn EpisodeType) -> Result<&'static dyn EpisodeType> {
    let units = t
        .units(ep)
        .map_err(|e| anyhow::anyhow!("source {:?} is a {}, but {e:#}", ep.source, t.name()))?;
    anyhow::ensure!(
        !units.is_empty(),
        "source {:?} is a {}, but its body holds nothing a {} reads",
        ep.source,
        t.name(),
        t.name()
    );
    Ok(t)
}

/// [`check`] on each episode of a write, in order: the first refused, as `episodes[i]: why`.
pub fn check_all(episodes: &[Episode]) -> Result<()> {
    check_where(episodes, |_| true)
}

/// What the store requires of every commit of new episodes, whoever wrote them: each body reads as its type,
/// [`of`] it, so an unclaimed source (a loaded corpus's) is a document. The engine's own documents
/// ([`ENGINE_SOURCES`]) aside. The callers' own paths ([`check`]) also require a claimed source.
pub fn check_stored(episodes: &[Episode]) -> Result<()> {
    for (i, ep) in episodes
        .iter()
        .enumerate()
        .filter(|(_, ep)| !engine_source(&ep.source))
    {
        let t = of(ep).expect("only the engine's own documents are of no type");
        readable(ep, t).map_err(|e| anyhow::anyhow!("{REFUSED}episodes[{i}]: {e:#}"))?;
    }
    Ok(())
}

fn check_where(episodes: &[Episode], checked: impl Fn(&Episode) -> bool) -> Result<()> {
    for (i, ep) in episodes.iter().enumerate().filter(|(_, ep)| checked(ep)) {
        check(ep).map_err(|e| anyhow::anyhow!("{REFUSED}episodes[{i}]: {e:#}"))?;
    }
    Ok(())
}

/// What a refusal by [`check_all`] starts with, so a server can answer it as the caller's mistake (400).
pub const REFUSED: &str = "untyped episode: ";

/// A type's catalog, parsed and checked like any schema; its item and engine predicates must be in it (or be a
/// name, which every type has).
pub fn catalog(t: &dyn EpisodeType) -> Result<Schema> {
    let s = Schema::parse(t.catalog_json())
        .map_err(|e| anyhow::anyhow!("catalog of episode type {}: {e:#}", t.name()))?;
    for item in t.items() {
        anyhow::ensure!(
            s.types.contains_key(*item),
            "catalog of {}: item type {item} is not declared",
            t.name()
        );
    }
    for p in t.engine_predicates() {
        anyhow::ensure!(
            [crate::query::schema::NAME, crate::query::schema::ALIAS].contains(p)
                || s.types.values().any(|d| d.predicates.contains_key(*p)),
            "catalog of {}: engine predicate {p} is not declared",
            t.name()
        );
    }
    Ok(s)
}

/// The units as the model reads them: one numbered line each.
pub fn render(units: &[Unit]) -> String {
    units
        .iter()
        .map(|u| match &u.speaker {
            Some(s) => format!("[{}] {s}: {}", u.index, u.text),
            None => format!("[{}] {}", u.index, u.text),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A text's words, lower case, one space apart: what two quotes are compared on.
pub fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&w.to_lowercase());
    }
    out
}

/// The unit a quote comes from: the hinted unit if it holds the quote, else the first that does. Quotes are
/// compared on their words, ignoring case and punctuation; one shorter than 12 characters cites nothing.
pub fn locate<'a>(units: &'a [Unit], quote: &str, hint: Option<usize>) -> Option<&'a Unit> {
    let q = squash(quote);
    if quote.trim().len() < 12 || q.is_empty() {
        return None;
    }
    let holds = |u: &&Unit| squash(&u.text).contains(&q);
    hint.and_then(|h| units.iter().find(|u| u.index == h).filter(holds))
        .or_else(|| units.iter().find(holds))
}

/// `[mm:ss] Speaker: text`, `[h:mm:ss] Speaker: text` or `Speaker: text` lines: (ms, speaker, text).
pub fn speaker_lines(body: &str) -> Vec<(Option<u64>, Option<String>, String)> {
    let mut out = vec![];
    for line in body.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (at, rest) = match line.strip_prefix('[').and_then(|l| l.split_once(']')) {
            Some((t, rest)) => (clock_ms(t), rest.trim()),
            None => (None, line),
        };
        match rest.split_once(": ") {
            Some((who, text)) if !who.is_empty() && who.len() <= 60 => {
                out.push((at, Some(who.to_string()), text.to_string()))
            }
            _ => out.push((at, None, rest.to_string())),
        }
    }
    out
}

fn clock_ms(t: &str) -> Option<u64> {
    let parts: Vec<u64> = t
        .split(':')
        .map(|p| p.trim().parse().ok())
        .collect::<Option<_>>()?;
    let secs = match parts.as_slice() {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some(secs * 1000)
}
