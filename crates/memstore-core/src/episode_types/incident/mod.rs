//! An issue from an error tracker: one error, grouped across its occurrences. The body is Markdown laid out as
//!
//! ```text
//! # <issue title>
//! Project: <project> · Level: <level> · Status: <unresolved|resolved|ignored>
//! Culprit: <culprit>
//! First seen: <RFC 3339> · Last seen: <RFC 3339> · Events: <n> · Users: <n>
//! Release: first <version|-> · last <version|->
//! Link: <permalink>
//!
//! ## Error
//! <exception type>: <exception value>
//!
//! ## Stack
//! <path>:<line> in <function>          (top frames, in-app first)
//!
//! ## Activity
//! <RFC 3339> <actor>: <what happened: resolved in <release> | regressed | assigned to <x> | commented "<text>">
//! ```
//!
//! Each header fact (the `·`-separated `Key: value`s), and each line under a section, is a unit a fact cites: a
//! [`Cite::Span`] of the body, under its section's heading (the title, for the header). A body laid out otherwise
//! still reads, line by line. The issue is `incident:<source after its prefix>` (`sentry:acme/api/issues/42` is
//! `incident:acme/api/issues/42`). The engine writes what the header states (its name, status, level, project,
//! culprit, when it was first and last seen, how many events and users, its first and last release, its link);
//! the model writes the rest, from `catalog.json`: what fails and where, who it affects, what caused, fixed or
//! resolved it and in which release, when it regressed, who owns it.

use anyhow::Result;

use super::{Cite, EngineFact, EpisodeType, Format, Header, Unit};
use crate::Episode;

pub struct Incident;

/// The type of the entity an issue is.
pub const TYPE: &str = "incident";

/// The issue an episode from `source` is the record of: `incident:<source after its prefix>`.
pub fn subject(source: &str) -> Option<String> {
    let (_, rest) = source.split_once(':')?;
    let rest = rest.trim();
    (!rest.is_empty()).then(|| format!("{TYPE}:{rest}"))
}

/// A line's piece: its text, where it starts in the body, and the header fact it states (its key, lower case,
/// words joined by `_`: `first_seen`), if it is one.
struct Piece<'a> {
    text: &'a str,
    offset: usize,
    key: Option<String>,
    heading: Option<String>,
    section: Section,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Title,
    Header,
    /// Under a `## ` heading.
    Body,
}

/// `Key: value`, the key a few words: what a header fact reads as.
fn key_of(part: &str) -> Option<String> {
    let (k, v) = part.split_once(": ")?;
    let k = k.trim();
    (!k.is_empty()
        && !v.trim().is_empty()
        && k.len() <= 24
        && k.chars()
            .all(|c| c.is_alphanumeric() || c == ' ' || c == '_'))
    .then(|| {
        k.to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("_")
    })
}

/// The body's pieces, in order: the title, each header fact, each line under a section.
fn pieces(text: &str) -> Vec<Piece<'_>> {
    let mut out: Vec<Piece> = vec![];
    let mut heading: Option<String> = None;
    let mut in_body = false;
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let start = offset;
        offset += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        let lead = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("## ") {
            heading = Some(h.trim().to_string());
            in_body = true;
            continue;
        }
        if !in_body && heading.is_none() && trimmed.starts_with("# ") {
            let title = trimmed.trim_start_matches('#').trim();
            heading = Some(title.to_string());
            out.push(Piece {
                text: title,
                offset: start + lead + (trimmed.len() - title.len()),
                key: None,
                heading: heading.clone(),
                section: Section::Title,
            });
            continue;
        }
        if in_body {
            out.push(Piece {
                text: trimmed,
                offset: start + lead,
                key: None,
                heading: heading.clone(),
                section: Section::Body,
            });
            continue;
        }
        // A header line: its `·`-separated facts, each its own piece. A part that isn't `Key: value` continues
        // the one before it (`Release: first 1.2 · last 1.4`).
        let mut at = start + lead;
        let mut parts: Vec<(usize, usize)> = vec![];
        for (i, part) in trimmed.split(" · ").enumerate() {
            if i > 0 {
                at += " · ".len();
            }
            let begin = at;
            at += part.len();
            match parts.last_mut() {
                Some(last) if key_of(part).is_none() => last.1 = at,
                _ => parts.push((begin, at)),
            }
        }
        for (b, e) in parts {
            let text = &text[b..e];
            out.push(Piece {
                text,
                offset: b,
                key: key_of(text),
                heading: heading.clone(),
                section: Section::Header,
            });
        }
    }
    out
}

/// `first <v> · last <v>` as (first, last), a `-` or nothing as none.
fn releases(v: &str) -> (Option<&str>, Option<&str>) {
    let mut first = None;
    let mut last = None;
    for part in v.split(" · ") {
        let part = part.trim();
        let known = |s: &'static str| {
            part.strip_prefix(s)
                .map(str::trim)
                .filter(|v| !v.is_empty() && *v != "-")
        };
        if let Some(v) = known("first ") {
            first = Some(v);
        } else if let Some(v) = known("last ") {
            last = Some(v);
        }
    }
    (first, last)
}

impl EpisodeType for Incident {
    fn name(&self) -> &'static str {
        "incident"
    }
    fn format(&self) -> Format {
        Format::Text
    }
    /// An error tracker's issue: `sentry:<org>/<project>/issues/<id>`.
    fn source_prefixes(&self) -> &'static [&'static str] {
        &["sentry:"]
    }
    fn catalog_json(&self) -> &'static str {
        include_str!("catalog.json")
    }
    fn engine_predicates(&self) -> &'static [&'static str] {
        &[
            crate::query::schema::NAME,
            "status",
            "level",
            "project",
            "culprit",
            "first_seen",
            "last_seen",
            "events",
            "users",
            "first_release",
            "last_release",
            "link",
        ]
    }
    fn instructions(&self) -> &'static str {
        "An issue from an error tracker: one error grouped across its occurrences, with its header, the error, its \
         top stack frames and the issue's activity. Write simple statements it makes, each citing its line: what \
         fails and where (file and function), who or what it affects, what caused it, what fixed or resolved it \
         and in which release, when it regressed, and who owns it. Its status, when it was first and last seen, \
         how often it happened and its releases are recorded already: never write them."
    }
    fn units(&self, ep: &Episode) -> Result<Vec<Unit>> {
        let text = String::from_utf8_lossy(&ep.body);
        Ok(pieces(&text)
            .into_iter()
            .enumerate()
            .map(|(i, p)| Unit {
                index: i,
                // An activity line names its actor itself (`<time> <actor>: <what happened>`): a speaker would
                // be shown before it again, and the model would quote that.
                speaker: None,
                text: p.text.to_string(),
                cite: Cite::Span {
                    offset: p.offset,
                    len: p.text.len(),
                    heading: p.heading,
                },
            })
            .collect())
    }
    /// Its title, and the issue it is the record of.
    fn header(&self, ep: &Episode) -> Header {
        let text = String::from_utf8_lossy(&ep.body);
        Header {
            title: pieces(&text)
                .into_iter()
                .find(|p| p.section == Section::Title)
                .map(|p| p.text.to_string()),
            subject: subject(&ep.source),
            people: vec![],
        }
    }
    /// What the header states, on the issue, each citing its fact: its name (the title), status, level, project,
    /// culprit, first and last seen, events, users, first and last release, and link. A `-` states nothing.
    fn engine_facts(&self, ep: &Episode, units: &[Unit]) -> Result<Vec<EngineFact>> {
        let Some(issue) = subject(&ep.source) else {
            return Ok(vec![]);
        };
        let text = String::from_utf8_lossy(&ep.body);
        let mut out = vec![];
        let mut fact = |predicate: &str, value: &str, u: &Unit| {
            let value = value.trim();
            if !value.is_empty() && value != "-" {
                out.push(EngineFact {
                    subject: issue.clone(),
                    predicate: predicate.into(),
                    value: value.into(),
                    cite: u.cite.clone(),
                });
            }
        };
        for (p, u) in pieces(&text).iter().zip(units) {
            if p.section == Section::Title {
                fact(crate::query::schema::NAME, p.text, u);
                continue;
            }
            let (Section::Header, Some(key)) = (p.section, p.key.as_deref()) else {
                continue;
            };
            let value = p.text.split_once(": ").map_or("", |(_, v)| v);
            match key {
                "status" | "level" => fact(key, &value.to_lowercase(), u),
                "project" | "culprit" | "first_seen" | "last_seen" | "link" => fact(key, value, u),
                "events" | "users" => fact(key, value.replace(',', "").as_str(), u),
                "release" | "releases" => {
                    let (first, last) = releases(value);
                    if let Some(v) = first {
                        fact("first_release", v, u);
                    }
                    if let Some(v) = last {
                        fact("last_release", v, u);
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }
}
