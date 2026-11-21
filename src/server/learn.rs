//! Adding to memory by hand, and what it learned from it: the Studio's "Add to memory" (files dropped in, text pasted,
//! a recording's transcript) and "Remember this" on an Ask answer, before any source is connected.
//!
//! - `POST /learn {items, for?}` writes each item as an episode through the path `/remember` takes
//!   ([`super::commit_remembered`]: the caller's own memory by default, `for: "team"` everyone's who reads it; signed
//!   with who wrote it; personal identifiers redacted where the scope asks). A pasted text or a file is filed under
//!   [`crate::origin::LEARNED`], so it reads as the person's own statement, as a fact they add in Studio does, not as
//!   an agent's note; an Ask answer kept stays a note (its words are the model's). Each is filed as a starter
//!   document (`starter/added/...`), so extraction reads it wherever it reads anything ([`crate::extract::STARTER`]),
//!   even where it reads nothing else no goal names. The same text added again is the same episode, not a new one.
//!   Items are `file` (text, by its name), `paste`, `answer` (a question and its answer) and `recording`: a
//!   recording's transcript (`turns` of `{speaker, start_ms?, text}`, the media file's `sha256`, `title?`,
//!   `recorded_at?`), filed as one call or meeting ([`crate::episode_types::sales_call`]) whose facts cite its turns.
//!   It is what its speakers said, not the person's own statement: filed under [`crate::origin::RECORDED`], not
//!   [`crate::origin::LEARNED`]. Its speakers are as given (`speaker_0`), listed as no one: no person is made of a
//!   label. It is keyed on the media's digest, so the same recording again is the episode it was ([`file_recording`],
//!   which a transcriber calls with the turns it heard). A `file` that is a recording (`.m4a`, `.mp4` and the
//!   like, at most [`MAX_MEDIA`]) is transcribed where the server is set to ([`crate::transcribe`]) and filed the
//!   same way, keyed on the digest of its bytes, which is looked up before it is sent: the same file again is never
//!   transcribed twice. Elsewhere it is refused, saying audio and video aren't read.
//!   Answers each item's episode id (`added`), and each item refused with why (`refused`), then wakes extraction
//!   through the ledger ([`crate::extract::ADDED_WAKE`]): it reads what was added within seconds.
//! - `POST /learn/status {episode_ids}` answers what extraction made of them so far: a receipt, per episode,
//!   whether it has been read (`pending`, `read`, `failed`, `not_read`: nothing here reads it, as
//!   [`crate::extract::reads`] decides, the scope's schema included), the facts it gave that the caller may
//!   read, their entities (and whether memory knew them before), and the facts it stated again that memory already
//!   held (where reconcile recorded the restatement: a source independent of the fact's own).
//!
//! An ingest key may use neither: the receipt reads memory. A queued memory (an inbox) refuses: what it would
//! learn comes at the next ingest, with nothing to show now.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use anyhow::Result;
use base64::Engine;
use serde_json::{Value, json};
use sha2::Digest;

use super::{State, ToolError};
use crate::search::{Col, Filter};
use crate::{Episode, Principal, Scope, Session, Store};

/// Most items one request adds.
const MAX_ITEMS: usize = 20;
/// Most episodes one status request asks about.
const MAX_STATUS: usize = 50;
/// Largest text one item may hold: what starter memory takes (and about what extraction reads of one episode).
const MAX_TEXT: usize = super::STARTER_MAX_BYTES;
/// What a file's name may end in to be read as text. Anything else is refused, saying what is read.
const TEXT_TYPES: [&str; 12] = [
    "txt", "text", "md", "markdown", "csv", "tsv", "json", "jsonl", "ndjson", "yaml", "yml", "log",
];
/// What a file's name ends in where it is a recording, as Studio names them: transcribed where the server is set to
/// ([`crate::transcribe`]) and its type is one [`TRANSCRIBED`] names, else refused, saying why.
const MEDIA_TYPES: [&str; 14] = [
    "mp4", "mov", "m4v", "webm", "mkv", "avi", "mp3", "wav", "m4a", "aac", "ogg", "oga", "opus",
    "flac",
];
/// The recordings sent to be transcribed: the rest of [`MEDIA_TYPES`] are refused.
const TRANSCRIBED: [&str; 12] = [
    "mp4", "mov", "m4v", "webm", "mp3", "wav", "m4a", "aac", "ogg", "oga", "opus", "flac",
];
/// Largest recording one item may hold: what fits, as base64, in a request `/learn` takes.
const MAX_MEDIA: usize = super::MAX_INGEST_BODY / 4 * 3;
/// Where what is added is filed: a starter document per person (a hash of who added it) and item.
const DOCUMENTS: &str = crate::extract::ADDED;

/// Wakes the read of what was just added on this server (`crate::extract::run_added`), so it looks at the ledger now
/// rather than at its next tick.
pub(super) static WAKE: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);

/// Whether this server extracts at all (a model configured, `AMPUP_MEMORY_EXTRACT` not `off`), as it started.
static EXTRACTS: LazyLock<bool> =
    LazyLock::new(|| crate::extract::Extractor::from_env().is_ok_and(|e| e.is_some()));

/// One item to add, read and checked.
struct Item {
    kind: &'static str,
    name: String,
    text: String,
    /// A recording's transcript (`recording`), filed as a call rather than as text.
    recording: Option<Recording>,
    /// A recording's file (its type, its bytes), transcribed and then filed as one ([`file_media`]).
    media: Option<(String, Vec<u8>)>,
}

/// A recording's transcript: the media file it is of (its name, and its SHA-256 in hex, which keys it), who spoke
/// when saying what, and its title and when it was recorded (microseconds) where they are known. The caller gives
/// the turns; a transcriber fills them from the media and files it the same way ([`file_recording`]).
pub(super) struct Recording {
    pub name: String,
    pub sha256: String,
    pub title: Option<String>,
    pub recorded_at: Option<i64>,
    pub turns: Vec<Turn>,
    /// The language its speakers speak, where a transcriber heard it (`eng`).
    pub language: Option<String>,
}

/// One turn of a recording: who spoke (a transcriber's label, such as `speaker_0`, is kept as it is), when it
/// starts in milliseconds, and what they said.
pub(super) struct Turn {
    pub speaker: String,
    pub start_ms: Option<u64>,
    pub text: String,
}

/// An item as filed: its episode, who reads it, whether it is new (not the episode the same item already was), its
/// source and document, and the bytes stored.
pub(super) struct Filed {
    pub id: String,
    pub readers: String,
    pub new: bool,
    pub source: String,
    pub key: String,
    pub bytes: usize,
}

/// `POST /learn`: add each item as an episode the caller wrote, in `scope`. Answers `{added: [{name, kind,
/// episode_id, source, bytes, new}], refused: [{name, reason}], scope, readers, extraction}`; `new` is false where
/// the same text was already there (the same episode). Refuses the whole request (a [`ToolError`]) only when it
/// isn't one: no items, too many, or `for` neither "me" nor "team".
pub(super) async fn add(
    state: &std::sync::Arc<State>,
    store: &Store,
    scope: &Scope,
    who: &Principal,
    args: &Value,
) -> Result<Value> {
    let items = args["items"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            ToolError(
                "body is {items: [{kind: file|paste|answer|recording, name?, text? | base64? | question, answer | \
                 sha256, turns: [{speaker, start_ms?, text}], title?, recorded_at?}], for?}"
                    .into(),
            )
        })?;
    if items.len() > MAX_ITEMS {
        return Err(ToolError(format!("at most {MAX_ITEMS} items a request")).into());
    }
    let whose = match args["for"].as_str() {
        None | Some("me") => "me",
        Some("team") => "team",
        Some(other) => {
            return Err(ToolError(format!("for is \"me\" or \"team\", not {other:?}")).into());
        }
    };
    let author = author_of(who);
    let (mut added, mut refused) = (vec![], vec![]);
    let (mut uploading, mut transcribing) = (vec![], vec![]);
    // Each kept, as extraction will see it, to say whether anything reads it.
    let mut kept: Vec<Episode> = vec![];
    let mut readers = String::new();
    let mut fresh = false;
    for raw in items {
        // A piece of a recording sent in parts ([`super::upload`]): kept until the last, then transcribed apart.
        if raw["kind"].as_str() == Some("part") {
            match super::upload::part(state, store, scope, who, whose, &author, raw).await {
                Ok(super::upload::Part::Uploading(v)) => uploading.push(v),
                Ok(super::upload::Part::Transcribing(v)) => transcribing.push(v),
                Err(e) => match refusal(&e) {
                    Some(why) => refused.push(json!({"name": item_name(raw), "reason": why})),
                    None => return Err(e),
                },
            }
            continue;
        }
        let item = match read(raw) {
            Ok(i) => i,
            Err(why) => {
                refused.push(json!({"name": item_name(raw), "reason": why}));
                continue;
            }
        };
        let commit = match (&item.recording, &item.media) {
            (Some(rec), _) => file_recording(state, store, scope, who, whose, &author, rec).await,
            (None, Some((ext, bytes))) => {
                file_media(
                    state, store, scope, who, whose, &author, &item.name, ext, bytes, None,
                )
                .await
            }
            (None, None) => file_text(state, store, scope, who, whose, &author, &item).await,
        };
        match commit {
            Ok(f) => {
                fresh |= f.new;
                readers = f.readers;
                let mut ep = Episode::new(&super::provenance(who, &f.source), b"", &[]);
                ep.episode_id = f.id.clone();
                ep.document = Some(f.key);
                kept.push(ep);
                added.push(
                    json!({"name": item.name, "kind": item.kind, "episode_id": f.id,
                                  "source": f.source, "bytes": f.bytes, "new": f.new}),
                );
            }
            // What the item itself is refused for (a body no episode type reads, say): that item alone.
            Err(e) => match refusal(&e) {
                Some(why) => refused.push(json!({"name": item.name, "reason": why})),
                None => return Err(e),
            },
        }
    }
    let reading = to_read(store, scope, &kept).await?;
    // Something new to read: said in the ledger, where the server that extracts finds it within seconds, whichever
    // server that is (one that only serves never extracts), and that one is woken now if it is this one.
    if fresh && !reading.is_empty() {
        let actor = format!("{}{}", crate::extract::ADDED_WAKE, who.user());
        match store.commit_record(scope, &actor, &[]).await {
            Ok(_) => WAKE.notify_one(),
            Err(e) => eprintln!("learn: {scope}'s extraction couldn't be woken: {e:#}"),
        }
    }
    for a in &mut added {
        a["read"] = json!(reading.contains(a["episode_id"].as_str().unwrap_or_default()));
    }
    let (mode, _) = extraction(store, scope, !reading.is_empty()).await;
    Ok(json!({
        "added": added,
        "refused": refused,
        "uploading": uploading,
        "transcribing": transcribing,
        "scope": scope.to_string(),
        "readers": readers,
        "extraction": mode,
    }))
}

/// File a text item (a file, a paste, an answer) as the person's starter document ([`filed`]).
async fn file_text(
    state: &State,
    store: &Store,
    scope: &Scope,
    who: &Principal,
    whose: &str,
    author: &str,
    item: &Item,
) -> Result<Filed> {
    let (source, key) = filed(item, author);
    // Already here, as this caller sees it: the same episode, said so rather than written again.
    let before = store.document(scope, who, &key, None).await?;
    let call = json!({"source": source, "body": item.text, "document": key, "for": whose});
    let call = super::remember_for(state, who, &call)?;
    let (id, readers) = match item.kind {
        "answer" => super::commit_remembered(state, store, scope, who, &call).await?,
        _ => super::commit_learned(state, store, scope, who, &call).await?,
    };
    let new = before.as_ref().is_none_or(|(b, _, _)| *b != id);
    Ok(Filed {
        id,
        readers,
        new,
        source,
        key,
        bytes: item.text.len(),
    })
}

/// File a recording's transcript as one call or meeting ([`crate::episode_types::sales_call`]: its turns citable by
/// number, time and speaker), by `who` for `whose` ("me" or "team"), as the starter document
/// `starter/added/<author>/recording/<digest>` keyed on the media's SHA-256: a recording already here, as this caller
/// sees it, is the episode it was (`new` false) whatever its transcript says this time, so a transcriber asks
/// [`recorded`] before it transcribes. Its source is `memstore:recording:<digest>` under who added it
/// ([`crate::origin::RECORDED`]): its speakers' words, not theirs. Its speakers are listed as no one, so the engine
/// writes no person for a label, only the meeting (`meeting:recording-<digest>`), its title and when it was recorded
/// (else added); the episode is about that meeting ([`recording_about`]), so those facts are kept. Personal identifiers
/// are redacted in each turn where the scope asks, before the transcript is laid out, so redaction can't touch its
/// times. A transcript over [`MAX_TEXT`] is refused (a [`ToolError`]).
pub(super) async fn file_recording(
    state: &State,
    store: &Store,
    scope: &Scope,
    who: &Principal,
    whose: &str,
    author: &str,
    rec: &Recording,
) -> Result<Filed> {
    let (source, key) = recording_filed(rec, author);
    if let Some(f) = recorded(state, store, scope, who, whose, author, rec).await? {
        return Ok(f);
    }
    let redacts = store.directory().await?.dir.redacts_pii(scope);
    let clean = |t: &str| -> String {
        if !redacts {
            return t.to_string();
        }
        let (out, found) = crate::pii::redact(t);
        found.count();
        out
    };
    let title = rec.title.clone().unwrap_or_else(|| {
        let stem = rec
            .name
            .rsplit_once('.')
            .map_or(rec.name.as_str(), |(s, _)| s);
        if stem.trim().is_empty() {
            "Recording".to_string()
        } else {
            stem.trim().to_string()
        }
    });
    let turns: Vec<Value> = rec
        .turns
        .iter()
        .map(|t| {
            let mut v = json!({"speaker": clean(&t.speaker), "text": clean(&t.text)});
            if let Some(ms) = t.start_ms {
                v["start_ms"] = json!(ms);
            }
            v
        })
        .collect();
    // The body sales_call reads: nothing in it changes from one sending to the next, so the same transcript is the
    // same episode.
    let mut body = json!({
        "meeting": recording_meeting(rec),
        "title": clean(&title),
        "participants": [],
        "turns": turns,
        "recording": {"name": rec.name, "sha256": rec.sha256},
    });
    if let Some(l) = &rec.language {
        body["language"] = json!(l);
    }
    let body = body.to_string();
    if body.len() > MAX_TEXT {
        return Err(ToolError(format!(
            "it's {} KB of transcript: at most {} KB is added at a time",
            body.len().div_ceil(1024),
            MAX_TEXT / 1024
        ))
        .into());
    }
    let call = json!({"source": source, "body": body, "document": key, "for": whose});
    let call = super::remember_for(state, who, &call)?;
    let about = recording_about(store, scope, rec).await?;
    let (id, readers) =
        super::commit_recorded(state, store, scope, who, &call, rec.recorded_at, about).await?;
    Ok(Filed {
        id,
        readers,
        new: true,
        source,
        key,
        bytes: body.len(),
    })
}

/// Transcribe a recording's file and file it as [`file_recording`] does, keyed on the digest of its bytes (worked out
/// here, never taken from the caller): one `who` added before is that episode (`new` false) and is not sent to be
/// transcribed again. One line is logged per transcription (the scope, the file's bytes, the seconds of audio),
/// and one where the file was already here. What the transcriber refuses, or a file it hears no speech in, is
/// refused (a [`ToolError`]); a transcript over [`MAX_TEXT`] too, though it was paid for.
#[allow(clippy::too_many_arguments)]
pub(super) async fn file_media(
    state: &State,
    store: &Store,
    scope: &Scope,
    who: &Principal,
    whose: &str,
    author: &str,
    name: &str,
    ext: &str,
    bytes: &[u8],
    within: Option<std::time::Duration>,
) -> Result<Filed> {
    let mut rec = Recording {
        name: name.to_string(),
        sha256: hex(&sha2::Sha256::digest(bytes)),
        title: None,
        recorded_at: None,
        turns: vec![],
        language: None,
    };
    if let Some(f) = recorded(state, store, scope, who, whose, author, &rec).await? {
        eprintln!(
            "transcribe {scope}: {} bytes of .{ext} already here as episode {}: not transcribed again",
            bytes.len(),
            f.id
        );
        return Ok(f);
    }
    let Ok(Some(t)) = &*crate::transcribe::TRANSCRIBER else {
        return Err(ToolError(NOT_TRANSCRIBED.into()).into());
    };
    // Transcribing is model work the org pays for: refused before it is spent, past its allowance or its plan's end.
    if let Some(l) = crate::limits::model_gate(scope.org_name()) {
        return Err(l.into());
    }
    let started = std::time::Instant::now();
    let heard = match within {
        Some(w) => t.transcribe_within(ext, bytes, w).await,
        None => t.transcribe(ext, bytes).await,
    };
    let took = started.elapsed().as_secs_f64();
    let heard = match heard {
        Ok(h) => h,
        Err(why) => {
            eprintln!(
                "transcribe {scope}: {} bytes of .{ext} refused after {took:.1} s: {why}",
                bytes.len()
            );
            return Err(ToolError(why).into());
        }
    };
    // Paid for now, whatever comes of it: counted against the org's monthly model tokens. Where the service didn't say
    // how long the audio is, the last turn's start stands in.
    let seconds = heard.seconds.unwrap_or_else(|| {
        let last = heard.turns.last().map_or(0, |t| t.start_ms) as f64 / 1000.0;
        eprintln!("transcribe {scope}: the service didn't say how long the audio is: counted as {last:.1} s");
        last
    });
    let tokens = crate::transcribe::tokens_for(seconds);
    crate::limits::count_for(scope.org_name(), "transcription", tokens);
    eprintln!(
        "transcribe {scope}: {} bytes of .{ext}, {} s of audio ({tokens} model tokens), {} turns, language {}, in {took:.1} s",
        bytes.len(),
        heard.seconds.map_or("?".to_string(), |s| format!("{s:.1}")),
        heard.turns.len(),
        heard.language.as_deref().unwrap_or("?"),
    );
    if heard.turns.is_empty() {
        return Err(ToolError("no speech found in it".into()).into());
    }
    rec.language = heard.language;
    rec.turns = heard
        .turns
        .into_iter()
        .map(|t| Turn {
            speaker: t.speaker,
            start_ms: Some(t.start_ms),
            text: t.text,
        })
        .collect();
    file_recording(state, store, scope, who, whose, author, &rec).await
}

/// Why an item (or an upload) is refused, as its receipt says it: what it was refused for ([`ToolError`]), or the
/// org's limit it ran into (a recording past the org's model tokens or its plan's end). None: a failure.
pub(super) fn refusal(e: &anyhow::Error) -> Option<String> {
    match e.downcast_ref::<ToolError>() {
        Some(m) => Some(m.0.clone()),
        None => crate::limits::LimitReached::of(e).map(|l| l.message.clone()),
    }
}

/// Why a recording's file isn't read where this server doesn't transcribe.
const NOT_TRANSCRIBED: &str =
    "audio and video aren't read yet: add a transcript of it as text instead";

/// The recording `rec` is, if `who` added it here before (its media's digest, whatever its name or transcript):
/// that episode, as [`file_recording`] answers it, `new` false. Nothing is written: its stored text is the version
/// there, so committing it again only says who reads it.
pub(super) async fn recorded(
    state: &State,
    store: &Store,
    scope: &Scope,
    who: &Principal,
    whose: &str,
    author: &str,
    rec: &Recording,
) -> Result<Option<Filed>> {
    let (source, key) = recording_filed(rec, author);
    let Some((id, _, body)) = store.document(scope, who, &key, None).await? else {
        return Ok(None);
    };
    let bytes = body.len();
    let body = String::from_utf8_lossy(&body).into_owned();
    let call = json!({"source": source, "body": body, "document": key, "for": whose});
    let call = super::remember_for(state, who, &call)?;
    let about = recording_about(store, scope, rec).await?;
    let (_, readers) = super::commit_recorded(state, store, scope, who, &call, None, about).await?;
    Ok(Some(Filed {
        id,
        readers,
        new: false,
        source,
        key,
        bytes,
    }))
}

/// `POST /learn/status`: the receipt of `episode_ids` as the caller may read them (an id they can't read answers
/// as `unknown`, as one that doesn't exist does). `done` once none is pending.
pub(super) async fn status(
    state: &State,
    store: &Store,
    session: &Session,
    who: &Principal,
    args: &Value,
) -> Result<Value> {
    let ids: Vec<String> = args["episode_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
        .map(String::from)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let upload_ids: Vec<String> = args["upload_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
        .map(String::from)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if ids.is_empty() && upload_ids.is_empty() {
        return Err(ToolError("body is {episode_ids: [...], upload_ids?: [...]}".into()).into());
    }
    if ids.len() > MAX_STATUS || upload_ids.len() > MAX_STATUS {
        return Err(ToolError(format!("at most {MAX_STATUS} episodes a request")).into());
    }
    let scope = session.scope().clone();
    // Recordings sent in parts, as their transcription stands (an episode id once filed).
    let uploads = super::upload::states(store, &scope, &author_of(who), &upload_ids).await?;
    let uploads_done = uploads.iter().all(|u| u["status"] != "transcribing");
    if ids.is_empty() {
        return Ok(json!({"done": uploads_done, "items": [], "uploads": uploads}));
    }
    let vis = session.vis();
    let seen: Vec<String> = ids
        .iter()
        .filter(|id| vis.can_see_episode(id, who))
        .cloned()
        .collect();
    let set: HashSet<String> = seen.iter().cloned().collect();
    let reads = match set.is_empty() {
        true => Default::default(),
        false => store.episode_reads(session, &set).await?,
    };
    // The facts citing them that the caller may read, current ones: a column scan for each id, then exactly.
    let mut facts = citing(store, session, who, &seen).await?;
    // A read that found facts, none of which shows here: the read is in the ledger this session pinned, but its
    // facts may be beyond this server's cached copy of the facts table (another server committed them, and the
    // two are refreshed apart). Read that again first, rather than tell the person the read found nothing: the
    // facts were committed with the read, so a copy loaded now holds them.
    let unseen = seen.iter().any(|id| {
        reads.get(id).is_some_and(|rows| {
            rows.iter()
                .any(|(by, n)| *n > 0 && !by.starts_with("compile:"))
        }) && !facts
            .iter()
            .any(|f| f.source_episodes.iter().any(|e| e == id))
    });
    if unseen {
        store.invalidate(&scope.namespace(), crate::FACTS);
        facts = citing(store, session, who, &seen).await?;
    }
    // Each entity the facts are about, and whether memory knew it before: a fact about it the caller may read
    // that cites none of these episodes.
    let entities: Vec<String> = facts
        .iter()
        .map(|f| f.entity.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let known_before: HashSet<String> = match entities.is_empty() {
        true => HashSet::new(),
        false => session
            .read_in(store, who, Col::Entity, &entities, None)
            .await?
            .into_iter()
            .filter(|f| !f.source_episodes.iter().all(|e| set.contains(e)))
            .map(|f| f.entity)
            .collect(),
    };
    // What each stated again that memory already held (reconcile's records), as the caller may read those facts.
    let restated: BTreeMap<&str, Vec<String>> = seen
        .iter()
        .map(|id| {
            (
                id.as_str(),
                vis.restated_by(id).into_iter().map(String::from).collect(),
            )
        })
        .collect();
    let known_ids: Vec<String> = restated.values().flatten().cloned().collect();
    let known_facts = match known_ids.is_empty() {
        true => vec![],
        false => {
            session
                .read_in(store, who, Col::FactId, &known_ids, Some(i64::MAX))
                .await?
        }
    };
    let refusals = crate::extract::refusals(&scope);
    let fact = |f: &crate::Fact| json!({"fact_id": f.fact_id, "entity": f.entity, "predicate": f.predicate, "value": f.value});
    // Where each stands with extraction, as its reads say: read (or citing facts already), given up on, or neither.
    let stands: BTreeMap<&str, &str> = seen
        .iter()
        .map(|id| {
            let rows = reads.get(id).map(Vec::as_slice).unwrap_or_default();
            let read = rows
                .iter()
                .any(|(by, n)| *n != crate::FAILED_TRY && !by.starts_with("compile:"));
            let failed = rows
                .iter()
                .filter(|(by, n)| *n == crate::FAILED_TRY && !by.starts_with("compile:"))
                .count();
            let cited = facts
                .iter()
                .any(|f| f.source_episodes.iter().any(|e| e == id));
            let state = if read || cited {
                "read"
            } else if failed >= crate::EXTRACT_TRIES {
                "failed"
            } else {
                "unread"
            };
            (id.as_str(), state)
        })
        .collect();
    // Those not read yet that extraction will read (pending), as it picks them; the rest it never will (not_read).
    let unread: Vec<String> = stands
        .iter()
        .filter(|(_, s)| **s == "unread")
        .map(|(id, _)| id.to_string())
        .collect();
    let reading = match unread.is_empty() {
        true => HashSet::new(),
        false => {
            let eps: Vec<Episode> = session
                .episode_sources(store, who, &unread)
                .await?
                .into_iter()
                .map(|(id, (source, at))| {
                    let mut e = Episode::new(&source, b"", &[]);
                    e.document = vis.document_of(&id).map(String::from);
                    e.episode_id = id;
                    e.occurred_at = at;
                    e
                })
                .collect();
            to_read(store, &scope, &eps).await?
        }
    };
    let (mode, why) = extraction(
        store,
        &scope,
        !reading.is_empty() || stands.values().any(|s| *s == "read"),
    )
    .await;
    let mut pending = false;
    let items: Vec<Value> = ids
        .iter()
        .map(|id| {
            if !set.contains(id) {
                return json!({"episode_id": id, "status": "unknown"});
            }
            let theirs: Vec<&crate::Fact> = facts
                .iter()
                .filter(|f| f.source_episodes.iter().any(|e| e == id))
                .collect();
            let state = match stands[id.as_str()] {
                "unread" if reading.contains(id) => {
                    pending = true;
                    "pending"
                }
                "unread" => "not_read",
                s => s,
            };
            let mut about: BTreeMap<&str, usize> = BTreeMap::new();
            for f in &theirs {
                *about.entry(f.entity.as_str()).or_default() += 1;
            }
            let known: Vec<Value> = known_facts
                .iter()
                .filter(|f| restated[id.as_str()].contains(&f.fact_id))
                .map(fact)
                .collect();
            let mut v = json!({
                "episode_id": id,
                "status": state,
                "facts": theirs.iter().map(|f| fact(f)).collect::<Vec<_>>(),
                "entities": about.iter().map(|(e, n)| json!({"entity": e, "facts": n,
                    "new": !known_before.contains(*e)})).collect::<Vec<_>>(),
                "known": known,
            });
            // Why its facts were dropped, where this server refused some of the last episode it read.
            if let Some(r) = refusals
                .last_refused
                .as_ref()
                .filter(|r| r.episode_id.as_deref() == Some(id.as_str()))
            {
                v["refused"] = json!({"facts": r.refused, "why": r.why});
            }
            if state == "failed" {
                v["reason"] = json!("extraction gave up on it after repeated failures");
            } else if state == "not_read" {
                v["reason"] = json!(why);
            }
            v
        })
        .collect();
    // On a server that doesn't extract (it doesn't hold the writer lease), what the one that does commits shows here
    // only once this one's cached tables are read again: while a person waits on a receipt, that is at its next
    // look, not when they next age out.
    if pending && !state.writer.holds() {
        fresh(store, &scope);
    }
    Ok(
        json!({"done": !pending && uploads_done, "extraction": mode, "items": items, "uploads": uploads}),
    )
}

/// The current facts citing any of `ids` that `who` may read, notes' own facts aside: a column scan for each id,
/// then exactly.
async fn citing(
    store: &Store,
    session: &Session,
    who: &Principal,
    ids: &[String],
) -> Result<Vec<crate::Fact>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let cites = Filter::Or(
        ids.iter()
            .map(|id| Filter::Contains(Col::Sources, id.clone()))
            .collect(),
    );
    Ok(session
        .read_where(store, who, &cites, Some(i64::MAX))
        .await?
        .into_iter()
        .filter(|f| !crate::is_note_fact(f))
        .collect())
}

/// How often a scope's tables are read again for a receipt waiting on another server's extraction: each is a
/// catalog load.
const FRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(3);

/// Have `scope`'s next read see what was committed since (the ledger, its reads and facts), at most every
/// [`FRESH_EVERY`].
fn fresh(store: &Store, scope: &Scope) {
    static LAST: LazyLock<std::sync::Mutex<BTreeMap<String, std::time::Instant>>> =
        LazyLock::new(Default::default);
    let ns = scope.namespace();
    {
        let mut last = LAST.lock().unwrap();
        if last.get(&ns).is_some_and(|t| t.elapsed() < FRESH_EVERY) {
            return;
        }
        last.insert(ns.clone(), std::time::Instant::now());
    }
    store.invalidate(crate::SYSTEM_NS, "ledger");
    store.invalidate(&ns, crate::EXTRACTIONS);
    store.invalidate(&ns, crate::FACTS);
}

/// Of `eps` (as extraction sees them: source, document, when), those extraction will read for facts
/// ([`crate::extract::reads`]); none where this server has no extraction model.
async fn to_read(store: &Store, scope: &Scope, eps: &[Episode]) -> Result<HashSet<String>> {
    match *EXTRACTS {
        true => crate::extract::reads(store, scope, eps).await,
        false => Ok(HashSet::new()),
    }
}

/// Whether what is added here is read for facts, and why not where it isn't: `on` where any of it is read or will be
/// (`reads`), else `none` (no extraction model on this server), `off` (the scope's admins turned extraction off and
/// its schema reads none of it), or `on` with nothing here reading these.
async fn extraction(store: &Store, scope: &Scope, reads: bool) -> (&'static str, &'static str) {
    if reads {
        return ("on", "");
    }
    if !*EXTRACTS {
        return ("none", "this server has no extraction model");
    }
    match crate::people::directory::Directory::load(store).await {
        Ok(d) if !d.extracts_starter(scope) => ("off", "extraction is turned off for this memory"),
        _ => ("on", "nothing here reads it for facts"),
    }
}

/// An item as asked, read: its text (a file's bytes as UTF-8), named. Err is why it is refused.
fn read(v: &Value) -> std::result::Result<Item, String> {
    let kind = match v["kind"].as_str().unwrap_or("file") {
        "file" => "file",
        "paste" => "paste",
        "answer" => "answer",
        "recording" => "recording",
        other => {
            return Err(format!(
                "kind is file, paste, answer or recording, not {other:?}"
            ));
        }
    };
    let name = item_name(v);
    if kind == "recording" {
        let recording = read_recording(v, &name)?;
        return Ok(Item {
            kind,
            name,
            text: String::new(),
            recording: Some(recording),
            media: None,
        });
    }
    let text = match kind {
        "answer" => {
            let (q, a) = (
                v["question"].as_str().unwrap_or_default().trim(),
                v["answer"].as_str().unwrap_or_default().trim(),
            );
            if q.is_empty() || a.is_empty() {
                return Err("an answer to keep needs its question and its answer".into());
            }
            format!("Question: {q}\n\nAnswer: {a}\n")
        }
        "file" => {
            let ext = name
                .rsplit_once('.')
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            if ext == "pdf" {
                return Err("PDFs aren't read here yet: copy its text and paste it instead".into());
            }
            if MEDIA_TYPES.contains(&ext.as_str()) {
                let bytes = read_media(v, &ext)?;
                return Ok(Item {
                    kind: "recording",
                    name,
                    text: String::new(),
                    recording: None,
                    media: Some((ext, bytes)),
                });
            }
            if !TEXT_TYPES.contains(&ext.as_str()) {
                return Err(format!(
                    "{} files aren't read: text, Markdown, CSV, JSON and YAML are ({})",
                    if ext.is_empty() {
                        "untyped".to_string()
                    } else {
                        format!(".{ext}")
                    },
                    TEXT_TYPES.map(|t| format!(".{t}")).join(", ")
                ));
            }
            match (v["text"].as_str(), v["base64"].as_str()) {
                (Some(t), _) => t.to_string(),
                (None, Some(b)) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b.trim())
                        .map_err(|_| "its base64 doesn't decode".to_string())?;
                    String::from_utf8(bytes).map_err(|_| {
                        "it isn't text (not UTF-8): save it as text first".to_string()
                    })?
                }
                (None, None) => return Err("a file is sent as text or base64".into()),
            }
        }
        _ => v["text"].as_str().unwrap_or_default().to_string(),
    };
    // A byte-order mark and NULs say nothing, and NUL can't be stored as text.
    let text = text.trim_start_matches('\u{feff}').replace('\0', "");
    if text.trim().is_empty() {
        return Err("it's empty".into());
    }
    if text.len() > MAX_TEXT {
        return Err(format!(
            "it's {} KB: at most {} KB is added at a time, so split it",
            text.len().div_ceil(1024),
            MAX_TEXT / 1024
        ));
    }
    Ok(Item {
        kind,
        name,
        text,
        recording: None,
        media: None,
    })
}

/// A recording's file as sent (base64), if this server transcribes it: Err is why not.
fn read_media(v: &Value, ext: &str) -> std::result::Result<Vec<u8>, String> {
    transcribes(ext)?;
    let Some(b) = v["base64"].as_str().map(str::trim) else {
        return Err("a recording's file is sent as base64".into());
    };
    let too_big = |n: usize| {
        format!(
            "it's {:.1} MB: a recording over {} MB is sent in parts (kind \"part\")",
            n as f64 / (1 << 20) as f64,
            MAX_MEDIA >> 20
        )
    };
    if b.len() / 4 * 3 > MAX_MEDIA + 3 {
        return Err(too_big(b.len() / 4 * 3));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b)
        .map_err(|_| "its base64 doesn't decode".to_string())?;
    if bytes.is_empty() {
        return Err("it's empty".into());
    }
    if bytes.len() > MAX_MEDIA {
        return Err(too_big(bytes.len()));
    }
    Ok(bytes)
}

/// A recording's transcript as sent: Err is why it is refused. Its turns that say nothing are left out; a turn's
/// speaker may also be sent as `speaker_id`.
fn read_recording(v: &Value, name: &str) -> std::result::Result<Recording, String> {
    let sha256 = v["sha256"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if sha256.is_empty() {
        return Err("a recording needs its media file's sha256, which keys it".into());
    }
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(
            "its sha256 isn't one: 64 hexadecimal characters, the media file's SHA-256".into(),
        );
    }
    let Some(raw) = v["turns"].as_array().filter(|t| !t.is_empty()) else {
        return Err("a recording needs its transcript: turns of {speaker, start_ms?, text}".into());
    };
    let mut turns = vec![];
    for (i, t) in raw.iter().enumerate() {
        let text = t["text"].as_str().unwrap_or_default().replace('\0', "");
        if text.trim().is_empty() {
            continue;
        }
        let speaker = t["speaker"]
            .as_str()
            .or(t["speaker_id"].as_str())
            .map(|s| s.replace('\0', "").trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("turns[{i}] names no speaker"))?;
        let start_ms = match &t["start_ms"] {
            Value::Null => None,
            ms => Some(
                ms.as_u64()
                    .ok_or_else(|| format!("turns[{i}].start_ms is milliseconds from the start"))?,
            ),
        };
        turns.push(Turn {
            speaker,
            start_ms,
            text: text.trim().to_string(),
        });
    }
    if turns.is_empty() {
        return Err("its turns hold no text".into());
    }
    let recorded_at = match v["recorded_at"].as_str().map(str::trim) {
        None | Some("") => None,
        Some(t) => Some(
            chrono::DateTime::parse_from_rfc3339(t)
                .map_err(|_| {
                    format!("recorded_at is a time such as 2026-11-18T09:30:00Z, not {t:?}")
                })?
                .timestamp_micros(),
        ),
    };
    let title = v["title"]
        .as_str()
        .map(|t| {
            t.chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect::<String>()
        })
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    Ok(Recording {
        name: name.to_string(),
        sha256,
        title,
        recorded_at,
        turns,
        language: None,
    })
}

/// An item's name as given (a file's, without its folders), else a name for its kind.
pub(super) fn item_name(v: &Value) -> String {
    let given = v["name"].as_str().unwrap_or_default();
    let base = given.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(120)
        .collect();
    match (clean.is_empty(), v["kind"].as_str()) {
        (false, _) => clean,
        // An answer is named by the question it answers.
        (true, Some("answer")) => match v["question"].as_str().map(str::trim) {
            Some(q) if !q.is_empty() => q.chars().filter(|c| !c.is_control()).take(120).collect(),
            _ => "Ask answer".into(),
        },
        (true, Some("paste")) => "Pasted text".into(),
        (true, Some("recording")) => "Recording".into(),
        _ => "Untitled".into(),
    }
}

/// Where an item is filed: its source (what the Explorer lists it under, below who wrote it) and its document key.
/// A file is a document by its name, so adding a changed one again is its next version; pasted text and an answer
/// are one each by their text. A file and pasted text are filed under [`crate::origin::LEARNED`], the person's own.
fn filed(item: &Item, author: &str) -> (String, String) {
    // `:` separates a source's parts and `/` a document key's.
    let part = |s: &str| -> String {
        let dashed: String = s
            .chars()
            .map(|c| {
                if c == ':' || c == '/' || c.is_whitespace() {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        dashed
            .split('-')
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join("-")
            .chars()
            .take(80)
            .collect()
    };
    let digest = hex(&sha2::Sha256::digest(item.text.as_bytes())[..6]);
    let learned = crate::origin::LEARNED;
    match item.kind {
        "file" => (
            format!("{learned}:upload:{}", part(&item.name)),
            format!("{DOCUMENTS}{author}/file/{}", part(&item.name)),
        ),
        kind => {
            // Named by its first words, as a reader would recognise it.
            let words: Vec<&str> = item.text.split_whitespace().take(8).collect();
            let label = match kind {
                "answer" => item
                    .text
                    .strip_prefix("Question: ")
                    .unwrap_or(&item.text)
                    .split_whitespace()
                    .take(8)
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => words.join(" "),
            };
            let label = part(&label.to_lowercase());
            let family = if kind == "answer" { "ask" } else { "paste" };
            // What a person pasted is theirs (`crate::origin::LEARNED`); an answer kept is the model's words.
            let filed = match kind {
                "answer" => String::new(),
                _ => format!("{learned}:"),
            };
            (
                format!(
                    "{filed}{family}:{}",
                    if label.is_empty() {
                        digest.clone()
                    } else {
                        label
                    }
                ),
                format!("{DOCUMENTS}{author}/{family}/{digest}"),
            )
        }
    }
}

/// The meeting a recording is the record of, by its media's digest: `meeting:recording-<digest>`.
fn recording_meeting(rec: &Recording) -> String {
    format!("meeting:recording-{}", &rec.sha256[..12])
}

/// What a recording's episode is about: its meeting, where the scope's schema has meetings (or no schema), so the
/// engine's facts about it (its name, when it was recorded) are kept. A schema without them would refuse the
/// write, and could keep no such fact anyway: there it is about nothing, as before.
async fn recording_about(store: &Store, scope: &Scope, rec: &Recording) -> Result<Vec<String>> {
    let meeting = recording_meeting(rec);
    let typed = match store.schema(scope, crate::MAIN).await? {
        Some(s) => s.entity_type(&meeting).is_some(),
        None => true,
    };
    Ok(if typed { vec![meeting] } else { vec![] })
}

/// Where a recording is filed: under [`crate::origin::RECORDED`] by its media's digest, not its name (two recordings
/// named alike are two sources, one sent again under another name is the same), and as the document of that digest.
fn recording_filed(rec: &Recording, author: &str) -> (String, String) {
    let digest = &rec.sha256[..12];
    (
        format!("{}:{digest}", crate::origin::RECORDED),
        format!("{DOCUMENTS}{author}/recording/{digest}"),
    )
}

/// Who added something, as their documents are filed: a hash of who they are.
pub(super) fn author_of(who: &Principal) -> String {
    hex(&sha2::Sha256::digest(who.user().as_bytes())[..4])
}

/// Whether this server transcribes recordings of type `ext`: Err is why not, as the person adding one is told.
pub(super) fn transcribes(ext: &str) -> std::result::Result<(), String> {
    match &*crate::transcribe::TRANSCRIBER {
        Ok(Some(_)) => {}
        Ok(None) => return Err(NOT_TRANSCRIBED.into()),
        Err(_) => {
            return Err(
                "this server is set to transcribe audio and video but can't yet: add a transcript of it as text \
                 instead"
                    .into(),
            );
        }
    }
    if !TRANSCRIBED.contains(&ext) {
        return Err(format!(
            ".{ext} recordings aren't transcribed: {} are",
            TRANSCRIBED.map(|t| format!(".{t}")).join(", ")
        ));
    }
    Ok(())
}

/// Have extraction read `f`, filed after its request was answered (a recording transcribed apart), as `/learn`
/// does for what it adds: said in the ledger where it is new and something reads it.
pub(super) async fn wake_for(store: &Store, scope: &Scope, who: &Principal, f: &Filed) {
    if !f.new {
        return;
    }
    let mut ep = Episode::new(&super::provenance(who, &f.source), b"", &[]);
    ep.episode_id = f.id.clone();
    ep.document = Some(f.key.clone());
    match to_read(store, scope, &[ep]).await {
        Ok(r) if r.is_empty() => {}
        Ok(_) => {
            let actor = format!("{}{}", crate::extract::ADDED_WAKE, who.user());
            match store.commit_record(scope, &actor, &[]).await {
                Ok(_) => WAKE.notify_one(),
                Err(e) => eprintln!("learn: {scope}'s extraction couldn't be woken: {e:#}"),
            }
        }
        Err(e) => eprintln!(
            "learn: whether {scope} reads {} couldn't be told: {e:#}",
            f.id
        ),
    }
}

/// Extension of `name`, lowercased.
pub(super) fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
