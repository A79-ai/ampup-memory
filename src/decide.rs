//! A decisions model: one that writes nothing and only picks, with a probability, among answers it is given
//! (OpenAI's `gpt-6-luna` at `POST /v1/decisions`, or TypeSafe's `typesafe/jev-1.13` on OpenRouter at
//! `POST /api/alpha/decisions`). Where the engine has to judge rather than
//! write (is this the same as that, does this replace that, does the text say this), it asks one: an answer is
//! one of the options, never free text to parse, it comes with how sure the model is, and it costs a few
//! thousandths of a chat call. A caller acts only on answers it is sure enough of.
//!
//! `MEMSTORE_DECIDE` (`openai`, `openrouter` or `off`), `MEMSTORE_DECIDE_MODEL` (default `gpt-6-luna`, or
//! `typesafe/jev-1.13` on OpenRouter), `MEMSTORE_DECIDE_URL` (default the backend's decisions endpoint) and
//! `MEMSTORE_DECIDE_KEY` (else `OPENAI_API_KEY` or `OPENROUTER_API_KEY`). Unset, it is OpenAI's where there is an
//! OpenAI key, else OpenRouter's where there is an OpenRouter key, else off.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// The two APIs: OpenAI's (`input`, a list of named questions, `predicate`) and OpenRouter's (`state`, questions
/// by name, `noul`).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Api {
    OpenAi,
    OpenRouter,
}
/// Questions per request: a request's state is read once for all of them.
const QUESTIONS_PER_REQUEST: usize = 40;

/// One question about the state.
pub enum Question {
    /// How likely it is that the instruction holds, in [0, 1].
    Likely(String),
    /// Which of the options (name, when it applies) the instruction's answer is.
    Choice(String, Vec<(&'static str, &'static str)>),
}

/// A model's answer to a [`Question`].
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    Likely(f64),
    Choice { choice: String, confidence: f64 },
}

impl Answer {
    /// The likelihood a [`Question::Likely`] got; 0 for any other answer.
    pub fn likely(&self) -> f64 {
        match self {
            Answer::Likely(p) => *p,
            Answer::Choice { .. } => 0.0,
        }
    }

    /// Whether this is `option`, chosen with at least `min` confidence.
    pub fn chose(&self, option: &str, min: f64) -> bool {
        matches!(self, Answer::Choice { choice, confidence } if choice == option && *confidence >= min)
    }
}

/// A decisions model to ask.
#[derive(Clone)]
pub struct Decider {
    api: Api,
    url: String,
    key: Option<String>,
    model: String,
    /// Requests made and what they cost (USD, as the API reports it).
    spent: Arc<Mutex<(usize, f64)>>,
}

impl Decider {
    /// The decisions model the environment configures, if any.
    pub fn from_env() -> Result<Option<Self>> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let api = match var("MEMSTORE_DECIDE") {
            Some(c) if c.eq_ignore_ascii_case("off") => return Ok(None),
            Some(c) if c.eq_ignore_ascii_case("openai") => Api::OpenAi,
            Some(c) if c.eq_ignore_ascii_case("openrouter") => Api::OpenRouter,
            Some(c) => anyhow::bail!("MEMSTORE_DECIDE is openai, openrouter or off, not {c:?}"),
            None if var("OPENAI_API_KEY").is_some() => Api::OpenAi,
            None if var("OPENROUTER_API_KEY").is_some() => Api::OpenRouter,
            None if var("MEMSTORE_DECIDE_KEY").is_some()
                || var("MEMSTORE_DECIDE_URL").is_some() =>
            {
                Api::OpenAi
            }
            None => return Ok(None),
        };
        let (url, model, key) = match api {
            Api::OpenAi => (
                "https://api.openai.com/v1/decisions",
                "gpt-6-luna",
                var("OPENAI_API_KEY"),
            ),
            Api::OpenRouter => (
                "https://openrouter.ai/api/alpha/decisions",
                "typesafe/jev-1.13",
                var("OPENROUTER_API_KEY"),
            ),
        };
        Ok(Some(Decider {
            api,
            url: var("MEMSTORE_DECIDE_URL").unwrap_or_else(|| url.into()),
            key: var("MEMSTORE_DECIDE_KEY").or(key),
            model: var("MEMSTORE_DECIDE_MODEL").unwrap_or_else(|| model.into()),
            spent: Arc::default(),
        }))
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Requests made so far, and what they cost.
    pub fn spent(&self) -> (usize, f64) {
        *self.spent.lock().unwrap()
    }

    /// Every question's answer, by its name, about `state`. A question the model didn't answer has none.
    pub async fn decide(
        &self,
        state: &Value,
        questions: Vec<(String, Question)>,
    ) -> Result<HashMap<String, Answer>> {
        let mut out = HashMap::new();
        let mut questions = questions.into_iter().peekable();
        while questions.peek().is_some() {
            let batch: Vec<(String, Question)> =
                questions.by_ref().take(QUESTIONS_PER_REQUEST).collect();
            let body = self.request(state, batch);
            let started = std::time::Instant::now();
            let v = self.send(&body).await?;
            let latency_ms = started.elapsed().as_millis() as u64;
            // OpenRouter answers by name; OpenAI in a list, each naming its question.
            let answers: Vec<(String, &Value)> = match &v["answers"] {
                Value::Object(m) => m.iter().map(|(k, a)| (k.clone(), a)).collect(),
                Value::Array(l) => l
                    .iter()
                    .filter_map(|a| Some((a["name"].as_str()?.to_string(), a)))
                    .collect(),
                _ => vec![],
            };
            let answered = answers.len();
            // Kept whole when made for work that is logged (`crate::extract::train_log`): the request as sent
            // and the reply as it came, the way a chat call is.
            crate::extract::train_log::record(crate::extract::train_log::Call {
                backend: "decisions",
                model: &self.model,
                body: &body,
                prompt: &body.to_string(),
                max_tokens: 0,
                reply: &v.to_string(),
                finish: Some("stop"),
                usable: answered > 0,
                cached: false,
                batch: false,
                tokens: (
                    v["usage"]["prompt_tokens"].as_u64().unwrap_or_default(),
                    v["usage"]["completion_tokens"].as_u64().unwrap_or_default(),
                ),
                latency_ms,
            })
            .await;
            for (name, a) in answers {
                // A refusal, or a type this doesn't know, is no answer.
                let answer = match a["type"].as_str() {
                    Some("noul") => a["noul"].as_f64().map(Answer::Likely),
                    Some("predicate") => a["probability"].as_f64().map(Answer::Likely),
                    Some("choice") => a["choice"].as_str().map(|c| Answer::Choice {
                        choice: c.to_string(),
                        confidence: a["confidence"].as_f64().unwrap_or_default(),
                    }),
                    _ => None,
                };
                if let Some(answer) = answer {
                    out.insert(name, answer);
                }
            }
            let mut spent = self.spent.lock().unwrap();
            spent.0 += 1;
            spent.1 += v["usage"]["cost"].as_f64().unwrap_or_default();
        }
        Ok(out)
    }

    /// The request body for `questions` about `state`, as the API takes it.
    fn request(&self, state: &Value, questions: Vec<(String, Question)>) -> Value {
        match self.api {
            Api::OpenRouter => {
                let qs: serde_json::Map<String, Value> = questions
                    .into_iter()
                    .map(|(name, q)| {
                        let q = match q {
                            Question::Likely(i) => json!({"type": "noul", "instructions": i}),
                            Question::Choice(i, options) => {
                                let criteria: serde_json::Map<String, Value> = options
                                    .into_iter()
                                    .map(|(o, when)| (o.to_string(), json!(when)))
                                    .collect();
                                json!({"type": "choice", "instructions": i, "criteria": criteria})
                            }
                        };
                        (name, q)
                    })
                    .collect();
                json!({"model": self.model, "state": state, "questions": qs})
            }
            Api::OpenAi => {
                let qs: Vec<Value> = questions
                    .into_iter()
                    .map(|(name, q)| match q {
                        Question::Likely(i) => {
                            json!({"type": "predicate", "name": name, "instructions": i})
                        }
                        Question::Choice(i, options) => json!({"type": "choice", "name": name,
                            "instructions": i,
                            "choices": options.into_iter()
                                .map(|(o, when)| json!({"value": o, "description": when}))
                                .collect::<Vec<_>>()}),
                    })
                    .collect();
                let input = match state {
                    Value::String(s) => s.clone(),
                    v => serde_json::to_string_pretty(v).unwrap_or_default(),
                };
                json!({"model": self.model, "input": input, "questions": qs})
            }
        }
    }

    /// One request, tried again once on a rate limit or a server error.
    async fn send(&self, body: &Value) -> Result<Value> {
        static HTTP: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("an HTTP client")
        });
        let mut tries = 0;
        loop {
            tries += 1;
            let mut req = HTTP.post(&self.url).json(body);
            if let Some(k) = &self.key {
                req = req.bearer_auth(k);
            }
            let res = req.send().await.context("decisions request")?;
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            if status.is_success() {
                return serde_json::from_str(&text).context("decisions reply is not JSON");
            }
            if tries < 2 && (status.as_u16() == 429 || status.is_server_error()) {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            anyhow::bail!(
                "decisions model answered {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
    }
}
