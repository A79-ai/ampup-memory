//! A Slack thread. The body is its messages, as JSON (`messages` with `ts`, `user` or `author`, `text`, and
//! optionally `permalink`, and Slack's `bot_id`, `bot_profile`, `app_id` or `subtype` where a bot or an app posted
//! it) or as `author: text` lines. Facts cite a message. Follow-ups to a call land here
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
    #[serde(default)]
    bot_id: Option<serde_json::Value>,
    #[serde(default)]
    bot_profile: Option<serde_json::Value>,
    #[serde(default)]
    app_id: Option<serde_json::Value>,
    #[serde(default)]
    subtype: Option<String>,
}

impl Message {
    /// Whether a bot or an app posted it: Slack names its bot or app (`bot_id`, `bot_profile`, `app_id`), or its
    /// subtype is `bot_message`.
    fn by_bot(&self) -> bool {
        let named = |v: &Option<serde_json::Value>| match v {
            Some(serde_json::Value::String(s)) => !s.trim().is_empty(),
            Some(serde_json::Value::Object(o)) => !o.is_empty(),
            _ => false,
        };
        named(&self.bot_id)
            || named(&self.bot_profile)
            || named(&self.app_id)
            || self.subtype.as_deref() == Some("bot_message")
    }
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
                    cite: Cite::Message {
                        bot: m.by_bot(),
                        ts: m.ts,
                        author: m.author,
                        permalink: m.permalink,
                    },
                    text: m.text,
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
                        bot: false,
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
