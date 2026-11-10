//! A Slack thread. The body is its messages, as JSON (`messages` with `ts`, `user` or `author`, `text`, and
//! optionally `permalink`) or as `author: text` lines. Facts cite a message. Follow-ups to a call land here
//! (the deck was sent, the demo is booked): they are stated as actions taken, and aggregation decides what
//! they complete.

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{Cite, EpisodeType, Format, Header, Unit, first_line, speaker_lines};
use crate::Episode;

pub struct SlackThread;

#[derive(Deserialize)]
struct Body {
    messages: Vec<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    ts: String,
    #[serde(alias = "user")]
    author: String,
    text: String,
    #[serde(default)]
    permalink: Option<String>,
}

impl EpisodeType for SlackThread {
    fn name(&self) -> &'static str {
        "slack_thread"
    }
    fn format(&self) -> Format {
        Format::Messages
    }
    fn source_prefixes(&self) -> &'static [&'static str] {
        &["slack:"]
    }
    fn catalog_json(&self) -> &'static str {
        include_str!("catalog.json")
    }
    fn items(&self) -> &'static [&'static str] {
        &["commitment"]
    }
    fn instructions(&self) -> &'static str {
        "A Slack thread among our team, or with the prospect. Write simple statements the messages make, each \
         citing its message: what someone did, promised, decided or is blocked on. Never judge whether something \
         is done: that is worked out later from many episodes."
    }
    fn units(&self, ep: &Episode) -> Result<Vec<Unit>> {
        let text = String::from_utf8_lossy(&ep.body);
        if text.trim_start().starts_with('{') {
            let b: Body = serde_json::from_str(&text).context("slack_thread body: not a thread")?;
            return Ok(b
                .messages
                .into_iter()
                .enumerate()
                .map(|(i, m)| Unit {
                    index: i,
                    speaker: Some(m.author.clone()),
                    text: m.text,
                    cite: Cite::Message {
                        ts: m.ts,
                        author: m.author,
                        permalink: m.permalink,
                    },
                })
                .collect());
        }
        Ok(speaker_lines(&text)
            .into_iter()
            .enumerate()
            .map(|(i, (_, who, text))| {
                let author = who.unwrap_or_default();
                Unit {
                    index: i,
                    speaker: (!author.is_empty()).then(|| author.clone()),
                    text,
                    cite: Cite::Message {
                        ts: i.to_string(),
                        author,
                        permalink: None,
                    },
                }
            })
            .collect())
    }
    /// The thread's first message, as its title.
    fn header(&self, ep: &Episode) -> Header {
        Header {
            title: self
                .units(ep)
                .ok()
                .and_then(|u| u.into_iter().next())
                .and_then(|u| first_line(&u.text, 80)),
            ..Header::default()
        }
    }
}
