//! One log line per advice call, whoever made it and however it came in: an agent's own `start_session` and
//! `get_advice` over MCP, and a hook's `/sessions` and `/advice`. Each says the org, the user, the app the call came
//! from (its `User-Agent`'s product, `openai-mcp` for ChatGPT, say), the session, the moment, what the call came to
//! and how long it took, so the log answers "who asks for advice, from which app, and what do they get" without
//! joining anything:
//!
//! `advice call: org=acme user=a@x.com app=openai-mcp via=mcp:get_advice session=… moment=agent outcome=served 4210 ms`

use std::time::Duration;

use hyper::HeaderMap;
use serde_json::Value;

/// The app a request came from: its `User-Agent`'s first product name (`claude-code/2.1` is `claude-code`),
/// lowercased, at most 40 characters of `[a-z0-9._-]`; `-` when it sent none.
pub(super) fn app_of(headers: &HeaderMap) -> String {
    let ua = headers
        .get(hyper::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let product: String = ua
        .split(['/', ' ', '(', ';'])
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(40)
        .collect::<String>()
        .to_ascii_lowercase();
    if product.is_empty() {
        "-".into()
    } else {
        product
    }
}

/// The arguments of `get_advice` only memstore's own hooks may send: an agent's call is offered read-only, and
/// these are the ones that keep or record what it carries (`max_chars` marks a hook's ask, whose question is kept).
pub(super) const HOOK_ONLY: [&str; 5] = ["max_chars", "event", "decide", "remembered", "keep"];

/// Whether a call to `/mcp` came from memstore-client 0.1.15's hooks (`memstore-hook/<version>`), which sent
/// the hook-only arguments there; later clients send them to `/mcp/hook`. A model behind an MCP host can choose a
/// tool's arguments but not its host's `User-Agent`. Drop once 0.1.15 is out of use.
pub(super) fn from_hook(app: &str) -> bool {
    app == "memstore-hook"
}

/// What an advice call's answer came to, read from what it hands back (a tool's text, or a route's JSON):
/// `pending` (still being looked up), `none` (nothing for this task), `served`, or `error`.
pub(super) fn outcome(answer: Result<&str, ()>) -> &'static str {
    let Ok(t) = answer else { return "error" };
    if t.starts_with(super::ADVICE_PENDING) {
        return "pending";
    }
    if t.starts_with(super::NO_NEW_ADVICE) || t.starts_with("No advice") {
        return "none";
    }
    match serde_json::from_str::<Value>(t) {
        Ok(v) if v.is_object() => {
            if v["advice_pending"].is_string() {
                "pending"
            } else if v["advice"].is_null() || v["advice"] == "" {
                "none"
            } else {
                "served"
            }
        }
        _ => "served",
    }
}

/// The session a tool's text names (`session_id: <id> (…`), for a `get_advice` that opened one.
pub(super) fn session_named(text: &str) -> Option<String> {
    let rest = text.split("session_id: ").nth(1)?;
    let id = rest.split(|c: char| c.is_whitespace()).next()?;
    (!id.is_empty()).then(|| id.to_string())
}

/// A caller-given value as the log may show it: up to 80 characters of `[A-Za-z0-9._:/-]`, else `-`, so a
/// session id or moment can't forge or split a line.
fn token(v: Option<&str>) -> &str {
    v.filter(|v| {
        !v.is_empty()
            && v.len() <= 80
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
    })
    .unwrap_or("-")
}

/// Logs one MCP request, whatever it asked (`initialize`, `tools/list`, a tool), so a client that connects but
/// never asks for advice (one that only lists the tools, or only searches) still shows, with its app:
///
/// `mcp call: org=acme user=a@x.com app=openai-mcp via=/mcp method=tools/call tool=search_memory outcome=ok 812 ms`
///
/// `outcome` is `ok`, `error` (a JSON-RPC error), `tool-error` (the tool answered `isError`) or `-` (a
/// notification, which has no answer).
pub(super) fn mcp_call(
    org: Option<&str>,
    user: &str,
    app: &str,
    via: &str,
    body: &[u8],
    answer: Option<&Value>,
    took: Duration,
) {
    let req: Value = serde_json::from_slice(body).unwrap_or_default();
    let method = token(req["method"].as_str());
    let tool = token(
        req["params"]["name"]
            .as_str()
            .filter(|_| method == "tools/call"),
    );
    let outcome = match answer {
        None => "-",
        Some(v) if !v["error"].is_null() => "error",
        Some(v) if v["result"]["isError"] == true => "tool-error",
        Some(_) => "ok",
    };
    eprintln!(
        "mcp call: org={} user={user} app={app} via={via} method={method} tool={tool} outcome={outcome} {} ms",
        org.unwrap_or("-"),
        took.as_millis()
    );
}

/// Logs one advice call (the module's line).
#[allow(clippy::too_many_arguments)]
pub(super) fn log(
    org: Option<&str>,
    user: &str,
    app: &str,
    via: &str,
    session: Option<&str>,
    moment: &str,
    outcome: &str,
    took: Duration,
) {
    let moment = token(Some(moment));
    eprintln!(
        "advice call: org={} user={user} app={app} via={via} session={} moment={moment} outcome={outcome} {} ms",
        org.unwrap_or("-"),
        token(session),
        took.as_millis()
    );
}
