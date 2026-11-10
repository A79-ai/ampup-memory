//! `POST /agent/run`: a question answered by a model that calls this memory's MCP tools, as an agent would,
//! and the trace of what it called. For the console's Ask, which runs one question on several tool sets (arms)
//! to compare them: the memstore tools, raw episode search, none.
//!
//! The caller names the tools; the model is offered only those of [`READS`] among them, and a call of any other
//! (one it was not offered, a write) is answered as refused, never run. Every tool runs as the caller, read-only
//! (`Writes::No`: `get_advice` opens no session), through the same `tools/call` an agent's MCP client reaches, so
//! its ACLs, metering and limits are those of `/mcp`. The model is the server's own (`AMPUP_MEMORY_EXTRACT`, an
//! OpenAI-compatible backend), one of [`MODELS`]; a run stops at its tool-call cap or its time budget
//! (`AMPUP_MEMORY_AGENT_BUDGET_SECS`, default 45: under the console's 60 s proxy) with what it has.

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use super::{Reads, State, Writes, mcp};
use crate::Principal;
use crate::extract::Extractor;
use crate::people::directory::SignIn;

/// The tools a run may offer: those that only read.
const READS: [&str; 8] = [
    "search_memory",
    "get_episode",
    "query_memory",
    "get_advice",
    "get_brief",
    "list_briefs",
    "list_facts",
    "list_advice",
];

/// The models a run may ask for, by name, and the OpenRouter model each is.
const MODELS: [(&str, &str); 3] = [
    ("haiku", "anthropic/claude-haiku-5.5"),
    ("sonnet", "anthropic/claude-sonnet-5.5"),
    ("opus", "anthropic/claude-opus-5.5"),
];

/// The most tool calls one run makes, whatever it asks.
const MAX_CALLS: usize = 12;
/// The longest question, and the most earlier turns (and their characters) a run is given.
const QUESTION_CHARS: usize = 4_000;
const HISTORY_TURNS: usize = 10;
const HISTORY_CHARS: usize = 20_000;
/// What the model is shown of one tool's answer (the trace keeps [`TRACE_CHARS`]).
const RESULT_CHARS: usize = 6_000;
const TRACE_CHARS: usize = 4_000;

static BUDGET: LazyLock<Duration> = LazyLock::new(|| {
    Duration::from_secs(
        std::env::var("AMPUP_MEMORY_AGENT_BUDGET_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(45),
    )
});

const SYSTEM: &str = "You answer questions about an organization from its memory, using only the tools you are \
given. Look things up before answering; read a cited episode when a fact alone doesn't settle it. Answer briefly \
and plainly, and cite what you rely on by the ids the tools gave ([e3], an episode id, a fact id). If the tools \
don't hold the answer, say so rather than guess. With no tools, answer from what you know and say that you had \
no memory to look in.";

/// A run's request, as `/agent/run` takes it.
pub(super) struct Ask {
    question: String,
    tools: Vec<String>,
    model: &'static str,
    max_calls: usize,
    /// Earlier turns of the same chat: (question, answer).
    history: Vec<(String, String)>,
}

impl Ask {
    /// Reads a request body: `{question, tools: [name], model?: haiku|sonnet|opus, max_tool_calls?, history?:
    /// [{question, answer}]}`. An error is the caller's to fix (a 400).
    pub fn parse(v: &Value) -> Result<Ask, String> {
        let question = v["question"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        if question.is_empty() {
            return Err("question is required".into());
        }
        // Every turn sends the whole conversation again: what a run is given is capped, so it costs what a
        // question does, not what a request body can hold.
        if question.chars().count() > QUESTION_CHARS {
            return Err(format!("question is {QUESTION_CHARS} characters at most"));
        }
        let tools = match &v["tools"] {
            Value::Null => vec![],
            Value::Array(a) => a
                .iter()
                .map(|t| t.as_str().map(String::from).ok_or("tools are names"))
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("tools is a list of tool names".into()),
        };
        if let Some(t) = tools.iter().find(|t| !READS.contains(&t.as_str())) {
            return Err(format!(
                "{t} isn't a tool a run may use: {}",
                READS.join(", ")
            ));
        }
        let name = v["model"].as_str().unwrap_or("haiku");
        let model = MODELS
            .iter()
            .find(|(n, id)| *n == name || *id == name)
            .map(|(_, id)| *id)
            .ok_or_else(|| format!("model is haiku, sonnet or opus, not {name}"))?;
        let max_calls = match &v["max_tool_calls"] {
            Value::Null => 8,
            n => n
                .as_u64()
                .map(|n| (n as usize).min(MAX_CALLS))
                .ok_or("max_tool_calls is a whole number")?,
        };
        let history: Vec<(String, String)> = v["history"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| Some((t["question"].as_str()?.into(), t["answer"].as_str()?.into())))
            .collect();
        if history.len() > HISTORY_TURNS
            || history
                .iter()
                .map(|(q, a)| q.chars().count() + a.chars().count())
                .sum::<usize>()
                > HISTORY_CHARS
        {
            return Err(format!(
                "history is {HISTORY_TURNS} turns and {HISTORY_CHARS} characters at most"
            ));
        }
        Ok(Ask {
            question,
            tools,
            model,
            max_calls,
            history,
        })
    }
}

/// Runs `ask` as `who` and answers `{answer, stopped, model, steps, tokens, cost, ms}`.
pub(super) async fn run(
    state: &Arc<State>,
    reads: &Reads,
    who: &Principal,
    via: &SignIn,
    ask: Ask,
) -> Result<Value> {
    let started = Instant::now();
    let Some(cx) = Extractor::from_env()? else {
        return Err(super::ToolError("no model is configured on this server".into()).into());
    };
    let cx = cx.with_model(ask.model);
    let call = |n: u64, method: &str, params: Value| {
        let body =
            json!({"jsonrpc": "2.0", "id": n, "method": method, "params": params}).to_string();
        async move {
            mcp(
                state,
                reads,
                who,
                (Writes::No, via),
                (None, true),
                (false, "memstore-agent", false),
                body.as_bytes(),
            )
            .await
            .unwrap_or(Value::Null)
        }
    };
    // The tools offered: those asked for that this caller is offered too, with the schemas `/mcp` lists.
    let listed = call(0, "tools/list", json!({})).await;
    let offered: Vec<Value> = listed["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| ask.tools.iter().any(|n| t["name"] == n.as_str()))
        .map(|t| {
            json!({"type": "function", "function": {
                "name": t["name"], "description": t["description"], "parameters": t["inputSchema"]}})
        })
        .collect();
    let offers = |name: &str| offered.iter().any(|t| t["function"]["name"] == name);

    let mut messages = vec![json!({"role": "system", "content": SYSTEM})];
    for (q, a) in &ask.history {
        messages.push(json!({"role": "user", "content": q}));
        messages.push(json!({"role": "assistant", "content": a}));
    }
    messages.push(json!({"role": "user", "content": ask.question}));

    let (mut steps, mut calls) = (Vec::<Value>::new(), 0usize);
    let (mut tokens_in, mut tokens_out, mut cost) = (0u64, 0u64, None::<f64>);
    let mut answer = String::new();
    let mut stopped = "answered";
    // Each turn either calls tools or answers; one past the cap is told to answer with what it has.
    for turn in 0..=ask.max_calls + 1 {
        let Some(left) = BUDGET.checked_sub(started.elapsed()) else {
            stopped = "time budget";
            break;
        };
        let mut body =
            json!({"messages": messages, "max_tokens": 2000, "usage": {"include": true}});
        if !offered.is_empty() {
            body["tools"] = json!(offered);
            if calls >= ask.max_calls || turn > ask.max_calls {
                body["tool_choice"] = json!("none");
            }
        }
        let v = match tokio::time::timeout(left, cx.chat(body)).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                eprintln!("agent run: the model failed: {e:#}");
                stopped = "model error";
                break;
            }
            Err(_) => {
                stopped = "time budget";
                break;
            }
        };
        tokens_in += v["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        tokens_out += v["usage"]["completion_tokens"].as_u64().unwrap_or(0);
        if let Some(c) = v["usage"]["cost"].as_f64() {
            cost = Some(cost.unwrap_or(0.0) + c);
        }
        let msg = &v["choices"][0]["message"];
        let wanted = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        if wanted.is_empty() {
            answer = msg["content"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            break;
        }
        messages
            .push(json!({"role": "assistant", "content": msg["content"], "tool_calls": wanted}));
        for c in &wanted {
            let name = c["function"]["name"].as_str().unwrap_or_default();
            let mut args: Value = c["function"]["arguments"]
                .as_str()
                .and_then(|a| serde_json::from_str(a).ok())
                .unwrap_or_else(|| json!({}));
            // A brief is read as it is: refreshing one runs the summariser and writes it.
            if let Some(a) = args.as_object_mut() {
                a.remove("refresh");
            }
            let t0 = Instant::now();
            let (text, error, ran) = if !offers(name) {
                (format!("{name} isn't available in this run"), true, false)
            } else if calls >= ask.max_calls {
                (
                    "The tool-call budget is spent: answer with what you have.".to_string(),
                    true,
                    false,
                )
            } else if started.elapsed() >= *BUDGET {
                (
                    "The time budget is spent: answer with what you have.".to_string(),
                    true,
                    false,
                )
            } else {
                calls += 1;
                let left = BUDGET.saturating_sub(started.elapsed());
                let r = tokio::time::timeout(
                    left,
                    call(
                        calls as u64,
                        "tools/call",
                        json!({"name": name, "arguments": args}),
                    ),
                )
                .await
                .unwrap_or_else(
                    |_| json!({"error": {"message": "The time budget ran out during this call."}}),
                );
                let text = r["result"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let error =
                    r["result"]["isError"].as_bool().unwrap_or(false) || !r["error"].is_null();
                let text = match r["error"]["message"].as_str() {
                    Some(m) if text.is_empty() => m.to_string(),
                    _ => text,
                };
                (text, error, true)
            };
            steps.push(json!({
                "tool": name, "args": args, "ran": ran, "error": error,
                "ms": t0.elapsed().as_millis() as u64, "chars": text.chars().count(),
                "text": cut(&text, TRACE_CHARS),
            }));
            messages.push(json!({"role": "tool", "tool_call_id": c["id"], "content": cut(&text, RESULT_CHARS)}));
        }
        if turn == ask.max_calls + 1 {
            stopped = "tool budget";
        }
    }
    Ok(json!({
        "answer": answer,
        "stopped": stopped,
        "model": ask.model,
        "tools": offered.iter().map(|t| t["function"]["name"].clone()).collect::<Vec<_>>(),
        "steps": steps,
        "tokens": {"in": tokens_in, "out": tokens_out},
        "cost": cost,
        "ms": started.elapsed().as_millis() as u64,
    }))
}

/// The first `n` characters of `s`, marked when cut.
fn cut(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}… [cut at {n} characters]", &s[..i]),
        None => s.to_string(),
    }
}
