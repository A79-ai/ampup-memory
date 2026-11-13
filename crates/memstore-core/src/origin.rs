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
//! - **stated**: a call or meeting's transcript or notes, an email, a chat message, a note, and what the server
//!   kept from what a person typed in a session (`keep`) or shared because several people said it.
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
            let kept = rest == KEPT || rest.strip_prefix(KEPT).is_some_and(|r| r.starts_with(':'));
            let shared = source
                .strip_prefix(crate::REMEMBER_SOURCE)
                .and_then(|s| s.strip_prefix(':'))
                == Some(SHARED);
            return match kept || shared {
                true => Origin::Stated,
                false => Origin::Agent,
            };
        }
        if crate::episode_types::engine_source(source) {
            return Origin::Agent;
        }
        match crate::episode_types::name_of_source(source) {
            Some("sales_call" | "email" | "slack_thread") => Origin::Stated,
            Some("doc") if ["granola:", "note:"].iter().any(|p| source.starts_with(p)) => {
                Origin::Stated
            }
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

/// Who said what an episode from `source` holds, as one source is told from another where who supports a fact is
/// counted ([`crate::corroboration`]): a note's author (each note they remember is theirs, whatever they label it),
/// else the source itself (a ticket, a thread, a call, a document).
pub fn sayer(source: &str) -> &str {
    match remembered(source) {
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
