//! **Slack.** A channel's messages and its threads' replies, one episode a message, in the format a hosted
//! deployment's Slack connector writes: source `slack:CHANNEL_ID:#name:TS`, about `channel:CHANNEL_ID`,
//! `thread:CHANNEL_ID:THREAD_TS` for a thread's messages and `slack_user:USER_ID`, and a `slack_thread` body (JSON:
//! the channel's name, the thread it's in, and the message with its `ts`, its author's name and its text, mentions
//! made readable). The token is a bot token from a Slack app made for one's own workspace, `SLACK_BOT_TOKEN` in the
//! server's environment ([`TOKEN_VAR`]). It is never written to a file or taken on the command line.
//!
//! **Which channel.** `sources add slack CHANNEL` takes a channel's name (without `#`) or its id; the server looks a
//! name up when it reads. A public channel the app isn't in it joins (`channels:join`); a private one it must be
//! invited to (`/invite @app`).
//!
//! **What's read.** From `--since` (default [`super::BACKFILL_DAYS`] days back) on, then what was posted after the
//! cursor in `DIR/sources-state.json`: [`SKEW`] before the last round started, so a message posted as a round read
//! (or on a clock a little off this machine's) is read by the next. A reply lands in its thread, not in the
//! channel's history, so each round reads the history [`LOOKBACK`] further back than that (floored at `--since`)
//! for threads whose latest reply is newer, and reads those threads' replies; a reply to a thread started longer
//! ago is missed, as the hosted connector misses it. What was posted in the [`SKEW`] before a round is sent again
//! by the next (the same episodes). Messages with a subtype (joins, topic changes, a legacy bot's posts) are left
//! out, but for a reply also sent to the channel, as are messages with no text.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{BATCH, Progress, Source, http, micros};

/// Where the server finds the bot token.
pub const TOKEN_VAR: &str = "SLACK_BOT_TOKEN";
/// A round has read everything posted until this long before it started: what's posted as it reads may be missed.
const SKEW: chrono::Duration = chrono::Duration::minutes(5);
/// How far before the window a round looks for threads that had replies since.
const LOOKBACK: chrono::Duration = chrono::Duration::days(14);
/// Pages of 200 a round reads of a channel's history: the window's new messages must fit, or the round fails.
const HISTORY_PAGES: u32 = 200;
/// Times a call waits out Slack's rate limit (`429`, `Retry-After`) before the round gives up until the next.
const WAITS: u32 = 5;
/// Slack errors that mean the token itself is no good, not one channel or a hiccup.
const AUTH_ERRORS: &[&str] = &[
    "invalid_auth",
    "not_authed",
    "account_inactive",
    "token_revoked",
    "token_expired",
    "team_disabled",
];

fn is_id(s: &str) -> bool {
    s.len() >= 9
        && (s.starts_with('C') || s.starts_with('G'))
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// `sources add slack CHANNEL`: a channel's name (`#` optional) or id.
pub fn source(target: &str, since: String) -> Result<Source> {
    let channel = target.trim().trim_start_matches('#');
    let name_ok = !channel.is_empty()
        && channel.chars().count() <= 80
        && channel
            .chars()
            .all(|c| (c.is_alphanumeric() && !c.is_uppercase()) || matches!(c, '-' | '_' | '.'));
    if !is_id(channel) && !name_ok {
        bail!(
            "{target:?} isn't a Slack channel's name (lowercase, as Slack shows it) or id (C0123ABCD)"
        );
    }
    Ok(Source::Slack {
        channel: channel.to_string(),
        since,
    })
}

/// A Slack Web API error: Slack answers `200` with `{"ok": false, "error": …}`.
#[derive(Debug)]
struct SlackError {
    method: &'static str,
    error: String,
    needed: Option<String>,
}

impl std::fmt::Display for SlackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let e = &self.error;
        if AUTH_ERRORS.contains(&e.as_str()) {
            return write!(
                f,
                "Slack refused {TOKEN_VAR} ({e}): install the app again and set its new Bot User OAuth Token"
            );
        }
        match (e.as_str(), &self.needed) {
            ("channel_not_found", _) => write!(
                f,
                "{}: no such channel, or a private one the app isn't in (`/invite @app` there)",
                self.method
            ),
            ("missing_scope", Some(n)) => write!(
                f,
                "{}: the app lacks the {n} scope; add it, reinstall the app and set its new token",
                self.method
            ),
            _ => write!(f, "{}: {e}", self.method),
        }
    }
}

impl std::error::Error for SlackError {}

fn slack_error(e: &anyhow::Error) -> Option<&str> {
    e.downcast_ref::<SlackError>().map(|s| s.error.as_str())
}

struct Api {
    base: String,
    token: String,
}

impl Api {
    fn from_env() -> Result<Api> {
        let token = std::env::var(TOKEN_VAR)
            .unwrap_or_default()
            .trim()
            .to_string();
        if token.is_empty() {
            bail!(
                "{TOKEN_VAR} isn't set where the server runs (a bot token, xoxb-…, from a Slack app installed in \
                 your workspace: see the self-hosting guide)"
            );
        }
        let base = std::env::var("AMPUP_MEMORY_SLACK_API")
            .unwrap_or_else(|_| "https://slack.com".into())
            .trim_end_matches('/')
            .to_string();
        Ok(Api { base, token })
    }

    /// One Web API call, waiting out a rate limit as Slack says, a few times.
    async fn call<T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: &[(&str, &str)],
    ) -> Result<T> {
        let mut waits = 0;
        loop {
            let res = http()
                .post(format!("{}/api/{method}", self.base))
                .bearer_auth(&self.token)
                .form(
                    &params
                        .iter()
                        .filter(|(_, v)| !v.is_empty())
                        .collect::<Vec<_>>(),
                )
                .send()
                .await
                .with_context(|| method.to_string())?;
            let status = res.status();
            if status.as_u16() == 429 && waits < WAITS {
                let wait = res
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(5)
                    .clamp(1, 60);
                waits += 1;
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            if status.as_u16() == 429 {
                bail!("{method}: rate limited; read again next round");
            }
            let v: Value = res
                .json()
                .await
                .with_context(|| format!("{method}: {status}, and not JSON"))?;
            if v["ok"] != json!(true) {
                return Err(SlackError {
                    method,
                    error: v["error"].as_str().unwrap_or("not ok").to_string(),
                    needed: v["needed"].as_str().map(String::from),
                }
                .into());
            }
            return serde_json::from_value(v).with_context(|| method.to_string());
        }
    }
}

#[derive(Deserialize, Default)]
struct Meta {
    #[serde(default)]
    next_cursor: String,
}

#[derive(Deserialize)]
struct Channel {
    id: String,
    name: String,
    #[serde(default)]
    is_private: bool,
    #[serde(default)]
    is_member: bool,
}

#[derive(Deserialize)]
struct ChannelList {
    #[serde(default)]
    channels: Vec<Channel>,
    #[serde(default)]
    response_metadata: Meta,
}

#[derive(Deserialize)]
struct ChannelInfo {
    channel: Channel,
}

#[derive(Deserialize, Clone)]
struct Message {
    ts: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    reply_count: Option<u64>,
    #[serde(default)]
    latest_reply: Option<String>,
}

#[derive(Deserialize)]
struct Messages {
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(default)]
    response_metadata: Meta,
}

/// A Slack `ts` (`1712345678.000200`) in microseconds.
fn ts_micros(ts: &str) -> Result<i64> {
    let (s, frac) = ts.split_once('.').unwrap_or((ts, ""));
    let frac: String = frac.chars().chain(std::iter::repeat('0')).take(6).collect();
    let s: i64 = s
        .parse()
        .with_context(|| format!("{ts:?} isn't a Slack ts"))?;
    let f: i64 = frac
        .parse()
        .with_context(|| format!("{ts:?} isn't a Slack ts"))?;
    Ok(s * 1_000_000 + f)
}

/// Microseconds as the `ts` Slack's `oldest` takes.
fn micros_ts(m: i64) -> String {
    format!("{}.{:06}", m.div_euclid(1_000_000), m.rem_euclid(1_000_000))
}

/// Channel names already looked up, by name: Slack lists a workspace's channels a page at a time.
static IDS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);
/// People's names, by Slack user id. A lookup that failed isn't kept: it's tried again next round.
static NAMES: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

/// The channel `target` names, as it is now (its name may have changed), and the app in it: a public channel the
/// app isn't in, it joins.
async fn channel(api: &Api, target: &str) -> Result<Channel> {
    let id = if is_id(target) {
        target.to_string()
    } else if let Some(id) = IDS.lock().unwrap().get(target).cloned() {
        id
    } else {
        let mut cursor = String::new();
        loop {
            let page: ChannelList = api
                .call(
                    "conversations.list",
                    &[
                        ("types", "public_channel,private_channel"),
                        ("exclude_archived", "true"),
                        ("limit", "200"),
                        ("cursor", &cursor),
                    ],
                )
                .await?;
            if let Some(c) = page.channels.iter().find(|c| c.name == target) {
                IDS.lock().unwrap().insert(target.to_string(), c.id.clone());
                break c.id.clone();
            }
            cursor = page.response_metadata.next_cursor;
            if cursor.is_empty() {
                bail!(
                    "no channel #{target} the app can see (a private one needs `/invite @app` in it first)"
                );
            }
        }
    };
    let mut c = api
        .call::<ChannelInfo>("conversations.info", &[("channel", &id)])
        .await?
        .channel;
    if !c.is_member {
        if c.is_private {
            bail!(
                "the app isn't in the private channel #{}: invite it there (`/invite @app`)",
                c.name
            );
        }
        api.call::<Value>("conversations.join", &[("channel", &id)])
            .await
            .with_context(|| format!("joining #{}", c.name))?;
        c.is_member = true;
    }
    Ok(c)
}

/// Every page of a history or a thread's replies from `oldest`, newest first for a history; `false` where it
/// stopped at `max_pages` with more to read.
async fn pages(
    api: &Api,
    method: &'static str,
    channel: &str,
    thread: Option<&str>,
    oldest: &str,
    max_pages: u32,
) -> Result<(Vec<Message>, bool)> {
    let mut out = Vec::new();
    let mut cursor = String::new();
    for _ in 0..max_pages {
        let mut params = vec![
            ("channel", channel),
            ("oldest", oldest),
            ("limit", "200"),
            ("cursor", cursor.as_str()),
        ];
        if let Some(ts) = thread {
            params.push(("ts", ts));
        }
        let page: Messages = api.call(method, &params).await?;
        out.extend(page.messages);
        cursor = page.response_metadata.next_cursor;
        if cursor.is_empty() {
            return Ok((out, true));
        }
    }
    Ok((out, false))
}

/// Names for these Slack user ids, each looked up once (`users.info`); one that fails stays an id.
async fn look_up(api: &Api, ids: BTreeSet<String>) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for id in ids {
        if let Some(n) = NAMES.lock().unwrap().get(&id) {
            out.insert(id, n.clone());
            continue;
        }
        let u: Value = match api.call("users.info", &[("user", &id)]).await {
            Ok(u) => u,
            Err(e) if slack_error(&e).is_some_and(|e| AUTH_ERRORS.contains(&e)) => return Err(e),
            Err(_) => continue,
        };
        let u = &u["user"];
        let p = &u["profile"];
        let name = [
            &p["real_name"],
            &u["real_name"],
            &p["display_name"],
            &u["name"],
        ]
        .into_iter()
        .filter_map(|v| v.as_str())
        .find(|s| !s.trim().is_empty())
        .unwrap_or(&id)
        .to_string();
        NAMES.lock().unwrap().insert(id.clone(), name.clone());
        out.insert(id, name);
    }
    Ok(out)
}

/// A name is whatever its owner typed: one plain line, so it can't pose as another line, address or person, and
/// short.
fn clean_name(name: &str) -> String {
    let n: String = name
        .replace(['<', '>', '\r', '\n', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(64)
        .collect();
    if n.is_empty() { "someone".into() } else { n }
}

/// Slack's markup made readable: `<@U1>` becomes `@Alice`, `<#C2|deals>` `#deals`, `<https://x|label>`
/// `label (https://x)`, `<!here>` `@here`.
fn readable(text: &str, people: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('>').filter(|&c| c > 0) else {
            out.push('<');
            rest = after;
            continue;
        };
        let inner = &after[..close];
        let (r, label) = match inner.split_once('|') {
            Some((r, l)) => (r, Some(l)),
            None => (inner, None),
        };
        if let Some(id) = r.strip_prefix('@') {
            match people.get(id) {
                Some(n) => out.push_str(&format!("@{}", clean_name(n))),
                None => out.push_str(&format!("@{id}")),
            }
        } else if let Some(c) = r.strip_prefix('#') {
            out.push_str(&format!("#{}", label.unwrap_or(c)));
        } else if let Some(s) = r.strip_prefix('!') {
            out.push_str(&format!(
                "@{}",
                label.unwrap_or_else(|| s.split('^').next().unwrap_or(s))
            ));
        } else {
            match label {
                Some(l) if l != r => out.push_str(&format!("{l} ({r})")),
                _ => out.push_str(r),
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The `slack_thread` body, in the order the hosted connector writes its keys.
#[derive(Serialize)]
struct Body<'a> {
    channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_ts: Option<&'a str>,
    messages: [Line<'a>; 1],
}

#[derive(Serialize)]
struct Line<'a> {
    ts: &'a str,
    author: String,
    text: String,
}

/// Whether a message is worth remembering: someone said something (a reply also sent to the channel counts).
fn worth(m: &Message) -> bool {
    m.subtype.as_deref().is_none_or(|s| s == "thread_broadcast")
        && m.text.as_deref().is_some_and(|t| !t.trim().is_empty())
}

/// A message as the hosted connector writes it: one source a message, a later version replacing it.
fn message_episode(
    c: &Channel,
    m: &Message,
    acl: &[String],
    people: &HashMap<String, String>,
) -> Result<Value> {
    let author = match m.user.as_deref() {
        Some(u) => people
            .get(u)
            .map_or_else(|| format!("@{u}"), |n| clean_name(n)),
        None => "@unknown".into(),
    };
    let body = Body {
        channel: format!("#{}", c.name),
        thread_ts: m.thread_ts.as_deref().filter(|t| *t != m.ts),
        messages: [Line {
            ts: &m.ts,
            author,
            text: readable(m.text.as_deref().unwrap_or_default(), people),
        }],
    };
    let mut about = vec![format!("channel:{}", c.id)];
    if let Some(t) = &m.thread_ts {
        about.push(format!("thread:{}:{t}", c.id));
    }
    if let Some(u) = &m.user {
        about.push(format!("slack_user:{u}"));
    }
    Ok(json!({
        "source": format!("slack:{}:#{}:{}", c.id, c.name, m.ts),
        "occurred_at": ts_micros(&m.ts)?,
        "acl": acl,
        "about": about,
        "index_text": true,
        "body": serde_json::to_string(&body)?,
        "replaces_source": true,
    }))
}

/// The user ids a message names: its author and those it mentions.
fn people_in(m: &Message, ids: &mut BTreeSet<String>) {
    let valid = |id: &str| {
        id.len() > 1
            && (id.starts_with('U') || id.starts_with('W'))
            && id
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    };
    if let Some(u) = m.user.as_deref().filter(|u| valid(u)) {
        ids.insert(u.to_string());
    }
    let text = m.text.as_deref().unwrap_or_default();
    for part in text.split("<@").skip(1) {
        let id: String = part
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if valid(&id) {
            ids.insert(id);
        }
    }
}

/// Read one Slack channel from its cursor, writing as it goes; the cursor moves only once everything new is
/// written.
pub(super) async fn read<W>(
    target: &str,
    since: &str,
    p: &mut Progress,
    acl: &[String],
    write: &W,
) -> Result<()>
where
    W: Fn(Value) -> BoxFuture<'static, Result<usize>>,
{
    read_channel(target, since, p, acl, write)
        .await
        .with_context(|| format!("slack {target}"))
}

async fn read_channel<W>(
    target: &str,
    since: &str,
    p: &mut Progress,
    acl: &[String],
    write: &W,
) -> Result<()>
where
    W: Fn(Value) -> BoxFuture<'static, Result<usize>>,
{
    let started = chrono::Utc::now().timestamp_micros();
    let api = Api::from_env()?;
    let mut c = channel(&api, target).await?;
    let floor = micros(since)?;
    let cursor = p.cursor.as_deref().map(ts_micros).transpose()?;
    // New is after `from`; threads started from `scan` on are looked at for replies after `from`.
    let (from, scan) = match cursor {
        Some(cur) => {
            let from = cur.max(floor);
            (
                from,
                (from - LOOKBACK.num_microseconds().unwrap_or(0)).max(floor),
            )
        }
        None => (floor, floor),
    };
    let oldest = micros_ts(scan);
    let history = match pages(
        &api,
        "conversations.history",
        &c.id,
        None,
        &oldest,
        HISTORY_PAGES,
    )
    .await
    {
        // Slack may say the app isn't in it after all (removed meanwhile): a public channel it joins again, once.
        Err(e) if slack_error(&e) == Some("not_in_channel") => {
            c = channel(&api, &c.id).await?;
            pages(
                &api,
                "conversations.history",
                &c.id,
                None,
                &oldest,
                HISTORY_PAGES,
            )
            .await?
        }
        r => r?,
    };
    let (history, whole) = history;
    // History comes newest first: stopping early left out the oldest, and only those before `from` may go.
    let oldest_read = history.iter().filter_map(|m| ts_micros(&m.ts).ok()).min();
    if !whole && oldest_read.is_none_or(|o| o > from) {
        bail!(
            "#{}: more than {} messages since {}; add it again with a later --since",
            c.name,
            HISTORY_PAGES * 200,
            micros_ts(from)
        );
    }
    // The messages to write, by ts (a reply also sent to the channel is in both the history and its thread).
    let mut new: BTreeMap<i64, Message> = BTreeMap::new();
    let mut threads = Vec::new();
    for m in &history {
        let ts = ts_micros(&m.ts)?;
        if ts > from {
            new.insert(ts, m.clone());
        }
        let latest = m
            .latest_reply
            .as_deref()
            .map(ts_micros)
            .transpose()?
            .unwrap_or(ts);
        if m.reply_count.unwrap_or(0) > 0 && latest > from && m.thread_ts.as_deref() == Some(&m.ts)
        {
            threads.push(m.ts.clone());
        }
    }
    let after = micros_ts(from);
    for t in &threads {
        // Every reply after `from` (Slack also gives the thread's first message, whatever `oldest` says).
        let (replies, _) = pages(
            &api,
            "conversations.replies",
            &c.id,
            Some(t),
            &after,
            u32::MAX,
        )
        .await?;
        for r in replies {
            let ts = ts_micros(&r.ts)?;
            if ts > from && r.ts != *t {
                new.insert(ts, r);
            }
        }
    }
    new.retain(|_, m| worth(m));
    let mut ids = BTreeSet::new();
    for m in new.values() {
        people_in(m, &mut ids);
    }
    let people = look_up(&api, ids).await?;
    let episodes = new
        .values()
        .map(|m| message_episode(&c, m, acl, &people))
        .collect::<Result<Vec<_>>>()?;
    for chunk in episodes.chunks(BATCH) {
        p.records += write(json!({ "episodes": chunk })).await? as u64;
    }
    // Everything posted before the round started is read, give or take the clocks.
    let through = started - SKEW.num_microseconds().unwrap_or(0);
    if cursor.is_none_or(|c| through > c) {
        p.cursor = Some(micros_ts(through));
    }
    p.partial = false;
    Ok(())
}
