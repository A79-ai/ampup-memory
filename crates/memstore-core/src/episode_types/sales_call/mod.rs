//! A recorded call or meeting: a sales call, a team's meeting, a note-taker's notes of one. The body is the
//! transcript, as JSON from the recorder (turns with speaker, start and text, and the meeting's id, title and
//! participants, and its notes' summary) or as `[mm:ss] Speaker: text` lines. A meeting with notes and no
//! transcript is its summary alone. Facts cite a turn (its number, when it starts and who spoke) or a line of the
//! summary (a span of it, under its heading). The engine writes the meeting's name (its title) and time, who was
//! invited (from the participants), who attended (from who spoke) and the full name a participant listed only by
//! their address speaks under; the model writes the rest, from `catalog.json`.

use anyhow::{Context, Result};
use serde::Deserialize;

use super::participants::{self, Participant};
use super::{Cite, EngineFact, EpisodeType, Format, Header, Unit, speaker_lines};
use crate::Episode;

pub struct SalesCall;

/// The recorder's JSON: `turns` with `speaker` (or `speaker_id`), `start_ms` (or `start_time` in seconds) and
/// `text`; `meeting` (`meeting:<id>`), `title`, `participants` (`{id, name}`) and `summary` (the notes' summary
/// as Markdown, also read as `notes` or `summary_markdown`) when known. Turns, a summary, or both.
#[derive(Deserialize)]
struct Body {
    #[serde(default)]
    meeting: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    participants: Vec<Participant>,
    #[serde(default)]
    turns: Vec<Turn>,
    #[serde(default, alias = "notes", alias = "summary_markdown")]
    summary: Option<String>,
}

/// A summary's lines as units, numbered on from `first`: each line that isn't a heading, a list's marker left out,
/// citing its span of the summary under the heading above it.
fn summary_units(summary: &str, first: usize) -> Vec<Unit> {
    let mut out = vec![];
    let mut heading: Option<String> = None;
    let mut offset = 0;
    for raw in summary.split_inclusive('\n') {
        let start = offset;
        offset += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        let lead = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            heading = Some(trimmed.trim_start_matches('#').trim().to_string());
            continue;
        }
        let text = ["- ", "* ", "• "]
            .iter()
            .find_map(|m| trimmed.strip_prefix(m))
            .map_or(trimmed, str::trim_start);
        let at = start + lead + (trimmed.len() - text.len());
        out.push(Unit {
            index: first + out.len(),
            speaker: None,
            text: text.to_string(),
            cite: Cite::Span {
                offset: at,
                len: text.len(),
                heading: heading.clone(),
            },
        });
    }
    out
}

#[derive(Deserialize)]
struct Turn {
    #[serde(alias = "speaker_id")]
    speaker: String,
    #[serde(default)]
    start_ms: Option<u64>,
    #[serde(default)]
    start_time: Option<f64>,
    text: String,
}

impl EpisodeType for SalesCall {
    fn name(&self) -> &'static str {
        "sales_call"
    }
    fn format(&self) -> Format {
        Format::Transcript
    }
    fn source_prefixes(&self) -> &'static [&'static str] {
        &[
            "meeting:",
            "gong:",
            "zoom:",
            "recall:",
            "ampup:meeting:",
            "granola:",
            "fireflies:",
            "fathom:",
        ]
    }
    fn catalog_json(&self) -> &'static str {
        include_str!("catalog.json")
    }
    fn items(&self) -> &'static [&'static str] {
        &["commitment", "objection"]
    }
    fn engine_predicates(&self) -> &'static [&'static str] {
        &[
            crate::query::schema::NAME,
            crate::query::schema::ALIAS,
            "scheduled_at",
            "invited",
            "attended_by",
        ]
    }
    fn instructions(&self) -> &'static str {
        "A recorded call or meeting, as its transcript, its notes' summary, or both. On a sales call our reps \
         (internal) talk with a prospect (external). Write simple statements the meeting makes, each citing the \
         turn or summary line it is said in. Never judge status or progress: that is worked out later from many \
         episodes."
    }
    fn units(&self, ep: &Episode) -> Result<Vec<Unit>> {
        let text = String::from_utf8_lossy(&ep.body);
        if text.trim_start().starts_with('{') {
            let b: Body =
                serde_json::from_str(&text).context("sales_call body: not a transcript")?;
            let turns = b.turns.len();
            let summary = b
                .summary
                .as_deref()
                .map(|s| summary_units(s, turns))
                .unwrap_or_default();
            return Ok(b
                .turns
                .into_iter()
                .enumerate()
                .map(|(i, t)| {
                    let at_ms = t.start_ms.or(t.start_time.map(|s| (s * 1000.0) as u64));
                    Unit {
                        index: i,
                        speaker: Some(t.speaker.clone()),
                        text: t.text,
                        cite: Cite::Turn {
                            turn: i,
                            at_ms,
                            speaker: t.speaker,
                        },
                    }
                })
                .chain(summary)
                .collect());
        }
        Ok(speaker_lines(&text)
            .into_iter()
            .enumerate()
            .map(|(i, (at_ms, who, text))| {
                let speaker = who.unwrap_or_default();
                Unit {
                    index: i,
                    speaker: (!speaker.is_empty()).then(|| speaker.clone()),
                    text,
                    cite: Cite::Turn {
                        turn: i,
                        at_ms,
                        speaker,
                    },
                }
            })
            .collect())
    }
    /// The recorder's title, the meeting the call is the record of, and its participants by the names they are
    /// heard by ([`participants::heard`]).
    fn header(&self, ep: &Episode) -> Header {
        let text = String::from_utf8_lossy(&ep.body);
        let Some(b) = text
            .trim_start()
            .starts_with('{')
            .then(|| serde_json::from_str::<Body>(&text).ok())
            .flatten()
        else {
            return Header::default();
        };
        let units = self.units(ep).unwrap_or_default();
        let heard = participants::heard(&b.participants, &units);
        Header {
            title: b.title.filter(|t| !t.trim().is_empty()),
            subject: b.meeting,
            people: b
                .participants
                .iter()
                .zip(heard)
                .map(|(p, h)| (p.id.clone(), h.name))
                .collect(),
        }
    }
    /// Invited: every participant. Attended: a participant who speaks, cited at their first turn. Calendar
    /// participants are only invitees: a no-show is invited and never attends.
    /// Who speaks is [`participants::heard`], and a participant listed only by their address is also known by the
    /// full name they speak under (`_alias`). The meeting's name is the recorder's title and its `scheduled_at` the
    /// episode's time, so it is found by either.
    fn engine_facts(&self, ep: &Episode, units: &[Unit]) -> Result<Vec<EngineFact>> {
        let text = String::from_utf8_lossy(&ep.body);
        if !text.trim_start().starts_with('{') {
            return Ok(vec![]);
        }
        let b: Body = serde_json::from_str(&text).context("sales_call body: not a transcript")?;
        let Some(meeting) = b.meeting else {
            return Ok(vec![]);
        };
        let fact = |predicate: &str, value: String, cite: Cite| EngineFact {
            subject: meeting.clone(),
            predicate: predicate.into(),
            value,
            cite,
        };
        let field = |field: &str| Cite::Field {
            object: meeting.clone(),
            field: field.into(),
            changed_at: None,
        };
        let mut out = vec![];
        if let Some(title) = b.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            out.push(fact(
                crate::query::schema::NAME,
                title.into(),
                field("title"),
            ));
        }
        if let Some(at) = chrono::DateTime::from_timestamp_micros(ep.occurred_at) {
            let at = at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            out.push(fact("scheduled_at", at, field("occurred_at")));
        }
        for (p, h) in b
            .participants
            .iter()
            .zip(participants::heard(&b.participants, units))
        {
            out.push(fact("invited", p.id.clone(), field("participants")));
            let Some(u) = h.turn else { continue };
            out.push(fact("attended_by", p.id.clone(), u.cite.clone()));
            if let Some(alias) = h.alias {
                out.push(EngineFact {
                    subject: p.id.clone(),
                    predicate: crate::query::schema::ALIAS.into(),
                    value: alias,
                    cite: u.cite.clone(),
                });
            }
        }
        Ok(out)
    }
}
