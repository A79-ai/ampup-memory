//! `get_episode` in parts: a long episode (a 40-minute call is ~40 KB of turns) comes back a page of its units
//! (turns, messages, paragraphs, field changes) at a time, as its episode type splits it, so one tool result
//! never floods a caller's context. A short one comes back whole, as it always did.

use anyhow::Result;
use serde_json::{Value, json};

use super::ToolError;
use super::reads::Reads;
use crate::episode_types::{self, Cite, Unit};
use crate::{Episode, Principal, Session, Store};

/// A body up to this size comes back whole when no part is asked for; a page holds about this much text.
pub const PAGE_BYTES: usize = 12 * 1024;
/// The most units one call returns, whatever `limit` asks.
const MAX_LIMIT: usize = 200;
/// Units on each side of `around` when no `window` is given.
const WINDOW: usize = 5;

/// What `get_episode` returns for an episode id: the raw body when it is short and no part is asked for, else
/// the asked part (or the first page) as `{episode_id, type, units, from, to, text, next_offset?, note?}`.
/// `whole: true` is the raw body whatever its size. The episode is looked for in every scope read, the caller's
/// own first.
pub(super) async fn get(
    reads: &Reads,
    store: &Store,
    who: &Principal,
    args: &Value,
) -> Result<String> {
    let eid = args["episode_id"].as_str().unwrap_or_default();
    let Some((i, body)) = reads.episode_body(store, who, eid).await? else {
        return Ok(match of_facts(reads, store, who, args, eid).await? {
            Some(text) => text,
            None => NO_EPISODE.into(),
        });
    };
    page(reads.session_at(i), store, args, eid, &body).await
}

/// What `get_episode` answers for an id it can't read: none such, or one the caller may not read, alike.
const NO_EPISODE: &str = "no such episode (or not visible)";
/// Episodes a subject's answer reads out at most (as many as fit in a page together, at least one); the rest
/// it names, up to `NAMED`.
const SUBJECT_EPISODES: usize = 3;
const NAMED: usize = 20;

/// `get_episode` for a fact's subject (`rule:offer_revival`) or a fact's id instead of an episode: the episodes
/// its facts cite, newest first, read as an episode id reads them. Only facts the caller may read lead anywhere
/// (each cites only episodes they may read), so a subject whose facts are all hidden is None, as an unknown one
/// is. With a part asked for (offset, limit, around, window, whole), only the newest episode, in that part;
/// else the newest few that fit in a page together, and the others' ids. Facts and episodes are looked for in
/// every scope read.
pub(super) async fn of_facts(
    reads: &Reads,
    store: &Store,
    who: &Principal,
    args: &Value,
    id: &str,
) -> Result<Option<String>> {
    use crate::search::Col;
    let id = id.trim();
    if id.is_empty() {
        return Ok(None);
    }
    let one = [id.to_string()];
    let (mut facts, head, is_fact) =
        match reads.read_in(store, who, Col::FactId, &one, None).await? {
            f if !f.is_empty() => {
                let head = format!("Fact {id} ({} · {})", f[0].entity, f[0].predicate);
                (f, head, true)
            }
            _ if id.contains(':') => (
                reads.read_in(store, who, Col::Entity, &one, None).await?,
                id.to_string(),
                false,
            ),
            _ => return Ok(None),
        };
    // Newest fact first (the current version before what it replaced), so its episodes come first.
    facts.sort_by_key(|f| std::cmp::Reverse((f.valid_to.is_none(), f.valid_from)));
    let mut seen = std::collections::HashSet::new();
    let cited: Vec<String> = facts
        .iter()
        .flat_map(|f| f.source_episodes.iter())
        .filter(|e| seen.insert(e.as_str()))
        .cloned()
        .collect();
    let paged = ["offset", "limit", "around", "window", "whole"]
        .iter()
        .any(|k| !args[*k].is_null());
    let want = if paged { 1 } else { SUBJECT_EPISODES };
    let mut read: Vec<(String, (usize, Episode))> = {
        let first: Vec<String> = cited.iter().take(want).cloned().collect();
        let mut got = reads.episodes(store, who, &first).await?;
        first
            .into_iter()
            .filter_map(|e| got.remove(&e).map(|ep| (e, ep)))
            .collect()
    };
    if read.is_empty() {
        return Ok(None);
    }
    read.sort_by_key(|(_, (_, ep))| std::cmp::Reverse(ep.occurred_at));
    let mut parts = vec![];
    let mut size = 0;
    let mut shown = vec![];
    for (e, (i, ep)) in &read {
        let text = page(reads.session_at(*i), store, args, e, &ep.body).await?;
        if !shown.is_empty() && size + text.len() > PAGE_BYTES {
            break;
        }
        size += text.len();
        let day = chrono::DateTime::from_timestamp_micros(ep.occurred_at)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        // From another scope than the caller's own: which.
        let scope = match i {
            0 => String::new(),
            i => format!(", in {}", reads.scope_at(*i)),
        };
        parts.push(format!("--- episode {e} ({day}{scope})\n{text}"));
        shown.push(e.as_str());
    }
    let others: Vec<&str> = cited
        .iter()
        .map(String::as_str)
        .filter(|e| !shown.contains(e))
        .collect();
    let mut top = format!(
        "{head} {} {} episode{}; {}.",
        if is_fact { "cites" } else { "has facts citing" },
        cited.len(),
        if cited.len() == 1 { "" } else { "s" },
        match shown.len() {
            1 if cited.len() == 1 => "it follows".to_string(),
            1 => "the newest follows".to_string(),
            n => format!("the newest {n} follow, newest first"),
        }
    );
    if !others.is_empty() {
        top.push_str(&format!(
            "\nThe others (get_episode reads one by its id): {}{}",
            others
                .iter()
                .take(NAMED)
                .copied()
                .collect::<Vec<_>>()
                .join(", "),
            match others.len().saturating_sub(NAMED) {
                0 => String::new(),
                n => format!(", and {n} more"),
            }
        ));
    }
    Ok(Some(format!("{top}\n\n{}", parts.join("\n\n"))))
}

/// Episode `eid`'s `body` as `get_episode` returns it: whole when short and no part is asked for, else the part.
pub(super) async fn page(
    session: &Session,
    store: &Store,
    args: &Value,
    eid: &str,
    body: &[u8],
) -> Result<String> {
    let paged = ["offset", "limit", "around", "window"]
        .iter()
        .any(|k| !args[*k].is_null());
    if flag_arg(args, "whole") || (!paged && body.len() <= PAGE_BYTES) {
        return Ok(String::from_utf8_lossy(body).into_owned());
    }
    // The type comes from the source, which only the episode's row holds: read it only when paging.
    let source = source_of(session, store, eid).await?;
    let ep = Episode::new(source.as_deref().unwrap_or_default(), body, &[]);
    let Some((kind, units)) = source.as_ref().and_then(|_| {
        let t = episode_types::of(&ep)?;
        t.units(&ep)
            .ok()
            .filter(|u| !u.is_empty())
            .map(|u| (t.name(), u))
    }) else {
        // Nothing splits it (the engine's own documents): whole, as before.
        return Ok(String::from_utf8_lossy(body).into_owned());
    };
    let n = units.len();
    let (from, limit) = match &args["around"] {
        Value::Null => (
            index_arg(args, "offset")?.unwrap_or(0),
            index_arg(args, "limit")?,
        ),
        at => {
            let i = locate(&units, at).ok_or_else(|| {
                ToolError(format!(
                    "around: no unit {at} in this {kind} (its units are numbered 0 to {}; a message may also \
                     be named by its ts, and a unit by the pointer search shows, such as t12, p3 or @656)",
                    n - 1
                ))
            })?;
            let w = index_arg(args, "window")?.unwrap_or(WINDOW);
            (i.saturating_sub(w), Some(2 * w + 1))
        }
    };
    if from >= n {
        return Err(ToolError(format!(
            "offset {from}: this {kind} has {n} units, numbered 0 to {}",
            n - 1
        ))
        .into());
    }
    let mut lines = vec![];
    let mut size = 0;
    let mut cut = vec![];
    for u in &units[from..] {
        let mut line = line(u);
        let full = match limit {
            Some(l) => lines.len() >= l.clamp(1, MAX_LIMIT),
            // By size: at least one unit, then as many as fit in a page.
            None => !lines.is_empty() && size + line.len() > PAGE_BYTES,
        };
        if full {
            break;
        }
        // One unit longer than a page (a body its type can't split finely) is cut; `whole` reads it all.
        if line.len() > PAGE_BYTES {
            line.truncate(line.floor_char_boundary(PAGE_BYTES));
            line.push_str(" …");
            cut.push(u.index);
        }
        size += line.len() + 1;
        lines.push(line);
    }
    let to = from + lines.len() - 1;
    let mut v = json!({
        "episode_id": eid, "source": source, "type": kind, "units": n, "from": from, "to": to,
        "text": lines.join("\n"),
    });
    if !cut.is_empty() {
        v["cut"] = json!(cut);
        v["note"] = json!(format!(
            "Units {cut:?} are longer than a page and were cut at {PAGE_BYTES} bytes: call get_episode with \
             whole: true for the full body ({} bytes).",
            body.len()
        ));
        if to + 1 < n {
            v["next_offset"] = json!(to + 1);
        }
    } else if to + 1 < n {
        v["next_offset"] = json!(to + 1);
        v["note"] = json!(format!(
            "Units {from}-{to} of {n} ({kind}). For more, call get_episode with offset {} (and a limit), or \
             around a unit a fact cites (cite.turn, cite.paragraph, a message's ts) with a window.",
            to + 1
        ));
    }
    Ok(serde_json::to_string_pretty(&v).unwrap_or_default())
}

/// A non-negative integer argument, if given: a JSON number, or a number as text (a client holding the tool's
/// schema from before it declared the argument sends `"70"`).
pub(super) fn index_arg(args: &Value, key: &str) -> Result<Option<usize>> {
    match &args[key] {
        Value::Null => Ok(None),
        v => v
            .as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            .map(|i| Some(i as usize))
            .ok_or_else(|| ToolError(format!("{key} is a whole number, 0 or more")).into()),
    }
}

/// A true/false argument: a JSON boolean, or `"true"` as text, as [`index_arg`] takes a number.
pub(super) fn flag_arg(args: &Value, key: &str) -> bool {
    match &args[key] {
        Value::Bool(b) => *b,
        Value::String(s) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// The unit `at` names: its index (a number, or a number as text), a message's ts, or the pointer search
/// prints after a citation (`t192` a turn, `m<ts>` a message, `p3` a paragraph, `@656` an offset in running
/// text). A number that is no unit's index is taken as an offset, as a caller that drops the `@` means it.
fn locate(units: &[Unit], at: &Value) -> Option<usize> {
    let by_index = |i: u64| units.iter().position(|u| u.index as u64 == i);
    let by_offset = |o: usize| {
        units.iter().position(
            |u| matches!(&u.cite, Cite::Span { offset, len, .. } if (*offset..offset + len.max(&1)).contains(&o)),
        )
    };
    let by_ts = |s: &str| {
        units
            .iter()
            .position(|u| matches!(&u.cite, Cite::Message { ts, .. } if ts == s))
    };
    let num = |s: &str| s.parse::<usize>().ok();
    match at {
        Value::Number(n) => n
            .as_u64()
            .and_then(|i| by_index(i).or_else(|| by_offset(i as usize))),
        Value::String(s) => {
            let s = s.trim();
            by_ts(s)
                .or_else(|| num(s).and_then(|i| by_index(i as u64).or_else(|| by_offset(i))))
                .or_else(|| {
                    let (tag, rest) = s.split_at(s.chars().next()?.len_utf8());
                    match tag {
                        "@" => by_offset(num(rest)?),
                        "m" => by_ts(rest),
                        "t" => units.iter().position(
                            |u| matches!(&u.cite, Cite::Turn { turn, .. } if Some(*turn) == num(rest)),
                        ),
                        "p" => units.iter().position(
                            |u| matches!(&u.cite, Cite::Email { paragraph, .. } if Some(*paragraph) == num(rest)),
                        ),
                        _ => None,
                    }
                })
        }
        _ => None,
    }
}

/// One unit as a line: `[12 @03:49] Dana Reyes: text`, `[3 ts 1724100000.12] U04: text`, `[0] text`.
fn line(u: &Unit) -> String {
    let at = match &u.cite {
        Cite::Turn {
            at_ms: Some(ms), ..
        } => format!(" @{:02}:{:02}", ms / 60_000, ms / 1000 % 60),
        Cite::Message { ts, .. } if *ts != u.index.to_string() => format!(" ts {ts}"),
        _ => String::new(),
    };
    match &u.speaker {
        Some(s) => format!("[{}{at}] {s}: {}", u.index, u.text),
        None => format!("[{}{at}] {}", u.index, u.text),
    }
}

/// The source of episode `id` in this session, from its row.
async fn source_of(session: &Session, store: &Store, id: &str) -> Result<Option<String>> {
    for bs in store.episode_files(session.scope()).await?.values() {
        for b in bs.iter() {
            let ids = crate::strings(b, "episode_id");
            if let Some(i) = ids.iter().position(|e| e == id) {
                return Ok(Some(crate::strings(b, "source")[i].clone()));
            }
        }
    }
    Ok(None)
}
