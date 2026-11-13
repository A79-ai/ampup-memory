//! Runs (G11, GM11): an agent's task run kept so its outcome can teach the memory later. `record_run` (MCP)
//! and `POST /runs {task, trace, used, outcome?, advice?}` keep:
//!
//! - what the run used: an entry in the answer log ([`crate::qa_log`]) naming the facts it read, checked
//!   against what the caller may read; its id is the run's id;
//! - its trace: an episode of the caller's own, readable by them alone (a teammate never sees another
//!   person's runs), signed like `remember_memory` with the source `run:<id>`. The situation key `get_advice`
//!   named for the task (`advice`) is a line of its own, `Advice: <key>`, between the task and the trace, so
//!   a run can be told apart by the advice it was given. A run that asked several questions (a review asks one
//!   per changed area) names every key it was served, separated by `; `: `Advice: <key>; <key>`.
//!
//! `reward_run {run_id, reward, reason?}` (`POST /runs/reward`) attaches the outcome later, from -1 to 1: only
//! the person who recorded the run may. Only a person records runs: a service key has no ACL id of its own, so
//! its trace couldn't be kept to itself. A session is rewarded the same way, by its id. `by: "person"` (the route
//! only, never the MCP tool, whose arguments a model chooses) says the person rated it themselves, at the
//! keyboard (the hooks' `+` and `-`): their rating, which advice weighs as the person's word, not a program's.
//!
//! **Many at once.** Loading runs whose outcomes are already known (historical or practice runs, for advice to
//! learn from) is a batch: `POST /runs {runs: [{task, trace, used?, outcome?, advice?, reward?, reason?},
//! ...]}` ([`record_many`]), or `memstore runs load` from a JSON-lines file of the same items. Every run is checked
//! as one alone would be, all before anything is written, and one refused refuses the batch, named by its index
//! (`runs[3]: ...`). Then all the traces go in one commit, and all the log entries, with the rewards given
//! inline, in another: a scored run needs no second call, and 400 runs cost two commits, not 800. The day's caps
//! count the whole batch. `POST /runs/reward {rewards: [{run_id, reward, reason?}, ...]}` ([`reward_many`])
//! rewards runs recorded earlier, in one commit. One request takes at most [`MAX_BATCH`] runs or rewards; MCP
//! `record_run` records one, as an agent finishes it, on the same path.
//!
//! **Ids of runs loaded together.** A run recorded alone gets a random id. A run of a batch gets one made from what
//! it is ([`batch_id`]): the scope, its recorder, the run as given and how many identical runs came before it in the
//! batch. So loading the same runs into a fresh store gives them the same ids, their traces the same bodies, and
//! advice learned from them the same prompts, which the model-call cache answers (`crate::extract::cache`); and
//! loading the same file twice records each run once. A batch item may name its own `id` (a UUID) instead: one
//! another person recorded is refused.

use anyhow::Result;
use serde_json::{Value, json};

use crate::{Episode, Principal, Scope, Store};

/// Longest trace kept, in characters.
const MAX_TRACE: usize = 20_000;
/// Longest reward reason kept, in characters.
const MAX_REASON: usize = 500;
/// Longest advice key kept, in characters: a situation key is a category and a situation, a few words each.
const MAX_ADVICE: usize = 200;
/// Most advice keys one run names: one per question it asked `get_advice`.
const MAX_ADVICE_KEYS: usize = 10;
/// Most runs one `POST /runs` records, and rewards one `POST /runs/reward` attaches: a batch is checked whole in
/// memory before it's written. `memstore runs load` isn't a request, and loads a whole file as one batch.
pub const MAX_BATCH: usize = 1_000;

/// A caller's mistake, said back to them as is.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

impl Refused {
    /// What was refused, without the batch item it names (`runs[3]: `), if any.
    pub fn reason(&self) -> &str {
        ["runs[", "rewards["]
            .iter()
            .find_map(|l| {
                let (i, rest) = self.0.strip_prefix(l)?.split_once("]: ")?;
                i.bytes().all(|b| b.is_ascii_digit()).then_some(rest)
            })
            .unwrap_or(&self.0)
    }
}

fn refuse(m: String) -> anyhow::Error {
    Refused(m).into()
}

/// A refusal about the `i`th item of a batch, said of it as `<label>[i]: ...`; with no label (one item, not a
/// batch), as is.
fn of_item(label: Option<&str>, i: usize, e: anyhow::Error) -> anyhow::Error {
    match (label, e.downcast_ref::<Refused>()) {
        (Some(l), Some(r)) => refuse(format!("{l}[{i}]: {}", r.0)),
        _ => e,
    }
}

/// The answer log's refusals as this module's.
fn relay(e: anyhow::Error) -> anyhow::Error {
    match e.downcast_ref::<crate::qa_log::Refused>() {
        Some(r) => refuse(r.0.clone()),
        None => e,
    }
}

/// Records a run. `signed` prefixes the trace episode's source (`mcp:remember_memory:<caller>:`, so it is
/// signed like a remembered note; `run:<id>` follows) and `acl` keeps it to the caller. Returns the run id.
pub async fn record(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    person: bool,
    acl: &[String],
    signed: &str,
    args: &Value,
) -> Result<String> {
    let rec = Recorder {
        who,
        person,
        acl,
        signed,
    };
    Ok(rec
        .write(store, scope, std::slice::from_ref(args), None)
        .await?
        .remove(0))
}

/// Records a batch of runs, each as [`record`] records one and each with its reward if it comes with one
/// (`reward` -1 to 1, `reason`): all checked before anything is written, then the traces in one commit and the
/// log entries and rewards in another. Returns the run ids, in order. A refusal names its run (`runs[i]: ...`).
pub async fn record_many(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    person: bool,
    acl: &[String],
    signed: &str,
    runs: &[Value],
) -> Result<Vec<String>> {
    if runs.is_empty() {
        return Err(refuse("runs is a list of at least one run".into()));
    }
    let rec = Recorder {
        who,
        person,
        acl,
        signed,
    };
    rec.write(store, scope, runs, Some("runs")).await
}

/// Records a batch of runs from outside a request (`memstore runs load`), as a server would record them for their
/// person: `person` by their id, their traces readable by `user:<id>` alone; or, with none, the one caller of
/// `serve --local`, reading as `groups`, their traces readable by those groups, as that server keeps its runs.
pub async fn load(
    store: &Store,
    scope: &Scope,
    person: Option<&Principal>,
    groups: &[String],
    runs: &[Value],
) -> Result<Vec<String>> {
    let local;
    let (who, acl) = match person {
        Some(p) if crate::auth::is_acl_id(p.user()) => (p, vec![format!("user:{}", p.user())]),
        Some(p) => {
            return Err(refuse(format!(
                "{} can't be named in an ACL, so its runs couldn't be kept to it",
                p.user()
            )));
        }
        None if groups.is_empty() => {
            return Err(refuse(
                "the local caller's runs are kept to its groups: name at least one".into(),
            ));
        }
        None => {
            let gs: Vec<&str> = groups.iter().map(String::as_str).collect();
            local = Principal::new(crate::server::SERVICE_PRINCIPAL, &gs);
            (
                &local,
                groups.iter().map(|g| format!("group:{g}")).collect(),
            )
        }
    };
    let signed = format!("{}:{}:", crate::REMEMBER_SOURCE, who.user());
    // A store no server has opened yet (a fresh `--local` directory) gets its tables, as `serve` would make them.
    store.ensure_scope(scope).await?;
    record_many(store, scope, who, true, &acl, &signed, runs).await
}

/// Who records: the caller, whether they are a person, the ACL keeping their traces to them, and how the traces
/// are signed.
struct Recorder<'a> {
    who: &'a Principal,
    person: bool,
    acl: &'a [String],
    signed: &'a str,
}

/// A run checked and ready to write.
struct Checked {
    id: String,
    /// Its answer-log entry, as `qa_log` checks one.
    entry: Value,
    episode: Episode,
    reward: Option<(f64, Option<String>)>,
}

impl Recorder<'_> {
    /// `label` names a batch's runs in its refusals; a run alone has none, and no reward.
    async fn write(
        &self,
        store: &Store,
        scope: &Scope,
        runs: &[Value],
        label: Option<&str>,
    ) -> Result<Vec<String>> {
        let who = self.who;
        if !self.person {
            return Err(refuse(format!(
                "a run is a person's: {} is a key",
                who.user()
            )));
        }
        let ids = self.ids(store, scope, runs, label).await?;
        let runs: Vec<Checked> = runs
            .iter()
            .zip(ids)
            .enumerate()
            .map(|(i, (args, id))| {
                self.checked(args, label.is_some(), id)
                    .map_err(|e| of_item(label, i, e))
            })
            .collect::<Result<_>>()?;
        // What they used must be facts the caller may read: checked before anything is written.
        let entries: Vec<&Value> = runs.iter().map(|r| &r.entry).collect();
        crate::qa_log::check_all(store, scope, who, &entries, label)
            .await
            .map_err(relay)?;
        // The day's caps before anything is written: a refused batch leaves no trace behind. Rewards given with
        // runs count as `reward_run`'s do, after the runs' own entries.
        crate::qa_log::runs_cap(store, scope, who, runs.len())
            .await
            .map_err(relay)?;
        let rewards = runs.iter().filter(|r| r.reward.is_some()).count();
        if rewards > 0 {
            crate::qa_log::entries_cap(store, scope, who, runs.len() + rewards)
                .await
                .map_err(relay)?;
        }
        // The traces first, then the log entries naming them: a trace forgotten takes its entry with it, and
        // traces that couldn't be written (a cap) leave no entry behind.
        let episodes: Vec<Episode> = runs.iter().map(|r| r.episode.clone()).collect();
        store.commit(scope, who.user(), &episodes, &[]).await?;
        let logged: Vec<crate::qa_log::RunEntry> = runs
            .iter()
            .map(|r| crate::qa_log::RunEntry {
                id: &r.id,
                entry: &r.entry,
                trace_episode: &r.episode.episode_id,
                reward: r
                    .reward
                    .as_ref()
                    .map(|(reward, reason)| (*reward, reason.as_deref())),
            })
            .collect();
        crate::qa_log::log_runs(store, scope, who, &logged, label)
            .await
            .map_err(relay)?;
        Ok(runs.into_iter().map(|r| r.id).collect())
    }

    /// Each run's id: for a run alone, a random one; for a batch's, the `id` it names or else [`batch_id`]. An id
    /// named twice in the batch, or naming a run another person recorded, is refused.
    async fn ids(
        &self,
        store: &Store,
        scope: &Scope,
        runs: &[Value],
        label: Option<&str>,
    ) -> Result<Vec<String>> {
        if label.is_none() {
            return Ok(runs
                .iter()
                .map(|_| uuid::Uuid::new_v4().to_string())
                .collect());
        }
        let mut ids = Vec::with_capacity(runs.len());
        let mut seen: std::collections::HashMap<String, usize> = Default::default();
        let mut named = false;
        for (i, args) in runs.iter().enumerate() {
            let id = match &args["id"] {
                Value::Null => {
                    // The nth run identical to one before it in the batch.
                    let n = seen.entry(args.to_string()).or_default();
                    *n += 1;
                    batch_id(scope, self.who.user(), args, *n - 1)
                }
                Value::String(s) => {
                    named = true;
                    uuid::Uuid::parse_str(s.trim())
                        .map_err(|_| of_item(label, i, refuse(format!("id is a UUID, not {s:?}"))))?
                        .hyphenated()
                        .to_string()
                }
                _ => {
                    return Err(of_item(
                        label,
                        i,
                        refuse("id is a UUID, as a string".into()),
                    ));
                }
            };
            if ids.contains(&id) {
                return Err(of_item(label, i, refuse(format!("id {id} is named twice"))));
            }
            ids.push(id);
        }
        if named {
            // A run another person recorded keeps its id: a batch can't put its own in its place.
            let theirs: std::collections::HashSet<String> = crate::qa_log::list(store, scope, None)
                .await?
                .into_iter()
                .filter(|l| l.asker != self.who.user())
                .map(|l| l.answer_id)
                .collect();
            if let Some(i) = ids.iter().position(|id| theirs.contains(id)) {
                return Err(of_item(
                    label,
                    i,
                    refuse(format!("id {} is another person's", ids[i])),
                ));
            }
        }
        Ok(ids)
    }

    /// A run's own fields checked, and its trace episode made under `id`; `rewarded`: it may come with its reward.
    fn checked(&self, args: &Value, rewarded: bool, id: String) -> Result<Checked> {
        let task = args["task"].as_str().unwrap_or_default().trim();
        let trace = args["trace"].as_str().unwrap_or_default().trim();
        if task.is_empty() || trace.is_empty() {
            return Err(refuse("task and trace are required".into()));
        }
        if trace.chars().count() > MAX_TRACE {
            return Err(refuse(format!("trace is at most {MAX_TRACE} characters")));
        }
        let outcome = args["outcome"]
            .as_str()
            .map(str::trim)
            .filter(|o| !o.is_empty());
        let advice = advice_key(&args["advice"])?;
        let reward = if rewarded {
            match (&args["reward"], &args["reason"]) {
                (Value::Null, Value::Null) => None,
                (Value::Null, _) => {
                    return Err(refuse(
                        "a reason goes with a reward: give reward too".into(),
                    ));
                }
                (r, reason) => Some(reward_of(r, reason)?),
            }
        } else {
            None
        };
        // A `goal`, from before goals were retired, is accepted and no longer kept. The answer log's question is the
        // task cut to its length (a task can be a whole issue, logs included); the trace below keeps it whole, and
        // that is what the advice learner reads.
        let entry = json!({"question": crate::qa_log::as_question(task), "cited": args["used"], "answer": outcome});
        let used: Vec<&str> = args["used"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        // The advice line sits before `Trace:`, which the advice learner splits the body on.
        let advice = advice.map_or(String::new(), |a| format!("Advice: {a}\n\n"));
        let mut body = format!("Run {id}\n\nTask: {task}\n\n{advice}Trace:\n{trace}\n");
        if !used.is_empty() {
            body.push_str(&format!("\nUsed facts: {}\n", used.join(", ")));
        }
        if let Some(o) = outcome {
            body.push_str(&format!("\nOutcome: {o}\n"));
        }
        let entries: Vec<&str> = self.acl.iter().map(String::as_str).collect();
        let episode = Episode::sourced(
            &format!("{}run:{id}", self.signed),
            body.as_bytes(),
            &entries,
        );
        Ok(Checked {
            id,
            entry,
            episode,
            reward,
        })
    }
}

/// The id of the `nth` (from 0) run of a batch identical to `run`, as `recorder` gives it in `scope`: a UUID made
/// from the SHA-256 of the four (version 8, the kind whose bits an application defines). The same run loaded again
/// by the same person into the same scope, in this store or a fresh one, gets the same id; anyone else's doesn't.
pub fn batch_id(scope: &Scope, recorder: &str, run: &Value, nth: usize) -> String {
    use sha2::{Digest, Sha256};
    let (ns, run, nth) = (scope.namespace(), run.to_string(), nth.to_string());
    let mut h = Sha256::new();
    for part in [
        "memstore run v1",
        ns.as_str(),
        recorder,
        run.as_str(),
        nth.as_str(),
    ] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part.as_bytes());
    }
    let b: [u8; 32] = h.finalize().into();
    uuid::Builder::from_custom_bytes(b[..16].try_into().unwrap())
        .into_uuid()
        .hyphenated()
        .to_string()
}

/// The situation keys `get_advice` named, as `record_run` is given them: one line, one key or several
/// separated by `;` (a run that asked several questions), at most [`MAX_ADVICE_KEYS`] of at most [`MAX_ADVICE`]
/// characters each, with the backticks the advice's `Situation:` line wraps a key in taken off
/// ([`crate::advice::advice_keys`]). Kept once each, in order, joined by `; `. None when absent or empty.
fn advice_key(v: &Value) -> Result<Option<String>> {
    let line = match v {
        Value::Null => return Ok(None),
        Value::String(s) => s.as_str(),
        _ => {
            return Err(refuse(
                "advice is the situation key get_advice named, as a string".into(),
            ));
        }
    };
    if line.chars().any(char::is_control) {
        return Err(refuse(
            "advice is one line: the situation key get_advice named (several separated by `; `)"
                .into(),
        ));
    }
    let mut keys: Vec<&str> = vec![];
    for k in crate::advice::advice_keys(line) {
        if k.chars().count() > MAX_ADVICE {
            return Err(refuse(format!(
                "advice is at most {MAX_ADVICE} characters a key"
            )));
        }
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    if keys.len() > MAX_ADVICE_KEYS {
        return Err(refuse(format!(
            "advice names at most {MAX_ADVICE_KEYS} situation keys"
        )));
    }
    Ok((!keys.is_empty()).then(|| keys.join("; ")))
}

/// A reward and its reason, as `reward_run` takes them: a number from -1 to 1, and at most [`MAX_REASON`]
/// characters of reason (none when absent or empty).
fn reward_of(reward: &Value, reason: &Value) -> Result<(f64, Option<String>)> {
    let Some(r) = reward.as_f64() else {
        return Err(refuse("reward is a number from -1 to 1".into()));
    };
    let reason = reason.as_str().map(str::trim).filter(|r| !r.is_empty());
    if reason.is_some_and(|r| r.chars().count() > MAX_REASON) {
        return Err(refuse(format!("reason is at most {MAX_REASON} characters")));
    }
    if !(-1.0..=1.0).contains(&r) {
        return Err(refuse(format!("reward is from -1 to 1, not {r}")));
    }
    Ok((r, reason.map(String::from)))
}

/// A `reward_run` item: the run's id, its reward and reason, and whether the person gave it (`by: "person"`).
fn reward_item(args: &Value) -> Result<(String, f64, Option<String>, bool)> {
    let id = args["run_id"].as_str().unwrap_or_default().trim();
    if id.is_empty() {
        return Err(refuse("run_id is required".into()));
    }
    let (r, reason) = reward_of(&args["reward"], &args["reason"])?;
    let person = match &args["by"] {
        Value::Null => false,
        v if v.as_str() == Some("person") => true,
        _ => {
            return Err(refuse(
                "by is \"person\" (the person rated it) or absent".into(),
            ));
        }
    };
    Ok((id.to_string(), r, reason, person))
}

/// Rewards a run the caller recorded.
pub async fn reward(store: &Store, scope: &Scope, who: &Principal, args: &Value) -> Result<String> {
    Ok(
        reward_items(store, scope, who, std::slice::from_ref(args), None)
            .await?
            .remove(0),
    )
}

/// Rewards runs the caller recorded, as [`reward`] rewards one, in one commit. Returns their ids, in order. A
/// refusal names its item (`rewards[i]: ...`); nothing is written then.
pub async fn reward_many(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    rewards: &[Value],
) -> Result<Vec<String>> {
    if rewards.is_empty() {
        return Err(refuse("rewards is a list of at least one reward".into()));
    }
    reward_items(store, scope, who, rewards, Some("rewards")).await
}

async fn reward_items(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    items: &[Value],
    label: Option<&str>,
) -> Result<Vec<String>> {
    let items: Vec<(String, f64, Option<String>, bool)> = items
        .iter()
        .enumerate()
        .map(|(i, a)| reward_item(a).map_err(|e| of_item(label, i, e)))
        .collect::<Result<_>>()?;
    let rewards: Vec<(&str, f64, Option<&str>, bool)> = items
        .iter()
        .map(|(id, r, reason, person)| (id.as_str(), *r, reason.as_deref(), *person))
        .collect();
    crate::qa_log::reward_all(store, scope, who, &rewards, label)
        .await
        .map_err(relay)?;
    Ok(items.into_iter().map(|(id, ..)| id).collect())
}
