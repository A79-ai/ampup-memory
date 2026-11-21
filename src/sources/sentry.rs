//! **Sentry.** A project's issues, each as it stands (a later version retires the one before, `replaces_source`), in
//! the format a hosted deployment's Sentry connector writes and the `incident` episode type reads: source
//! `sentry:ORG/PROJECT/issues/ID`, about `incident:ORG/PROJECT/issues/ID`, and a body of the issue's header facts
//! (status, level, culprit, first and last seen, events, users, releases, link), its error, its stack and its
//! activity.
//!
//! **The token.** `SENTRY_AUTH_TOKEN` in the server's environment (sentry-cli's own name), else the file
//! `AMPUP_MEMORY_SENTRY_TOKEN_FILE` names, else the one `sentry-cli login` keeps in `~/.sentryclirc`. The background
//! service carries no token into its unit, but it does carry `AMPUP_MEMORY_*_FILE` settings, and it reads
//! `~/.sentryclirc` as `serve --local` does. The token needs to read the project, its issues and their events (a
//! personal token or an internal integration's; an organization token made for uploading releases can't). It is
//! never written to a file or taken on the command line. Sentry is `SENTRY_URL` (or `AMPUP_MEMORY_SENTRY_URL`, or
//! `url` under `[defaults]` in `~/.sentryclirc`), else `https://sentry.io`: a self-hosted Sentry, or
//! `https://de.sentry.io` for an organization kept in the EU.
//!
//! **What's read.** The issues with events from `--since` (default [`super::BACKFILL_DAYS`] days back) on, the last
//! seen first, then those with events from [`OVERLAP`] before the newest event read (the cursor), and once a day
//! those with events in the last [`RESCAN`] days, so an issue resolved, ignored or assigned with no event since is
//! found too. An issue is read whole (its activity, releases and latest event) and written only when its state
//! changed (status, assignee, comments, title, culprit, level), or when it had events since its last version and
//! that version is a day old ([`REFRESH`]): a busy issue isn't written again every round for its counts. An issue
//! first seen ignored isn't written. A round lists at most [`MAX_PAGES`] pages of 100 and reads at most
//! [`MAX_READ`] issues whole; one cut short leaves the cursor where it was, and the next lists the same window
//! again, past the issues it wrote.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{MAX_PAGES, OVERLAP, Progress, SWEEP_EVERY, Source, Version, http};

/// How far back the day's wider round reads (the hosted connector's rescan).
const RESCAN: chrono::Duration = chrono::Duration::days(14);
/// How often an issue whose state hasn't changed is written again for its counts.
const REFRESH: i64 = 86_400;
/// Issues read whole (two requests each) in one round.
const MAX_READ: usize = 200;
/// Frames of a stack written, and activities of an issue, and characters of a comment.
const STACK_MAX: usize = 15;
const ACTIVITY_MAX: usize = 50;
const COMMENT_MAX: usize = 500;
/// A wait Sentry asks for that's this short is waited out in place, at most this many times a request.
const WAIT_IN_PLACE: Duration = Duration::from_secs(30);
const WAITS_IN_PLACE: u32 = 5;

/// `ORG/PROJECT`, each a Sentry slug.
fn check_target(target: &str) -> Result<()> {
    let ok = |s: &str| {
        let mut c = s.chars();
        !s.is_empty()
            && s.len() <= 100
            && c.next()
                .is_some_and(|f| f.is_ascii_lowercase() || f.is_ascii_digit())
            && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_' || x == '-')
    };
    match target.split_once('/') {
        Some((org, project)) if ok(org) && ok(project) => Ok(()),
        _ => bail!("{target:?} isn't ORG/PROJECT (Sentry's slugs, as in its URLs)"),
    }
}

/// `sources add sentry ORG/PROJECT`.
pub fn source(target: &str, since: String) -> Result<Source> {
    check_target(target)?;
    Ok(Source::Sentry {
        project: target.to_string(),
        since,
    })
}

/// `key = value` under `[section]` in `~/.sentryclirc`, as sentry-cli writes it.
fn sentryclirc(section: &str, key: &str) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let text = std::fs::read_to_string(std::path::Path::new(&home).join(".sentryclirc")).ok()?;
    let mut here = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            here = s.trim() == section;
        } else if here
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
            && !v.trim().is_empty()
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

fn env(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The Sentry token this machine holds, if any.
fn sentry_token() -> Result<Option<String>> {
    if let Some(t) = env("SENTRY_AUTH_TOKEN") {
        return Ok(Some(t));
    }
    if let Some(f) = env("AMPUP_MEMORY_SENTRY_TOKEN_FILE") {
        let t = std::fs::read_to_string(&f)
            .with_context(|| format!("reading AMPUP_MEMORY_SENTRY_TOKEN_FILE ({f})"))?;
        return Ok(Some(t.trim().to_string()).filter(|t| !t.is_empty()));
    }
    Ok(sentryclirc("auth", "token"))
}

fn sentry_url() -> String {
    env("SENTRY_URL")
        .or_else(|| env("AMPUP_MEMORY_SENTRY_URL"))
        .or_else(|| sentryclirc("defaults", "url"))
        .unwrap_or_else(|| "https://sentry.io".into())
        .trim_end_matches('/')
        .to_string()
}

struct Api {
    base: String,
    token: String,
    target: String,
}

impl Api {
    /// One page of `path` (under `/api/0/`), and the cursor of the next page when there is one; `None` for a 404.
    async fn page(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<(Value, Option<String>)>> {
        let t = &self.target;
        for waits in 0.. {
            let res = http()
                .get(format!("{}/api/0/{path}", self.base))
                .query(query)
                .bearer_auth(&self.token)
                .header("accept", "application/json")
                .send()
                .await
                .with_context(|| format!("sentry {t}: couldn't reach {}", self.base))?;
            let status = res.status();
            let header = |k: &str| {
                res.headers()
                    .get(k)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from)
            };
            if status.is_success() {
                let next = header("link").as_deref().and_then(next_cursor);
                return Ok(Some((res.json().await?, next)));
            }
            if status.as_u16() == 429 {
                let wait = retry_after(header("x-sentry-rate-limit-reset"), header("retry-after"));
                if wait <= WAIT_IN_PLACE && waits < WAITS_IN_PLACE {
                    tokio::time::sleep(wait).await;
                    continue;
                }
                bail!(
                    "sentry {t}: rate limited for {}s; read again next round",
                    wait.as_secs()
                );
            }
            let text = res.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["detail"].as_str().map(String::from))
                .unwrap_or(text);
            let detail: String = detail.chars().take(300).collect();
            match status.as_u16() {
                401 => bail!("sentry {t}: the token was refused (401): {detail}"),
                403 => bail!(
                    "sentry {t}: the token may not read this (403: {detail}); it needs to read the project, its \
                     issues and events (project:read, event:read, org:read), as a personal token or an internal \
                     integration's can"
                ),
                404 => return Ok(None),
                _ => bail!("sentry {t}: {status} on /api/0/{path}: {detail}"),
            }
        }
        unreachable!()
    }

    async fn get(&self, path: &str) -> Result<Option<Value>> {
        Ok(self.page(path, &[]).await?.map(|(v, _)| v))
    }
}

/// How long Sentry says to wait: until `X-Sentry-Rate-Limit-Reset` (epoch seconds), or `Retry-After`, else a minute.
fn retry_after(reset: Option<String>, after: Option<String>) -> Duration {
    let now = chrono::Utc::now().timestamp();
    if let Some(r) = reset
        .and_then(|r| r.trim().parse::<f64>().ok())
        .filter(|r| *r > 0.0)
    {
        return Duration::from_secs((r as i64 - now).max(0) as u64 + 1);
    }
    if let Some(a) = after.and_then(|a| a.trim().parse::<f64>().ok()) {
        return Duration::from_secs(a.max(0.0) as u64);
    }
    Duration::from_secs(60)
}

/// The `cursor` of the `rel="next"` link, when it says there are results.
fn next_cursor(link: &str) -> Option<String> {
    let part = link.split(',').find(|p| p.contains("rel=\"next\""))?;
    if !part.contains("results=\"true\"") {
        return None;
    }
    let (_, rest) = part.split_once("cursor=\"")?;
    Some(rest.split('"').next()?.to_string())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Issue {
    id: Value,
    #[serde(rename = "shortId")]
    short_id: Option<String>,
    title: Option<String>,
    culprit: Option<String>,
    level: Option<String>,
    status: Option<String>,
    substatus: Value,
    count: Value,
    #[serde(rename = "userCount")]
    user_count: Value,
    #[serde(rename = "firstSeen")]
    first_seen: Option<String>,
    #[serde(rename = "lastSeen")]
    last_seen: Option<String>,
    permalink: Option<String>,
    #[serde(rename = "numComments")]
    num_comments: Value,
    #[serde(rename = "assignedTo")]
    assigned_to: Value,
    project: Value,
    metadata: Value,
    #[serde(rename = "firstRelease")]
    first_release: Value,
    #[serde(rename = "lastRelease")]
    last_release: Value,
    activity: Vec<Value>,
}

impl Issue {
    fn id(&self) -> String {
        str_of(&self.id)
    }
    fn last_seen(&self) -> &str {
        self.last_seen.as_deref().unwrap_or_default()
    }
}

/// A string as itself, a number as its digits, anything else as nothing.
fn str_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// One line: runs of whitespace (newlines too) as one space.
fn line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn line_of(v: &Value) -> String {
    line(&str_of(v))
}

/// The first of these that isn't empty.
fn or(parts: impl IntoIterator<Item = String>) -> String {
    parts
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_default()
}

/// Milliseconds since the epoch, as a JavaScript `Date` reads an RFC 3339 time.
fn millis(t: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(t.trim())
        .ok()
        .map(|d| d.timestamp_millis())
}

/// RFC 3339 at whole seconds, in UTC: `2026-11-18T09:30:00Z`.
fn rfc3339(t: &str) -> String {
    match millis(t).and_then(chrono::DateTime::from_timestamp_millis) {
        Some(d) => d.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        None => line(t),
    }
}

/// A count as a whole number: 0 where it isn't one.
fn count(v: &Value) -> String {
    let n = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    match n.filter(|n| n.is_finite() && *n != 0.0) {
        Some(n) if n.fract() == 0.0 => format!("{}", n as i64),
        Some(n) => n.to_string(),
        None => "0".into(),
    }
}

/// The exception values of an event's exception entry, as Sentry lists them (the main one last).
fn exceptions(ev: Option<&Value>) -> Vec<Value> {
    ev.and_then(|e| e["entries"].as_array())
        .and_then(|es| es.iter().find(|e| e["type"] == "exception"))
        .and_then(|e| e["data"]["values"].as_array())
        .cloned()
        .unwrap_or_default()
}

/// A JSON value JavaScript reads as true.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// The error: the main exception, or the event's message, or the issue's metadata, or its title.
fn error_line(issue: &Issue, ev: Option<&Value>) -> String {
    if let Some(main) = exceptions(ev).last()
        && (truthy(&main["type"]) || truthy(&main["value"]))
    {
        let ty = or([line_of(&main["type"]), "Error".into()]);
        let value = line_of(&main["value"]);
        return if value.is_empty() {
            ty
        } else {
            format!("{ty}: {value}")
        };
    }
    let msg = ev
        .and_then(|e| e["entries"].as_array())
        .and_then(|es| es.iter().find(|e| e["type"] == "message"))
        .map(|e| &e["data"]);
    fn given(v: Option<&Value>) -> Option<&Value> {
        v.filter(|v| !v.is_null())
    }
    let text = given(msg.map(|m| &m["formatted"]))
        .or_else(|| given(msg.map(|m| &m["message"])))
        .or_else(|| given(ev.map(|e| &e["message"])))
        .map(line_of)
        .unwrap_or_default();
    if !text.is_empty() {
        return text;
    }
    let md = &issue.metadata;
    if truthy(&md["type"]) || truthy(&md["value"]) {
        return [line_of(&md["type"]), line_of(&md["value"])]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(": ");
    }
    line(issue.title.as_deref().unwrap_or_default())
}

/// The stack's frames, innermost first, in-app frames before the rest, at most [`STACK_MAX`]: from the main
/// exception, or the nearest one with a stack.
fn stack_lines(ev: Option<&Value>) -> Vec<String> {
    let values = exceptions(ev);
    let frames: Vec<&Value> = values
        .iter()
        .rev()
        .find_map(|v| {
            v["stacktrace"]["frames"]
                .as_array()
                .filter(|f| !f.is_empty())
        })
        .map(|f| f.iter().rev().collect())
        .unwrap_or_default();
    let in_app = |f: &Value| truthy(&f["inApp"]);
    frames
        .iter()
        .filter(|f| in_app(f))
        .chain(frames.iter().filter(|f| !in_app(f)))
        .take(STACK_MAX)
        .map(|f| {
            let file = or([
                line_of(&f["absPath"]),
                line_of(&f["filename"]),
                line_of(&f["module"]),
                "?".into(),
            ]);
            let at = match &f["lineNo"] {
                Value::Number(n) => n.to_string(),
                _ => "?".into(),
            };
            format!(
                "{file}:{at} in {}",
                or([line_of(&f["function"]), "?".into()])
            )
        })
        .collect()
}

/// What one activity says happened.
fn activity_text(a: &Value) -> String {
    let d = &a["data"];
    let ty = a["type"].as_str().unwrap_or_default();
    match ty {
        "set_resolved" => "resolved".into(),
        "set_resolved_in_release" => match line_of(&d["version"]) {
            v if v.is_empty() => "resolved in the next release".into(),
            v => format!("resolved in {v}"),
        },
        "set_resolved_in_commit" => "resolved in a commit".into(),
        "set_resolved_in_pull_request" => "resolved in a pull request".into(),
        "set_resolved_by_age" => "resolved by age".into(),
        "set_unresolved" => "unresolved".into(),
        "set_ignored" | "set_archived" => "ignored".into(),
        "set_regression" => match line_of(&d["version"]) {
            v if v.is_empty() => "regressed".into(),
            v => format!("regressed in {v}"),
        },
        "assigned" => {
            let who = or([
                line_of(&d["assigneeEmail"]),
                line_of(&d["assigneeName"]),
                if d["assigneeType"] == "team" {
                    format!("team {}", line_of(&d["assignee"]))
                } else {
                    line_of(&d["assignee"])
                },
            ]);
            if who.is_empty() {
                "assigned".into()
            } else {
                format!("assigned to {who}")
            }
        }
        "unassigned" => "unassigned".into(),
        "note" => {
            let text = line_of(&d["text"]);
            match text.chars().count() > COMMENT_MAX {
                true => format!(
                    "commented \"{}…\"",
                    text.chars().take(COMMENT_MAX).collect::<String>()
                ),
                false => format!("commented \"{text}\""),
            }
        }
        "first_seen" => "first seen".into(),
        "set_escalating" => "escalating".into(),
        "set_priority" => match line_of(&d["priority"]) {
            p if p.is_empty() => "priority changed".into(),
            p => format!("priority set to {p}"),
        },
        _ => or([
            line(&ty.strip_prefix("set_").unwrap_or(ty).replace('_', " ")),
            "changed".into(),
        ]),
    }
}

/// The issue's activity, oldest first, the newest [`ACTIVITY_MAX`] of it.
fn activity_lines(issue: &Issue) -> Vec<String> {
    let mut acts: Vec<&Value> = issue
        .activity
        .iter()
        .filter(|a| a["type"].is_string() && truthy(&a["dateCreated"]))
        .collect();
    acts.sort_by(|a, b| str_of(&a["dateCreated"]).cmp(&str_of(&b["dateCreated"])));
    let skip = acts.len().saturating_sub(ACTIVITY_MAX);
    acts[skip..]
        .iter()
        .map(|a| {
            let u = &a["user"];
            let actor = or([
                line_of(&u["name"]),
                line_of(&u["email"]),
                line_of(&u["username"]),
                "Sentry".into(),
            ]);
            format!(
                "{} {actor}: {}",
                rfc3339(&str_of(&a["dateCreated"])),
                activity_text(a)
            )
        })
        .collect()
}

/// The issue's text, laid out as the `incident` episode type reads it.
fn issue_body(org: &str, project: &str, issue: &Issue, ev: Option<&Value>) -> String {
    let opt = |s: &Option<String>| line(s.as_deref().unwrap_or_default());
    let release = |r: &Value| or([line_of(&r["version"]), "-".into()]);
    let mut head = vec![
        format!(
            "# {}",
            or([opt(&issue.title), opt(&issue.short_id), issue.id()])
        ),
        format!(
            "Project: {org}/{project} · Level: {} · Status: {}",
            or([opt(&issue.level), "error".into()]),
            or([opt(&issue.status), "unresolved".into()])
        ),
        format!("Culprit: {}", or([opt(&issue.culprit), "-".into()])),
        format!(
            "First seen: {} · Last seen: {} · Events: {} · Users: {}",
            rfc3339(issue.first_seen.as_deref().unwrap_or_default()),
            rfc3339(issue.last_seen()),
            count(&issue.count),
            count(&issue.user_count)
        ),
        format!(
            "Release: first {} · last {}",
            release(&issue.first_release),
            release(&issue.last_release)
        ),
        format!("Link: {}", opt(&issue.permalink)),
    ];
    let short = opt(&issue.short_id);
    if !short.is_empty() {
        head.push(format!("Short ID: {short}"));
    }
    let mut parts = vec![
        head.join("\n"),
        format!("## Error\n{}", error_line(issue, ev)),
    ];
    let stack = stack_lines(ev);
    if !stack.is_empty() {
        parts.push(format!("## Stack\n{}", stack.join("\n")));
    }
    let activity = activity_lines(issue);
    if !activity.is_empty() {
        parts.push(format!("## Activity\n{}", activity.join("\n")));
    }
    format!("{}\n", parts.join("\n\n"))
}

/// When the issue last changed, in milliseconds: its last event, or a later activity.
fn changed_at(issue: &Issue) -> i64 {
    let mut t = millis(issue.last_seen());
    for a in &issue.activity {
        if let Some(at) = millis(&str_of(&a["dateCreated"]))
            && t.is_none_or(|t| at > t)
        {
            t = Some(at);
        }
    }
    t.unwrap_or_else(|| chrono::Utc::now().timestamp_millis())
}

/// What a new version is written for: the issue's state as its list entry shows it, leaving out its counts and
/// last event, which move with every event.
fn change_key(i: &Issue) -> String {
    let a = &i.assigned_to;
    let who = if truthy(a) {
        format!(
            "{}:{}",
            str_of(&a["type"]),
            or([str_of(&a["id"]), str_of(&a["name"])])
        )
    } else {
        String::new()
    };
    let opt = |s: &Option<String>| line(s.as_deref().unwrap_or_default());
    json!([
        i.status,
        i.substatus,
        who,
        if i.num_comments.is_null() {
            json!(0)
        } else {
            i.num_comments.clone()
        },
        opt(&i.title),
        opt(&i.culprit),
        opt(&i.level),
    ])
    .to_string()
}

/// The issue's episode: its newest version, retiring the earlier ones.
fn issue_episode(
    org: &str,
    project: &str,
    issue: &Issue,
    ev: Option<&Value>,
    acl: &[String],
) -> Value {
    let path = format!("{org}/{project}/issues/{}", issue.id());
    json!({
        "source": format!("sentry:{path}"),
        "occurred_at": changed_at(issue) * 1000,
        "acl": acl,
        "about": [format!("incident:{path}")],
        "index_text": true,
        "body": issue_body(org, project, issue, ev),
        "replaces_source": true,
    })
}

/// Whether a listed issue is to be read whole and written: never written and not ignored, its state changed, or
/// events since a version a day old.
fn due(i: &Issue, was: Option<&Version>, now: i64) -> bool {
    let Some(was) = was else {
        return i.status.as_deref() != Some("ignored");
    };
    if was.key != change_key(i) {
        return true;
    }
    let seen = |t: &str| millis(t).unwrap_or(0);
    seen(i.last_seen()) > seen(&was.seen) && now - was.at >= REFRESH
}

/// Read one Sentry project from its cursor, writing as it goes; the cursor moves only once the window is read.
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
    let Some((org, project)) = target.split_once('/') else {
        bail!("sentry {target}: isn't ORG/PROJECT");
    };
    let Some(token) = sentry_token()? else {
        bail!(
            "sentry {target}: no token: set SENTRY_AUTH_TOKEN in the server's environment, \
             AMPUP_MEMORY_SENTRY_TOKEN_FILE to a file holding one, or sign in with `sentry-cli login`"
        );
    };
    let api = Api {
        base: sentry_url(),
        token,
        target: target.to_string(),
    };
    let Some(proj) = api.get(&format!("projects/{org}/{project}/")).await? else {
        bail!(
            "sentry {target}: no such project at {}, or not readable with this token",
            api.base
        );
    };
    let project_id = str_of(&proj["id"]);
    let now = chrono::Utc::now();
    let since_t = chrono::DateTime::parse_from_rfc3339(since)?.to_utc();
    let sweep = p.cursor.is_some() && !p.partial && now.timestamp() - p.swept_at >= SWEEP_EVERY;
    if sweep {
        p.swept_at = now.timestamp();
    }
    let from = match &p.cursor {
        None => since_t,
        Some(c) => {
            let back = chrono::DateTime::parse_from_rfc3339(c)?.to_utc() - OVERLAP;
            match sweep {
                true => back.min(now - RESCAN),
                false => back,
            }
            .max(since_t)
        }
    };
    let iso =
        |t: chrono::DateTime<chrono::Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (start, end) = (iso(from), iso(now));
    let mut newest = p.cursor.clone();
    let mut read = 0;
    let mut page_cursor: Option<String> = None;
    let mut done = false;
    let mut failed = None;
    'pages: for _ in 0..MAX_PAGES {
        let mut query = vec![
            ("project", project_id.as_str()),
            ("query", ""),
            ("sort", "date"),
            ("start", start.as_str()),
            ("end", end.as_str()),
            ("limit", "100"),
        ];
        if let Some(c) = &page_cursor {
            query.push(("cursor", c.as_str()));
        }
        let (listed, next) = match api
            .page(&format!("organizations/{org}/issues/"), &query)
            .await
        {
            Ok(Some((v, next))) => (serde_json::from_value::<Vec<Issue>>(v)?, next),
            Ok(None) => {
                failed = Some(anyhow::anyhow!(
                    "sentry {target}: the issue list isn't there (404)"
                ));
                break;
            }
            Err(e) => {
                failed = Some(e);
                break;
            }
        };
        let now_s = now.timestamp();
        let mut episodes = vec![];
        let mut versions = vec![];
        for i in listed
            .iter()
            .filter(|i| due(i, p.versions.get(&i.id()), now_s))
        {
            if read >= MAX_READ {
                break;
            }
            read += 1;
            let id = i.id();
            let whole = match api.get(&format!("organizations/{org}/issues/{id}/")).await {
                Ok(Some(v)) => match serde_json::from_value::<Issue>(v) {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("sources: sentry {target}: issue {id} skipped: {e}");
                        continue;
                    }
                },
                Ok(None) => continue,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            };
            if whole.project["slug"].as_str().is_some_and(|s| s != project) {
                continue;
            }
            let ev = match api
                .get(&format!("organizations/{org}/issues/{id}/events/latest/"))
                .await
            {
                Ok(ev) => ev,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            };
            episodes.push(issue_episode(org, project, &whole, ev.as_ref(), acl));
            // What the list showed: the next round compares its list entries with this.
            versions.push((
                id,
                Version {
                    key: change_key(i),
                    seen: i.last_seen().to_string(),
                    at: now_s,
                },
            ));
        }
        if !episodes.is_empty() {
            match write(json!({ "episodes": episodes })).await {
                Ok(n) => p.records += n as u64,
                Err(e) => {
                    failed = Some(e);
                    break 'pages;
                }
            }
            p.versions.extend(versions);
        }
        if failed.is_some() || read >= MAX_READ {
            break;
        }
        for i in &listed {
            if newest
                .as_deref()
                .is_none_or(|c| millis(i.last_seen()) > millis(c))
                && !i.last_seen().is_empty()
            {
                newest = Some(i.last_seen().to_string());
            }
        }
        match next {
            Some(c) => page_cursor = Some(c),
            None => {
                done = true;
                break;
            }
        }
    }
    // An issue with no event in this round's window or the rescan's isn't listed again but for a new event, which
    // writes it anyway.
    let keep = from
        .min(now - RESCAN - chrono::Duration::days(1))
        .timestamp_millis();
    p.versions
        .retain(|_, v| millis(&v.seen).is_none_or(|t| t >= keep));
    if done {
        p.cursor = newest;
        p.partial = false;
    } else {
        p.partial = true;
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
