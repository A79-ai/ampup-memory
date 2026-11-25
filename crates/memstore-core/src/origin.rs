//! Where a fact's words came from: a system that recorded them (`observed`), a person who said or wrote them
//! (`stated`), or an LLM agent (`agent`). It is read from the episodes a fact cites, never stored: an episode's
//! source says it, and for a chat message a bot or an app posted, the message itself (its body, and the citation of
//! a fact taken from it).
//!
//! What an agent wrote is its own conclusion, not evidence. An agent that searches memory, misreads it and
//! remembers what it concluded would otherwise find its conclusion next time as a fact, restate it, and
//! remember it again, until one wrong guess reads as many confirmations. So a fact whose words only an agent
//! wrote is labelled where it is served, ranks a little below what was observed or stated, and never ends or
//! replaces a fact something else observed or stated ([`may_end`]): it may end another agent's.
//!
//! - **agent**: an agent's note (`remember_memory`, `/remember`, `close_memory`'s reason, a run's trace, a
//!   session's record), what the engine itself wrote from what it read (compiled skills, learned advice, a
//!   profile or a dream of someone's notes), and a chat message a bot or an app posted, where the message says so
//!   (Slack's `bot_id`, `bot_profile`, `app_id` or `bot_message` subtype: [`crate::episode_types::Cite::Message`]).
//!   A note is an agent's even when it relays what the person said: the server can't tell a relay from a
//!   conclusion, and what a person types reaches memory as their own words another way (below).
//! - **stated**: a call or meeting's transcript or notes, an email, a chat message, a note, what the server
//!   kept from what a person typed in a session (`keep`) or shared because several people said it, and a fact a
//!   person added themselves as an org admin (`add_fact`: Studio's "Add a fact" and "This is wrong") or stated
//!   in settling a conflict (`resolve_conflict` with a value), or text they pasted or uploaded in Studio's "Add to
//!   memory" (`/learn`), which they typed or chose, value and all, so it is their statement, not an agent's
//!   conclusion. Not a service key's: a key is a program, whatever it adds. A recording added there is a call's
//!   transcript ([`RECORDED`]): stated by its speakers, whoever added it.
//! - **observed**: a record a system keeps (a CRM change, an issue tracker's ticket, an error report, a pull
//!   request) and a document loaded; anything else a connector delivers.
//!
//! A fact citing several episodes takes the strongest origin among them (observed, then stated, then agent): a
//! fact an agent restated from a ticket is the ticket's.

use serde::{Deserialize, Serialize};

use crate::episode_types::Cite;
use crate::{Episode, Fact};

/// Where a fact's or an episode's words came from, weakest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Agent,
    Stated,
    Observed,
}

/// What follows a person in the source of what the server kept from what they typed (`keep`, or
/// `keep:<category>:<topic>`), and what follows the remembered-note prefix in the source of what it shared because
/// several people said it.
pub const KEPT: &str = "keep";
pub const SHARED: &str = "memstore:shared";
/// What follows the remembered-note prefix in the source of a fact the server wrote as the org's because enough
/// people's own memories showed it (`memstore::facts_share`), then the weakest origin among those it rests on
/// (`agent` or `stated`), its entity and its predicate: `mcp:remember_memory:memstore:shared-fact:agent:repo:acme/api:
/// command`. Its origin is that one: a fact learned from coding sessions (an agent's records) stays an agent's.
pub const SHARED_FACT: &str = "memstore:shared-fact";
/// What follows a person in the source of a fact they added themselves (`add_fact`), before the source they
/// named: `mcp:remember_memory:<them>:memstore:fact:<label>`. Under `memstore:`, which no note may take, so only
/// the server files it.
pub const ADDED: &str = "memstore:fact";
/// What follows a person in the source of text they added themselves in Studio's "Add to memory" (`/learn`: pasted
/// or uploaded), before what it was: `mcp:remember_memory:<them>:memstore:learn:paste:<first words>`. Under
/// `memstore:` too, so no agent's note passes for it.
pub const LEARNED: &str = "memstore:learn";
/// What follows a person in the source of a recording they added in Studio's "Add to memory" (`/learn`), its
/// transcript filed as a call: `mcp:remember_memory:<them>:memstore:recording:<digest>`. Not under
/// [`LEARNED`]: what its speakers said, not the statement of whoever added it, so it is stated as any call's
/// transcript is, and its speakers, not that person, said it ([`sayer`]). Under `memstore:` too, so only the server
/// files it.
pub const RECORDED: &str = "memstore:recording";

impl Origin {
    /// Its name, as JSON answers carry it.
    pub fn name(self) -> &'static str {
        match self {
            Origin::Agent => "agent",
            Origin::Stated => "stated",
            Origin::Observed => "observed",
        }
    }

    /// The origin of an episode from `source`, as the source alone says.
    pub fn of_source(source: &str) -> Origin {
        if let Some(rest) = remembered(source) {
            // A recording's transcript: what its speakers said, whoever added it.
            if recorded(rest) {
                return Origin::Stated;
            }
            let kept = rest == KEPT || rest.strip_prefix(KEPT).is_some_and(|r| r.starts_with(':'));
            let after = source
                .strip_prefix(crate::REMEMBER_SOURCE)
                .and_then(|s| s.strip_prefix(':'));
            // A fact shared from people's own memories is stated only where each it rests on was.
            let shared = after == Some(SHARED)
                || after
                    .and_then(|s| s.strip_prefix(SHARED_FACT))
                    .is_some_and(|r| r.starts_with(":stated:"));
            let added = added(rest) && !author_is_key(source);
            return match kept || shared || added {
                true => Origin::Stated,
                false => Origin::Agent,
            };
        }
        if crate::episode_types::engine_source(source) {
            return Origin::Agent;
        }
        match crate::episode_types::name_of_source(source) {
            Some("sales_call" | "email" | "slack_thread") => Origin::Stated,
            Some("doc") if source.starts_with("note:") => Origin::Stated,
            _ => Origin::Observed,
        }
    }

    /// The origin of an episode: its source's, unless a bot or an app posted every message in it.
    pub fn of_episode(ep: &Episode) -> Origin {
        Self::of_cited(ep, None)
    }

    /// The origin of what a fact citing `cite` (its `cite` JSON) takes from `ep`: an agent's where the message it
    /// cites was a bot's, else the episode's ([`Origin::of_episode`]).
    pub fn of_cited(ep: &Episode, cite: Option<&str>) -> Origin {
        let o = Self::of_source(&ep.source);
        if o != Origin::Stated {
            return o;
        }
        if cite.is_some_and(by_bot) {
            return Origin::Agent;
        }
        let messages: Vec<(String, bool)> = crate::episode_types::of(ep)
            .and_then(|t| t.units(ep).ok())
            .into_iter()
            .flatten()
            .filter_map(|u| match u.cite {
                Cite::Message { ts, bot, .. } => Some((ts, bot)),
                _ => None,
            })
            .collect();
        let ts = cite
            .and_then(|c| serde_json::from_str::<Cite>(c).ok())
            .and_then(|c| match c {
                Cite::Message { ts, .. } => Some(ts),
                _ => None,
            });
        let bots = match ts.and_then(|ts| messages.iter().find(|(t, _)| *t == ts)) {
            Some((_, bot)) => *bot,
            None => !messages.is_empty() && messages.iter().all(|(_, bot)| *bot),
        };
        match bots {
            true => Origin::Agent,
            false => o,
        }
    }

    /// The short label a read serves beside a fact of this origin, if it takes one: `an agent concluded, <day>`.
    pub fn note(self, valid_from: i64) -> Option<String> {
        (self == Origin::Agent)
            .then(|| format!("an agent concluded, {}", crate::freshness::day(valid_from)))
    }
}

/// What follows the author in a `remember_memory` episode's source, if it is one: a service key's author is
/// `key:<org>/<name>`, anyone else's runs to the next `:`.
fn remembered(source: &str) -> Option<&str> {
    let rest = source
        .strip_prefix(crate::REMEMBER_SOURCE)?
        .strip_prefix(':')?;
    let skip = if rest.starts_with("key:") { 4 } else { 0 };
    let end = skip + rest[skip..].find(':')?;
    Some(&rest[end + 1..])
}

/// Whether `rest`, what follows a person in a source, is what they added themselves in Studio: a fact ([`ADDED`])
/// or text they pasted or uploaded ([`LEARNED`]).
pub fn added(rest: &str) -> bool {
    under(rest, ADDED) || learned(rest)
}

/// Whether `rest`, what follows a person in a source, is text they pasted or uploaded in Studio ([`LEARNED`]).
pub fn learned(rest: &str) -> bool {
    under(rest, LEARNED)
}

/// Whether `rest`, what follows a person in a source, is a recording they added in Studio ([`RECORDED`]).
pub fn recorded(rest: &str) -> bool {
    under(rest, RECORDED)
}

/// Whether `source` is a recording someone added in Studio ([`RECORDED`]): a call, though filed under their name.
pub fn recording(source: &str) -> bool {
    remembered(source).is_some_and(recorded)
}

/// Whether `rest` is `tail`, or starts with it and a `:`.
fn under(rest: &str, tail: &str) -> bool {
    rest.strip_prefix(tail)
        .is_some_and(|r| r.is_empty() || r.starts_with(':'))
}

/// Whether the author of a `remember_memory` episode from `source` is a service key (`key:<org>/<name>`).
fn author_is_key(source: &str) -> bool {
    source
        .strip_prefix(crate::REMEMBER_SOURCE)
        .and_then(|s| s.strip_prefix(':'))
        .is_some_and(|s| s.starts_with("key:"))
}

/// Who said what an episode from `source` holds, as one source is told from another where who supports a fact is
/// counted ([`crate::corroboration`]): a note's author (each note they remember is theirs, whatever they label it),
/// else the source itself (a ticket, a thread, a call, a document). A recording someone added is its own
/// (`memstore:recording:<digest>`), whoever added it: the same media added by two people is one meeting, one sayer.
pub fn sayer(source: &str) -> &str {
    match remembered(source) {
        Some(rest) if recorded(rest) => rest,
        Some(rest) => &source[..source.len() - rest.len() - 1],
        None => source,
    }
}

/// [`sayer`] as a number, as [`crate::Vis::episode_sayer`] holds it: the same in every state of this process.
pub fn sayer_id(source: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sayer(source).hash(&mut h);
    h.finish()
}

/// Whether a citation (as a fact's `cite` holds it) is of a message a bot or an app posted.
pub(crate) fn by_bot(cite: &str) -> bool {
    matches!(
        serde_json::from_str::<Cite>(cite),
        Ok(Cite::Message { bot: true, .. })
    )
}

/// A fact's origin from those of the episodes it cites (`of_episode` gives each, None for one not known): the
/// strongest. The episode a fact's citation points into is an agent's where the message cited was a bot's. None
/// when none is known.
pub fn of_fact(f: &Fact, of_episode: impl Fn(&str) -> Option<Origin>) -> Option<Origin> {
    let bot = f.cite.as_deref().is_some_and(by_bot);
    f.source_episodes
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match i == 0 && bot {
            true => Some(Origin::Agent),
            false => of_episode(e),
        })
        .max()
}

/// Whether a fact of origin `by` may end or replace one of origin `old` (None: not known): an agent's only another
/// agent's. Anything else may end anything.
pub fn may_end(by: Origin, old: Option<Origin>) -> bool {
    by != Origin::Agent || old == Some(Origin::Agent)
}
