//! Speech to text for what people add in Studio (`/learn`): a small audio or video file becomes a recording's
//! transcript, its turns by speaker and time, filed as a call (`crate::server` `learn::file_recording`).
//!
//! Off unless the server is told which service to use:
//!
//! - `AMPUP_MEMORY_TRANSCRIBE`: `elevenlabs` (ElevenLabs Scribe, `scribe_v1`, speakers told apart) or `off` (the
//!   default). Off, a recording is refused, saying audio and video aren't read.
//! - `ELEVENLABS_API_KEY`: the service's key. Without it the switch is on but nothing is transcribed: said once at
//!   start, and each recording is refused saying so.
//! - `AMPUP_MEMORY_TRANSCRIBE_URL`: the service's base URL (default `https://api.elevenlabs.io`), for a stand-in.
//! - `AMPUP_MEMORY_TRANSCRIBE_TIMEOUT_SECS`: how long one transcription may take (default 120).
//! - `AMPUP_MEMORY_TRANSCRIBE_TOKENS_PER_MIN`: what a minute of audio counts as against its org's monthly model
//!   tokens ([`tokens_for`]; default [`TOKENS_PER_MIN`]).
//!
//! Transcribing is a model call the org pays for like any other: it is refused once the org's allowance or plan has
//! run out (`crate::limits::model_gate`), and its minutes count against the allowance afterwards.
//!
//! The language is the service's to detect. Words are grouped into turns by speaker, a long run of one speaker
//! split at a sentence's end once it passes [`TURN_MS`], so a fact cites a moment rather than a monologue; sounds
//! the service tags (laughter, music) are left out.

use std::sync::LazyLock;
use std::time::Duration;

use serde::Deserialize;

/// The default service URL.
const ELEVENLABS: &str = "https://api.elevenlabs.io";
/// How long a turn of one speaker runs before it is split at the next sentence's end.
const TURN_MS: u64 = 30_000;

/// What a minute of audio counts as against an org's monthly model tokens. Set from cost: Scribe at its highest
/// listed price, $0.40 an hour, is $0.0067 a minute; the default extraction model (Haiku 5.5, $1 per million tokens
/// read and $5 written, mostly read) is about $1.50 per million tokens, so a minute costs what ~4,400 tokens do.
/// 4,000 rounds that down; Starter's 2M tokens then hold about 8 hours of audio if nothing else were read.
pub const TOKENS_PER_MIN: u64 = 4_000;

/// The model tokens `seconds` of audio count as (`AMPUP_MEMORY_TRANSCRIBE_TOKENS_PER_MIN`, default
/// [`TOKENS_PER_MIN`]), each started second counted.
pub fn tokens_for(seconds: f64) -> u64 {
    static RATE: LazyLock<u64> = LazyLock::new(|| {
        std::env::var("AMPUP_MEMORY_TRANSCRIBE_TOKENS_PER_MIN")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(TOKENS_PER_MIN)
    });
    (seconds.max(0.0).ceil() as u64 * *RATE).div_ceil(60)
}

/// A service that turns speech into text.
pub struct Transcriber {
    key: String,
    url: String,
    timeout: Duration,
}

/// A recording's transcript: its turns, the language the service heard, and how long the audio is.
pub struct Transcript {
    pub turns: Vec<Turn>,
    pub language: Option<String>,
    pub seconds: Option<f64>,
}

/// One turn: who spoke (the service's label, `speaker_0`), from when (milliseconds), saying what.
pub struct Turn {
    pub speaker: String,
    pub start_ms: u64,
    pub text: String,
}

/// The server's transcriber, as the environment configured it when first asked: None where the switch is off, Err
/// (why) where it is on but can't run.
pub static TRANSCRIBER: LazyLock<Result<Option<Transcriber>, String>> =
    LazyLock::new(Transcriber::from_env);

impl Transcriber {
    /// The transcriber the environment names (see the module), None where it is off.
    pub fn from_env() -> Result<Option<Self>, String> {
        let which = std::env::var("AMPUP_MEMORY_TRANSCRIBE").unwrap_or_default();
        match which.trim().to_ascii_lowercase().as_str() {
            "" | "off" => return Ok(None),
            "elevenlabs" => {}
            other => {
                return Err(format!(
                    "AMPUP_MEMORY_TRANSCRIBE is elevenlabs or off, not {other:?}"
                ));
            }
        }
        let key = std::env::var("ELEVENLABS_API_KEY").unwrap_or_default();
        if key.trim().is_empty() {
            return Err(
                "AMPUP_MEMORY_TRANSCRIBE is elevenlabs, but ELEVENLABS_API_KEY isn't set".into(),
            );
        }
        let timeout = std::env::var("AMPUP_MEMORY_TRANSCRIBE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(120u64);
        Ok(Some(Transcriber {
            key: key.trim().to_string(),
            url: std::env::var("AMPUP_MEMORY_TRANSCRIBE_URL")
                .ok()
                .map(|u| u.trim().trim_end_matches('/').to_string())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| ELEVENLABS.into()),
            timeout: Duration::from_secs(timeout.max(5)),
        }))
    }

    /// `bytes`, a media file of type `ext` (its name's extension), as a transcript. Err is why it couldn't be, as a
    /// person adding it would be told.
    pub async fn transcribe(&self, ext: &str, bytes: &[u8]) -> Result<Transcript, String> {
        self.transcribe_within(ext, bytes, self.timeout).await
    }

    /// [`Self::transcribe`], allowed `timeout` rather than the configured one: a recording transcribed after its
    /// request was answered (`crate::server` `upload`) isn't held to a request's time.
    pub async fn transcribe_within(
        &self,
        ext: &str,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<Transcript, String> {
        // A multipart form by hand: the model, speakers told apart, and the file under a name of its type alone
        // (what the person called it isn't sent).
        let boundary = format!("memstore-{:016x}", rand::random::<u64>());
        let mut body = Vec::with_capacity(bytes.len() + 512);
        for (field, value) in [("model_id", "scribe_v1"), ("diarize", "true")] {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"media.{ext}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| format!("transcription isn't available here: {e}"))?;
        let r = client
            .post(format!("{}/v1/speech-to-text", self.url))
            .header("xi-api-key", &self.key)
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .map_err(|e| match e.is_timeout() {
                true => format!(
                    "transcribing it took longer than {} s: try a shorter recording",
                    timeout.as_secs()
                ),
                false => format!("the transcription service couldn't be reached: {e}"),
            })?;
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(refusal(status.as_u16(), &text));
        }
        let reply: Reply = serde_json::from_str(&text)
            .map_err(|e| format!("the transcription service's answer didn't read: {e}"))?;
        Ok(Transcript {
            turns: turns(&reply.words),
            language: reply.language_code.filter(|l| !l.trim().is_empty()),
            seconds: reply.audio_duration_secs,
        })
    }
}

/// Why the service refused a file, as the person adding it is told: its own words where it gives them.
fn refusal(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            let d = &v["detail"];
            d["message"]
                .as_str()
                .or(d.as_str())
                .or(v["message"].as_str())
                .map(String::from)
        })
        .map(|d| d.chars().take(200).collect::<String>())
        .unwrap_or_default();
    let said = if detail.is_empty() {
        String::new()
    } else {
        format!(" ({detail})")
    };
    match status {
        401 | 403 => format!("the transcription service refused this server's key{said}"),
        400 | 415 | 422 => {
            format!("the transcription service couldn't read it as audio or video{said}")
        }
        429 => format!("the transcription service is busy: try it again in a minute{said}"),
        s if s >= 500 => {
            format!("the transcription service failed ({s}): try it again later{said}")
        }
        s => format!("the transcription service refused it ({s}){said}"),
    }
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    words: Vec<Word>,
    #[serde(default)]
    language_code: Option<String>,
    #[serde(default)]
    audio_duration_secs: Option<f64>,
}

#[derive(Deserialize)]
struct Word {
    #[serde(default)]
    text: String,
    #[serde(default)]
    start: Option<f64>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    speaker_id: Option<String>,
}

/// The words as turns: a new one where the speaker changes, or where one speaker's run has passed [`TURN_MS`] and a
/// sentence ends. Sounds the service tagged are left out; a turn that says nothing isn't kept.
fn turns(words: &[Word]) -> Vec<Turn> {
    let mut out: Vec<Turn> = vec![];
    let mut ends_sentence = false;
    for w in words {
        let kind = w.kind.as_deref().unwrap_or("word");
        if kind == "audio_event" {
            continue;
        }
        let ms = w.start.map_or(0, |s| (s.max(0.0) * 1000.0) as u64);
        if kind == "word" {
            let speaker = w.speaker_id.as_deref().unwrap_or("speaker_0");
            let new = match out.last() {
                None => true,
                Some(t) => t.speaker != speaker || (ends_sentence && ms >= t.start_ms + TURN_MS),
            };
            if new {
                out.push(Turn {
                    speaker: speaker.to_string(),
                    start_ms: ms,
                    text: String::new(),
                });
            }
            ends_sentence = w.text.trim_end().ends_with(['.', '?', '!']);
        }
        if let Some(t) = out.last_mut() {
            t.text.push_str(&w.text);
        }
    }
    for t in &mut out {
        t.text = t.text.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    out.retain(|t| !t.text.is_empty());
    out
}
