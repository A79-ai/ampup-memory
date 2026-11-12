//! Admin operations record and launch Kubernetes Jobs. No dream work runs in the API process.
use super::*;
use crate::dream_jobs::Request;
use crate::dream_jobs::kubernetes::{Authorization, Launcher, control};

pub(super) async fn handle(
    state: &Arc<State>,
    who: &Principal,
    via: &SignIn,
    superuser: bool,
    req: &Value,
) -> Result<Value> {
    use crate::people::directory::{Directory, Refused};
    let store = state.store.read().unwrap().clone();
    let fields: &[&str] = match req["op"].as_str() {
        Some("create_dream") => &["op", "org", "team", "model", "request"],
        Some("get_dream" | "run_dream" | "cancel_dream") => &["op", "org", "team", "id"],
        Some("list_dreams") => &["op", "org", "team", "limit", "after", "summary", "statuses"],
        _ => anyhow::bail!("unknown dream operation"),
    };
    anyhow::ensure!(
        req.as_object()
            .is_some_and(|m| m.keys().all(|k| fields.contains(&k.as_str()))),
        "unknown dream operation field"
    );
    if let Some(team) = req.get("team") {
        anyhow::ensure!(team.is_string(), "team must be a string");
    }
    if let Some(model) = req.get("model") {
        anyhow::ensure!(model.is_string(), "model must be a string");
    }
    let org = req["org"].as_str().unwrap_or_default();
    crate::check_name("org", org).map_err(|e| Refused(e.to_string()))?;
    let dir = Directory::fresh(&store).await?;
    if !superuser && !dir.is_admin(who.user(), via, org) {
        return Err(Refused("dream jobs require an org admin".into()).into());
    }
    let scope = match req["team"].as_str().filter(|s| !s.is_empty()) {
        Some(team) if superuser => Scope::team(org, team)?,
        Some(team) => dir.resolve(who.user(), via, Some(org), Some(team))?,
        None => Scope::org(org)?,
    };
    let reader = as_member(who, org);
    let id = req["id"].as_str().unwrap_or_default();
    // Durable records remain readable when the launcher is unconfigured or Kubernetes is unavailable.
    if req["op"] == "get_dream" {
        let mut job = crate::dream_jobs::get_for_admin(&store, &scope, &reader, id).await?;
        let owner = Principal::new(&job.creator, &[]);
        let observer = if job.request.mode == crate::dream_jobs::Mode::Reconcile {
            &owner
        } else {
            &reader
        };
        let observation = if job.executions.is_empty()
            || matches!(
                job.status,
                crate::dream_jobs::Status::Completed
                    | crate::dream_jobs::Status::Failed
                    | crate::dream_jobs::Status::Canceled
            ) {
            json!({"state":"record_only"})
        } else {
            match Launcher::from_env() {
                Ok(launcher) => {
                    match launcher
                        .observe(&store, &scope, observer, job.clone())
                        .await
                    {
                        Ok(observed) => {
                            job = observed;
                            json!({"state":"observed", "checked_at":crate::now_micros()})
                        }
                        Err(e) => json!({"state":"unavailable", "error":writer::redacted(&e)}),
                    }
                }
                Err(e) => json!({"state":"not_configured", "error":writer::redacted(&e)}),
            }
        };
        let mut view = api_view(job)?;
        // This is an infrastructure read timestamp, never a worker heartbeat or percentage complete.
        view["observation"] = observation;
        return Ok(view);
    }
    if req["op"] == "list_dreams" {
        if let Some(summary) = req.get("summary") {
            anyhow::ensure!(summary.is_boolean(), "summary must be a boolean");
        }
        if let Some(after) = req.get("after") {
            anyhow::ensure!(
                after.is_null() || after.is_string(),
                "after must be a dream ID"
            );
        }
        if let Some(limit) = req.get("limit") {
            anyhow::ensure!(limit.is_u64(), "limit must be 1–100");
        }
        let limit = req["limit"].as_u64().unwrap_or(20);
        anyhow::ensure!((1..=100).contains(&limit), "limit must be 1–100");
        let mut jobs = crate::dream_jobs::list_for_admin(&store, &scope, &reader).await?;
        if !req["statuses"].is_null() {
            let statuses: Vec<crate::dream_jobs::Status> =
                serde_json::from_value(req["statuses"].clone())
                    .context("statuses must be an array of dream statuses")?;
            anyhow::ensure!(
                !statuses.is_empty() && statuses.len() <= 5,
                "select 1–5 statuses"
            );
            jobs.retain(|j| statuses.contains(&j.status));
        }
        let start = match req["after"].as_str() {
            None => 0,
            Some(id) => {
                jobs.iter()
                    .position(|j| j.id == id)
                    .context("unknown dream page cursor")?
                    + 1
            }
        };
        let end = (start + limit as usize).min(jobs.len());
        let next = (end < jobs.len()).then(|| jobs[end - 1].id.clone());
        let data = if req["summary"].as_bool().unwrap_or(false) {
            jobs[start..end]
                .iter()
                .map(crate::dream_jobs::Job::summary)
                .collect::<Vec<_>>()
        } else {
            jobs[start..end]
                .iter()
                .cloned()
                .map(api_view)
                .collect::<Result<Vec<_>>>()?
        };
        return Ok(json!({"data":data, "next_page":next}));
    }
    if !cell::mine(&scope) {
        return Err(Refused(
            "select the org's cell (?org= or x-memstore-org) to write dream jobs".into(),
        )
        .into());
    }
    let launcher = if req["op"] == "cancel_dream" {
        None
    } else {
        Some(Launcher::from_env()?)
    };
    let job = if req["op"] == "create_dream" {
        let model = crate::extract::Extractor::dream_from_env()?
            .context("no dreaming model is configured")?;
        let request: Request =
            serde_json::from_value(req["request"].clone()).context("invalid dream request")?;
        crate::dream_jobs::create(
            &store,
            &scope,
            &reader,
            req["model"].as_str().unwrap_or(model.model()),
            request,
        )
        .await?
    } else {
        crate::dream_jobs::get_for_admin(&store, &scope, &reader, id).await?
    };
    let owner = Principal::new(&job.creator, &[]);
    let actor = reader.clone();
    let reader = if job.request.mode == crate::dream_jobs::Mode::Reconcile {
        owner
    } else {
        reader
    };
    let turn = control(&store, &scope, &job.id).await?;
    let writing = turn.store(&store);
    let result = async {
        let job = crate::dream_jobs::get(&writing, &scope, &reader, &job.id).await?;
        if req["op"] == "cancel_dream" {
            let canceled = crate::dream_jobs::cancel(&writing, &scope, &reader, &job.id).await?;
            if !canceled.executions.is_empty() {
                Launcher::from_env()?.cancel(&canceled).await?;
            }
            return Ok(canceled);
        }
        launcher
            .as_ref()
            .expect("launch configuration checked before recording")
            .launch(
                &writing,
                &scope,
                &reader,
                job,
                Authorization::new(&actor, via, superuser),
            )
            .await
    }
    .await;
    turn.finish(&writing).await;
    api_view(result?)
}

/// A dream as the API returns it. The approved worker identity (claims, groups, the sign-in's other orgs) stays in
/// the durable record for the worker; scope admins reading someone else's run never see it.
fn api_view(job: crate::dream_jobs::Job) -> Result<Value> {
    let mut view = serde_json::to_value(job)?;
    if let Some(fields) = view.as_object_mut() {
        fields.remove("authorization");
    }
    Ok(view)
}
