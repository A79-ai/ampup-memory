//! A recording too large for one request, added in Studio in parts: `/learn` items of kind `part`, each a piece of
//! the file as base64, kept beside the catalog until the last arrives; then it is transcribed and filed apart from
//! any request ([`super::learn::file_media`], keyed on the digest of its bytes as a small one is), its pieces
//! deleted whatever came of it, and `/learn/status {upload_ids}` says how it stands (`uploading` until its last part
//! arrives, then `transcribing`, then `added` or `refused`).
//!
//! - The first part (`n` 0) names the file and its `size` in bytes and is answered with an `upload_id`; the rest
//!   carry it. Every part but the last holds exactly [`PART`] bytes, the last the rest, so the pieces kept add up to
//!   the size named; a part sent again replaces itself. The type is checked, and that this server transcribes it, on
//!   the first part, before anything is kept.
//! - A recording is at most `AMPUP_MEMORY_TRANSCRIBE_MAX_MB` (default 200) MB.
//! - Its pieces are under the caller's own folder (their scope's and their own hash), so an upload id reaches only
//!   what its sender sent. What's left of an upload never finished is deleted after a day, at the sender's next
//!   upload; what came of one, after a week.
//! - Transcription is held to [`LONG`], a few at a time per server; one whose server stopped before it was filed is
//!   said to be interrupted once that has passed, so it can be added again (its bytes were deleted or are deleted
//!   with what's left).
//!
//! Where the catalog has no place beside it for this (no object store it can write), parts are refused, saying so.

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use sha2::Digest;

use super::{State, ToolError};
use crate::{Principal, Scope, Store};

/// The bytes in every part but a recording's last: 3 MB, 4 MB as base64, inside what a request may carry here and
/// through the console.
pub(super) const PART: usize = 3 << 20;
/// Where the parts are kept, beside the catalog.
const DIR: &str = "_uploads";
/// How long one recording's transcription may take.
const LONG: Duration = Duration::from_secs(30 * 60);
/// How many recordings one server transcribes at once (each held in memory whole while it is).
static RUNNING: LazyLock<tokio::sync::Semaphore> = LazyLock::new(|| tokio::sync::Semaphore::new(2));

/// The largest recording sent in parts.
fn max_bytes() -> usize {
    std::env::var("AMPUP_MEMORY_TRANSCRIBE_MAX_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(200)
        << 20
}

/// What a part item came to: still uploading, or the last, now transcribing.
pub(super) enum Part {
    Uploading(Value),
    Transcribing(Value),
}

/// The upload's state as a person reading its receipt sees it, kept as `state.json`.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct Kept {
    name: String,
    ext: String,
    size: usize,
    whose: String,
    created: DateTime<Utc>,
}

static PLACE: LazyLock<tokio::sync::Mutex<Option<(opendal::Operator, Instant)>>> =
    LazyLock::new(Default::default);

/// The place beside the catalog parts are kept, found again every ten minutes and before its credentials run out;
/// none where the catalog has none.
async fn place(store: &Store) -> Result<Option<opendal::Operator>> {
    let mut p = PLACE.lock().await;
    if let Some((op, _)) = p.as_ref().filter(|(_, until)| Instant::now() < *until) {
        return Ok(Some(op.clone()));
    }
    let found = tokio::time::timeout(Duration::from_secs(30), super::writer::beside(store, DIR))
        .await
        .context("finding where uploads are kept took over 30s")??;
    *p = found
        .clone()
        .map(|(op, until)| (op, until.min(Instant::now() + Duration::from_secs(600))));
    Ok(found.map(|(op, _)| op))
}

/// The caller's folder: their scope's hash and theirs (`author`).
fn folder(scope: &Scope, author: &str) -> String {
    let s = sha2::Sha256::digest(scope.to_string().as_bytes());
    let s: String = s[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{s}/{author}/")
}

fn upload_id(v: &Value) -> Option<String> {
    v["upload_id"]
        .as_str()
        .map(str::trim)
        .filter(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
}

fn part_name(dir: &str, n: usize) -> String {
    format!("{dir}part-{n:05}")
}

fn refuse(why: impl Into<String>) -> anyhow::Error {
    ToolError(why.into()).into()
}

/// One `part` item: `{kind: "part", name, size, n, base64, upload_id?}` (`upload_id` on every part but the first).
pub(super) async fn part(
    state: &Arc<State>,
    store: &Store,
    scope: &Scope,
    who: &Principal,
    whose: &str,
    author: &str,
    v: &Value,
) -> Result<Part> {
    let n = v["n"]
        .as_u64()
        .ok_or_else(|| refuse("a part says which it is (n, from 0)"))? as usize;
    let Some(b64) = v["base64"].as_str().map(str::trim) else {
        return Err(refuse("a part is sent as base64"));
    };
    if b64.len() / 4 * 3 > PART + 3 {
        return Err(refuse(format!("a part holds at most {PART} bytes")));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| refuse("its base64 doesn't decode"))?;
    // What isn't transcribed here is refused before anything is kept.
    if n == 0 {
        super::learn::transcribes(&super::learn::ext_of(&super::learn::item_name(v)))
            .map_err(refuse)?;
        // Nor is a recording uploaded that its org couldn't have transcribed: past its model tokens or its plan's end.
        if let Some(l) = crate::limits::model_gate(scope.org_name()) {
            return Err(refuse(l.message));
        }
    }
    let op = place(store).await?.ok_or_else(|| {
        refuse("recordings this large aren't taken here: this memory keeps no uploads")
    })?;
    let mine = folder(scope, author);
    let (id, kept) = match (n, upload_id(v)) {
        (0, None) => {
            let name = super::learn::item_name(v);
            let ext = super::learn::ext_of(&name);
            super::learn::transcribes(&ext).map_err(refuse)?;
            let size = v["size"].as_u64().unwrap_or(0) as usize;
            if size == 0 {
                return Err(refuse("a recording's first part says its size in bytes"));
            }
            if size > max_bytes() {
                return Err(refuse(format!(
                    "it's {:.0} MB: a recording of at most {} MB is transcribed here",
                    size as f64 / (1 << 20) as f64,
                    max_bytes() >> 20
                )));
            }
            // The first part checked before anything is kept.
            if bytes.len() != size.min(PART) {
                return Err(refuse(format!(
                    "part 0 holds {} bytes: it should hold {}",
                    bytes.len(),
                    size.min(PART)
                )));
            }
            sweep(&op, &mine).await;
            let id = uuid::Uuid::new_v4().simple().to_string();
            let kept = Kept {
                name,
                ext,
                size,
                whose: whose.to_string(),
                created: Utc::now(),
            };
            op.write(&format!("{mine}{id}/meta.json"), serde_json::to_vec(&kept)?)
                .await
                .context("keep an upload's name")?;
            (id, kept)
        }
        (0, Some(_)) | (_, None) => {
            return Err(refuse(
                "the first part (n 0) starts an upload and is answered with its upload_id; the rest carry it",
            ));
        }
        (_, Some(id)) => match op.read(&format!("{mine}{id}/meta.json")).await {
            Ok(b) => (id, serde_json::from_slice::<Kept>(&b.to_vec())?),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => {
                return Err(refuse(
                    "no such upload (it may have expired): start it again",
                ));
            }
            Err(e) => return Err(anyhow::Error::from(e).context("read an upload's name")),
        },
    };
    let dir = format!("{mine}{id}/");
    let parts = kept.size.div_ceil(PART);
    if n >= parts {
        return Err(refuse(format!(
            "a recording of {} bytes is {parts} parts (n 0 to {})",
            kept.size,
            parts - 1
        )));
    }
    let want = if n + 1 < parts {
        PART
    } else {
        kept.size - n * PART
    };
    if bytes.len() != want {
        return Err(refuse(format!(
            "part {n} holds {} bytes: it should hold {want}",
            bytes.len()
        )));
    }
    op.write(&part_name(&dir, n), bytes)
        .await
        .context("keep a part of an upload")?;
    if n + 1 < parts {
        return Ok(Part::Uploading(
            json!({"name": kept.name, "upload_id": id, "n": n, "parts": parts}),
        ));
    }
    // The last: every other part must be here.
    for i in 0..n {
        if !op.exists(&part_name(&dir, i)).await? {
            return Err(refuse(format!(
                "part {i} of {parts} hasn't arrived: send it, then the last again"
            )));
        }
    }
    let started = json!({"status": "transcribing", "name": kept.name, "started": Utc::now()});
    op.write(&format!("{dir}state.json"), started.to_string())
        .await
        .context("keep how an upload stands")?;
    eprintln!(
        "transcribe {scope}: upload {id}, {} bytes of .{} in {parts} parts, to transcribe",
        kept.size, kept.ext
    );
    // Working for the request's org: its model tokens counted, its limits held to (`crate::limits`).
    tokio::spawn(crate::log_org::with(
        scope.org_name(),
        transcribe(
            state.clone(),
            store.clone(),
            scope.clone(),
            who.clone(),
            author.to_string(),
            op,
            dir,
            id.clone(),
            kept.clone(),
        ),
    ));
    Ok(Part::Transcribing(
        json!({"name": kept.name, "upload_id": id}),
    ))
}

/// Transcribe an upload whose parts are all here and file it, its parts deleted whatever came of it, and keep what
/// came of it for its receipt.
#[allow(clippy::too_many_arguments)]
async fn transcribe(
    state: Arc<State>,
    store: Store,
    scope: Scope,
    who: Principal,
    author: String,
    op: opendal::Operator,
    dir: String,
    id: String,
    kept: Kept,
) {
    let Ok(_running) = RUNNING.acquire().await else {
        return;
    };
    // From now: the receipt says it was interrupted only once this has run past its time.
    let now = json!({"status": "transcribing", "name": kept.name, "started": Utc::now()});
    let _ = op.write(&format!("{dir}state.json"), now.to_string()).await;
    let parts = kept.size.div_ceil(PART);
    let filed = async {
        let mut bytes = Vec::with_capacity(kept.size);
        for i in 0..parts {
            bytes.extend_from_slice(&op.read(&part_name(&dir, i)).await?.to_vec());
        }
        anyhow::ensure!(
            bytes.len() == kept.size,
            "upload {id} holds {} bytes, not {}",
            bytes.len(),
            kept.size
        );
        super::learn::file_media(
            &state,
            &store,
            &scope,
            &who,
            &kept.whose,
            &author,
            &kept.name,
            &kept.ext,
            &bytes,
            Some(LONG),
        )
        .await
    }
    .await;
    for i in 0..parts {
        let _ = op.delete(&part_name(&dir, i)).await;
    }
    let _ = op.delete(&format!("{dir}meta.json")).await;
    let outcome = match filed {
        Ok(f) => {
            super::learn::wake_for(&store, &scope, &who, &f).await;
            json!({"status": "added", "name": kept.name, "episode_id": f.id, "new": f.new,
                   "source": f.source, "finished": Utc::now()})
        }
        Err(e) => {
            let reason = match super::learn::refusal(&e) {
                Some(why) => why,
                None => {
                    eprintln!("transcribe {scope}: upload {id} failed: {e:#}");
                    crate::report::failure("learn upload", &e);
                    "it couldn't be added: add it again".to_string()
                }
            };
            json!({"status": "refused", "name": kept.name, "reason": reason, "finished": Utc::now()})
        }
    };
    if let Err(e) = op
        .write(&format!("{dir}state.json"), outcome.to_string())
        .await
    {
        eprintln!("transcribe {scope}: upload {id}'s outcome wasn't kept: {e}");
    }
}

/// How each of `ids` the caller sent stands: `uploading` (parts still to come: its last hasn't arrived),
/// `transcribing`, `added` (its `episode_id`, `new`), `refused` (`reason`), or `unknown` (not theirs, or long gone).
pub(super) async fn states(
    store: &Store,
    scope: &Scope,
    author: &str,
    ids: &[String],
) -> Result<Vec<Value>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let Some(op) = place(store).await? else {
        return Ok(ids
            .iter()
            .map(|id| json!({"upload_id": id, "status": "unknown"}))
            .collect());
    };
    let mine = folder(scope, author);
    let mut out = vec![];
    for id in ids {
        let wellformed = upload_id(&json!({"upload_id": id})).is_some();
        let read = match wellformed {
            true => op.read(&format!("{mine}{id}/state.json")).await,
            false => Err(opendal::Error::new(opendal::ErrorKind::NotFound, "")),
        };
        let mut v = match read {
            Ok(b) => serde_json::from_slice::<Value>(&b.to_vec()).unwrap_or_default(),
            // Nothing came of it yet: still uploading while its name is kept (the first part arrived, the last
            // hasn't), else not theirs or long gone.
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => {
                match op.exists(&format!("{mine}{id}/meta.json")).await {
                    Ok(true) => json!({"status": "uploading"}),
                    Ok(false) => json!({"status": "unknown"}),
                    Err(e) => {
                        return Err(anyhow::Error::from(e).context("read how an upload stands"));
                    }
                }
            }
            Err(e) => return Err(anyhow::Error::from(e).context("read how an upload stands")),
        };
        let started = v["started"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok());
        if v["status"] == "transcribing"
            && started.is_some_and(|t| {
                Utc::now() - t.with_timezone(&Utc)
                    > TimeDelta::from_std(LONG).unwrap_or_default() + TimeDelta::minutes(15)
            })
        {
            v["status"] = json!("refused");
            v["reason"] = json!("its transcription was interrupted: add it again");
        }
        v["upload_id"] = json!(id);
        out.push(v);
    }
    Ok(out)
}

/// Delete what's left in `mine` of uploads never finished (a day old) and of what came of finished ones (a week).
async fn sweep(op: &opendal::Operator, mine: &str) {
    let Ok(entries) = op.list(mine).await else {
        return;
    };
    let now = Utc::now();
    for e in entries {
        let dir = e.path().to_string();
        if !dir.ends_with('/') || dir == mine {
            continue;
        }
        let when = |b: opendal::Buffer, key: &str| -> Option<DateTime<Utc>> {
            serde_json::from_slice::<Value>(&b.to_vec()).ok()?[key]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc))
        };
        let stale = match op.read(&format!("{dir}state.json")).await {
            Ok(b) => {
                let v = when(b.clone(), "finished").or_else(|| when(b, "started"));
                v.is_none_or(|t| now - t > TimeDelta::days(7))
            }
            Err(_) => match op.read(&format!("{dir}meta.json")).await {
                Ok(b) => when(b, "created").is_none_or(|t| now - t > TimeDelta::days(1)),
                // Parts with neither: left by an upload whose name was deleted.
                Err(_) => true,
            },
        };
        if stale && let Err(err) = op.delete_with(&dir).recursive(true).await {
            eprintln!("uploads: {dir} not deleted: {err}");
        }
    }
}
