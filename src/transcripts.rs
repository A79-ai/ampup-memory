//! Session transcripts: what a coding agent session actually ran and saw, kept for its author alone.
//!
//! A session's note (`<harness>:<repo>:<session>`) holds what was asked, what the agent said and the files it
//! changed; its end's trace holds each tool call with the start of its result. Neither keeps what the tools
//! returned, which is what a later pass checks a note against. Where the scope keeps transcripts
//! (`Settings::transcripts`, else the server's `MEMSTORE_TRANSCRIPTS`; off by default), the plugin's hooks
//! (`memstore_client::hook`) send each session's transcript as it grows, in parts: `POST /transcripts {harness,
//! repo, session_id, from, text}`, `text` the entries the transcript gained from entry `from` on (prompts, answers,
//! each tool call with its input and its result; never thinking), secret-looking words redacted on the machine and
//! again here. `{check: true}` asks whether the scope keeps them and how far the caller's parts of a session go.
//!
//! Each part is an episode of its author's own, readable by them alone: source
//! `<signature>memstore:transcript:<harness>:<repo>:<session>` (what follows `memstore:transcript:` is the session
//! note's own source, and no note may take a `memstore:` source), a version of document
//! `transcripts/<harness>/<session>/<from, 8 digits>`, so a session's parts list in order under
//! `transcripts/<harness>/<session>/` and a part sent again from the same entry replaces the one before it for
//! readers. Its body is running text, one turn to a paragraph, so `get_episode` reads it a page of turns at a time.
//! It is left out of passage search, extraction, a person's profile and the notes a session starts with, and
//! nothing that reads for the team reads it. Forgetting it (`forget_memory`), a retention policy, offboarding its
//! author and deleting the org remove it as any of their episodes; forgetting one version of a part forgets every
//! version of it ([`forgotten`]), and a part its author forgot, or that aged out, is never kept again: sent again
//! from the same entry it is refused, and a hook that lost its marker sends only what comes after it ([`held`]). An
//! org whose `pii` setting is on has personal identifiers replaced in it, as in all it stores.

use std::collections::HashSet;

use anyhow::Result;
use serde_json::{Value, json};

use crate::{Episode, MAIN, Principal, Scope, Store};

/// What a transcript's source holds after its signature, before the session note's own source.
pub const SOURCE: &str = "memstore:transcript:";
/// Where a transcript's parts are kept in versions: `transcripts/<harness>/<session>/<from>`.
pub const PREFIX: &str = "transcripts/";
/// The harnesses whose transcripts the hooks read tool calls from.
const HARNESSES: [&str; 3] = ["claude-code", "codex", "gemini"];
/// Longest part kept, in bytes (the hooks send at most 2 MB).
const MAX_TEXT: usize = 4 << 20;

pub struct Refused(pub String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::fmt::Debug for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refused {}

fn refuse(m: impl Into<String>) -> anyhow::Error {
    Refused(m.into()).into()
}

/// The author of a session transcript's part, from its source (`mcp:remember_memory:<author>:memstore:transcript:…`):
/// a service key's `key:<org>/<name>`, else everything up to the next `:`. None for any other source.
pub fn author_of(source: &str) -> Option<&str> {
    let rest = source
        .strip_prefix(crate::REMEMBER_SOURCE)?
        .strip_prefix(':')?;
    let skip = if rest.starts_with("key:") { 4 } else { 0 };
    let end = skip + rest[skip..].find(':')?;
    rest[end + 1..].starts_with(SOURCE).then(|| &rest[..end])
}

/// Whether `source` is a session transcript's.
pub fn is_transcript(source: &str) -> bool {
    author_of(source).is_some()
}

/// Which of the episodes `ids` are parts of `author`'s transcripts, by the sources their rows hold: a forgotten
/// part's ACL no longer names its author, its source still does.
async fn theirs(
    store: &Store,
    scope: &Scope,
    author: &str,
    ids: &[&str],
) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    if ids.is_empty() {
        return Ok(out);
    }
    for b in store
        .rows(&scope.namespace(), crate::EPISODES)
        .await?
        .rows
        .iter()
    {
        let (id, source) = (crate::strings(b, "episode_id"), crate::strings(b, "source"));
        for i in 0..id.len() {
            if ids.contains(&id[i].as_str()) && author_of(&source[i]) == Some(author) {
                out.insert(id[i].clone());
            }
        }
    }
    Ok(out)
}

/// Every version of document `key` there is, whoever may read it, newest first.
fn versions<'a>(vis: &'a crate::Vis, key: &str) -> Vec<&'a str> {
    vis.document_versions(key)
        .into_iter()
        .filter(|(k, _)| *k == key)
        .flat_map(|(_, v)| v)
        .collect()
}

/// After `author` forgot episode `id` (`forget_memory`): where it is a version of a part of one of their sessions'
/// transcripts, every other version of that part goes too, so none an earlier send kept (and a send with more
/// replaced) is read again. Only their own versions. How many it forgot besides `id`.
pub async fn forgotten(store: &Store, scope: &Scope, author: &str, id: &str) -> Result<usize> {
    store.invalidate(crate::SYSTEM_NS, "ledger");
    store.invalidate(&scope.namespace(), crate::EPISODES);
    let session = store.pin(scope, MAIN).await?;
    let vis = session.vis();
    let Some(doc) = vis.document_of(id).filter(|d| d.starts_with(PREFIX)) else {
        return Ok(0);
    };
    let mut n = 0;
    for v in versions(vis, doc) {
        let gone = vis
            .acls
            .get(v)
            .is_some_and(|a| a.iter().all(|e| e == crate::FORGOTTEN));
        if v != id
            && !gone
            && store
                .forget(scope, author, v, |s| author_of(s) == Some(author))
                .await?
        {
            n += 1;
        }
    }
    Ok(n)
}

/// The document a session's part from entry `from` is a version of.
pub fn document(harness: &str, session: &str, from: u64) -> String {
    format!("{PREFIX}{harness}/{session}/{from:08}")
}

/// A session's id: letters, digits, `-`, `_` and `.`, not starting with `.`.
fn id_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A repo's name as the hooks give it: no `:` or `/`, which would make the source ambiguous.
fn repo_ok(s: &str) -> bool {
    !s.trim().is_empty()
        && s.len() <= 200
        && !s.contains([':', '/', '\\'])
        && !s.chars().any(char::is_control)
}

/// `{check: true}` where the scope keeps them: given `harness` and `session_id`, also where the caller's parts of
/// that session stand, `from` the entry its last part starts at and `bytes` what the parts before it hold. A hook
/// that lost its marker sends again from there, replacing that part, instead of sending the session twice. Where
/// a part of theirs after the last they read is gone (forgotten, or aged out), `forgotten: true` and `bytes` what
/// the parts they read hold: the hook sends only what the transcript gains from now on, so none of it comes back.
async fn held(store: &Store, scope: &Scope, who: &Principal, args: &Value) -> Result<Value> {
    let mut v = json!({"kept": false, "off": false});
    let text = |k: &str| args[k].as_str().unwrap_or_default();
    let (harness, session) = (text("harness"), text("session_id"));
    if !HARNESSES.contains(&harness) || !id_ok(session) {
        return Ok(v);
    }
    let pinned = store.pin(scope, MAIN).await?;
    let vis = pinned.vis();
    let parts = vis.document_versions(&format!("{PREFIX}{harness}/{session}/"));
    let read: Vec<&str> = parts
        .iter()
        .filter(|(_, vs)| vs.iter().any(|e| vis.can_see_episode(e, who)))
        .map(|(k, _)| *k)
        .collect();
    // The versions of the parts after the last one they read: any of theirs was forgotten or aged out.
    let after: Vec<&str> = parts
        .iter()
        .filter(|(k, _)| read.last().is_none_or(|last| k > last))
        .flat_map(|(_, vs)| vs.iter().copied())
        .collect();
    let gone = !theirs(store, scope, who.user(), &after).await?.is_empty();
    let (counted, from) = match (gone, read.split_last()) {
        (true, _) => (&read[..], None),
        (false, Some((last, before))) => (
            before,
            last.rsplit('/').next().and_then(|t| t.parse::<u64>().ok()),
        ),
        (false, None) => return Ok(v),
    };
    let mut bytes = 0;
    for key in counted {
        if let Some((_, _, body)) = store.document(scope, who, key, None).await? {
            bytes += body.len();
        }
    }
    match from {
        Some(from) => v["from"] = json!(from),
        None if gone => v["forgotten"] = json!(true),
        None => return Ok(v),
    }
    v["bytes"] = json!(bytes);
    Ok(v)
}

/// `POST /transcripts`: keeps part of the caller's session transcript `args` as their own episode, signed `signed`
/// and readable by `acl`. Answers what was kept; `{kept: false, off: true}` where the scope doesn't keep them.
pub async fn keep(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    acl: &[String],
    signed: &str,
    args: &Value,
) -> Result<Value> {
    // Unread, nothing is kept: the scope may have them off.
    let folded = store.directory().await?;
    let dir = &folded.dir;
    if !dir.keeps_transcripts(scope) {
        return Ok(json!({"kept": false, "off": true}));
    }
    if args["check"].as_bool() == Some(true) {
        return held(store, scope, who, args).await;
    }
    let text = |k: &str| args[k].as_str().unwrap_or_default();
    let harness = text("harness");
    if !HARNESSES.contains(&harness) {
        return Err(refuse(format!(
            "harness is one of {}",
            HARNESSES.join(", ")
        )));
    }
    let session = text("session_id");
    if !id_ok(session) {
        return Err(refuse(
            "session_id is an id (letters, digits, '-', '_' and '.')",
        ));
    }
    let repo = text("repo");
    if !repo_ok(repo) {
        return Err(refuse("repo is a name, without ':' or '/'"));
    }
    let Some(from) = args["from"].as_u64() else {
        return Err(refuse(
            "from is the transcript entry the part starts at, 0 or more",
        ));
    };
    let part = text("text");
    if part.trim().is_empty() || part.len() > MAX_TEXT {
        return Err(refuse(format!(
            "text is the part, at most {} MB",
            MAX_TEXT >> 20
        )));
    }
    let body = crate::hook::redact(part);
    let body = match dir.redacts_pii(scope) {
        true => crate::pii::redact(&body).0,
        false => body,
    };
    let source = format!("{signed}{SOURCE}{harness}:{repo}:{session}");
    let doc = document(harness, session, from);
    // The same part sent again (its answer lost) is no new version.
    let current = store.document(scope, who, &doc, None).await?;
    if let Some((id, _, now)) = &current
        && now == body.as_bytes()
    {
        return Ok(json!({"kept": true, "episode_id": id, "document": doc}));
    }
    // A part they forgot (or that aged out) isn't kept again.
    if current.is_none() {
        let pinned = store.pin(scope, MAIN).await?;
        if !theirs(store, scope, who.user(), &versions(pinned.vis(), &doc))
            .await?
            .is_empty()
        {
            return Ok(json!({"kept": false, "off": false, "forgotten": true, "document": doc}));
        }
    }
    let acl: Vec<&str> = acl.iter().map(String::as_str).collect();
    let ep = Episode::sourced(&source, body.as_bytes(), &acl)
        .version_of(&doc)
        .without_text_index();
    crate::episode_types::check(&ep).map_err(|e| refuse(format!("{e:#}")))?;
    store
        .commit(scope, who.user(), std::slice::from_ref(&ep), &[])
        .await?;
    Ok(json!({"kept": true, "episode_id": ep.episode_id, "document": doc}))
}
