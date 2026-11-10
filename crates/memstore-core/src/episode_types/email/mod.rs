//! An email. The body is the message, as JSON (`message_id`, `from`, `sent_at`, `subject`, `body`) or as text
//! with `From:`, `Date:` and `Message-ID:` headers above a blank line. Facts cite a paragraph of the body. A
//! `Timezone:` header (`timezone` in JSON), an IANA name, says what zone the writer's times are in.

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{Cite, EpisodeType, Format, Header, Unit, first_line};
use crate::Episode;

pub struct Email;

#[derive(Deserialize)]
struct Body {
    #[serde(default)]
    message_id: String,
    from: String,
    #[serde(default)]
    sent_at: Option<String>,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    timezone: Option<String>,
    body: String,
}

/// The message: its JSON, or its headers and the text below them.
fn parse(ep: &Episode) -> Result<Body> {
    let text = String::from_utf8_lossy(&ep.body);
    if text.trim_start().starts_with('{') {
        return serde_json::from_str::<Body>(&text).context("email body: not a message");
    }
    let (head, body) = text.split_once("\n\n").unwrap_or(("", &text));
    let header = |name: &str| {
        head.lines()
            .find_map(|l| {
                l.split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            })
            .map(|(_, v)| v.trim().to_string())
    };
    Ok(Body {
        message_id: header("Message-ID").unwrap_or_default(),
        from: header("From").unwrap_or_default(),
        sent_at: header("Date"),
        subject: header("Subject").unwrap_or_default(),
        timezone: header("Timezone"),
        body: body.to_string(),
    })
}

impl EpisodeType for Email {
    fn name(&self) -> &'static str {
        "email"
    }
    fn format(&self) -> Format {
        Format::Email
    }
    fn source_prefixes(&self) -> &'static [&'static str] {
        &["gmail:", "outlook:", "email:"]
    }
    fn catalog_json(&self) -> &'static str {
        include_str!("catalog.json")
    }
    fn items(&self) -> &'static [&'static str] {
        &["commitment", "meeting_plan"]
    }
    fn instructions(&self) -> &'static str {
        "An email between our team and a prospect. Write simple statements it makes, each citing its paragraph: \
         what was sent, proposed, priced or promised. Never judge whether something is done or agreed: that is \
         worked out later from many episodes."
    }
    fn units(&self, ep: &Episode) -> Result<Vec<Unit>> {
        let b = parse(ep)?;
        let mut paragraphs: Vec<String> = vec![];
        if !b.subject.is_empty() {
            paragraphs.push(format!("Subject: {}", b.subject));
        }
        paragraphs.extend(
            b.body
                .split("\n\n")
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty()),
        );
        Ok(paragraphs
            .into_iter()
            .enumerate()
            .map(|(i, p)| Unit {
                index: i,
                speaker: Some(b.from.clone()),
                text: p,
                cite: Cite::Email {
                    message_id: b.message_id.clone(),
                    from: b.from.clone(),
                    sent_at: b.sent_at.clone(),
                    paragraph: i,
                },
            })
            .collect())
    }
    /// The `Timezone:` hint, else the offset the `Date:` header carries (RFC 3339 or RFC 2822). A `Date:` in
    /// UTC says nothing about the writer: CRMs and mail servers stamp every message in UTC.
    fn time_zone(&self, ep: &Episode) -> Option<String> {
        let b = parse(ep).ok()?;
        if let Some(tz) = b.timezone.filter(|t| !t.trim().is_empty()) {
            return Some(tz.trim().to_string());
        }
        let date = b.sent_at?;
        let t = chrono::DateTime::parse_from_rfc3339(date.trim())
            .or_else(|_| chrono::DateTime::parse_from_rfc2822(date.trim()))
            .ok()?;
        (t.offset().local_minus_utc() != 0).then(|| t.offset().to_string())
    }
    /// The subject, else the message's first line.
    fn header(&self, ep: &Episode) -> Header {
        let Ok(b) = parse(ep) else {
            return Header::default();
        };
        let subject = b.subject.trim();
        Header {
            title: match subject.is_empty() {
                true => first_line(&b.body, 80),
                false => Some(subject.to_string()),
            },
            ..Header::default()
        }
    }
}
