//! Running text: an agent's note, a doc, a pull request. It claims its own sources like every other type (it
//! is no catch-all: a source no type claims is refused). Facts cite a paragraph (its byte offset and the
//! heading above it). Its catalog is the built-in `engineering` schema, the nine kinds the extractor has
//! always written, so notes and docs are read as before.

use anyhow::Result;

use super::{Cite, EpisodeType, Format, Header, Unit, first_line};
use crate::Episode;

pub struct Doc;

impl EpisodeType for Doc {
    fn name(&self) -> &'static str {
        "doc"
    }
    fn format(&self) -> Format {
        Format::Text
    }
    /// Running text, by where it comes from: an agent's note (`/remember`, MCP `remember_memory` and
    /// `close_memory`, all signed `mcp:remember_memory:<who>:…`), a GitHub issue or pull request, a Linear
    /// issue (`linear:<workspace>/<KEY-123>`: title, metadata, description, comments and linked pull requests
    /// as markdown), a Sentry issue (`sentry:https://<org>.sentry.io/issues/<id>/`: title, status, culprit, stack
    /// frames and the pull requests that fixed it, as markdown), a Granola meeting note (summary and transcript as
    /// markdown), a document, a note.
    fn source_prefixes(&self) -> &'static [&'static str] {
        &[
            "mcp:remember_memory:",
            "github:",
            "linear:",
            "sentry:",
            "granola:",
            "doc:",
            "note:",
        ]
    }
    fn catalog_json(&self) -> &'static str {
        include_str!("../../../../../schemas/engineering.json")
    }
    fn instructions(&self) -> &'static str {
        "Running text: a note, a doc or a pull request. Write durable facts a teammate would want remembered, each \
         citing the paragraph it comes from."
    }
    fn units(&self, ep: &Episode) -> Result<Vec<Unit>> {
        let text = String::from_utf8_lossy(&ep.body);
        let mut out = vec![];
        let mut heading: Option<String> = None;
        let mut offset = 0;
        for para in text.split("\n\n") {
            let start = offset;
            offset += para.len() + 2;
            let p = para.trim();
            if p.is_empty() {
                continue;
            }
            if let Some(h) = p.lines().next().filter(|l| l.starts_with('#')) {
                heading = Some(h.trim_start_matches('#').trim().to_string());
            }
            out.push(Unit {
                index: out.len(),
                speaker: None,
                text: p.to_string(),
                cite: Cite::Span {
                    offset: start,
                    len: para.len(),
                    heading: heading.clone(),
                },
            });
        }
        Ok(out)
    }
    /// The offset of the date a note opens with, in brackets (`[2023-07-05T09:30:00-04:00] …`), as a log line,
    /// a chat export or a dated note corpus writes it: the writer's clock. One in UTC says nothing about the
    /// writer (a machine stamps in UTC), as in an email's `Date:`.
    fn time_zone(&self, ep: &Episode) -> Option<String> {
        let head = String::from_utf8_lossy(&ep.body[..64.min(ep.body.len())]);
        let (stamp, _) = head.trim_start().strip_prefix('[')?.split_once(']')?;
        let t = chrono::DateTime::parse_from_rfc3339(stamp.trim()).ok()?;
        (t.offset().local_minus_utc() != 0).then(|| t.offset().to_string())
    }
    /// Its first line, a heading's `#`s dropped.
    fn header(&self, ep: &Episode) -> Header {
        let text = String::from_utf8_lossy(&ep.body);
        Header {
            title: first_line(&text, 80).map(|l| l.trim_start_matches('#').trim().to_string()),
            ..Header::default()
        }
    }
}
