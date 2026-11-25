//! A platform key's requests ([`crate::people::tenants`]): each names a tenant (`x-memstore-tenant`), which this
//! resolves to its org (making it on first use), and optionally the platform's user in it (`x-memstore-user`),
//! whom it enrolls as a member on first use. The request then goes on as that member, or as the tenant's own
//! service identity, in that org and nowhere else. `/tenants` manages the platform's tenants.
//!
//! What a platform key reaches is a closed list ([`USER_ROUTES`], [`TENANT_ROUTES`]): anything else is refused,
//! and nothing a request says (an org, a team, a scope, a branch) moves it out of its tenant's org.

use std::collections::BTreeSet;
use std::sync::Arc;

use hyper::body::Bytes;
use hyper::{HeaderMap, Method, StatusCode};
use serde_json::{Value, json};

use super::{Reply, State, json_reply, with_limits};
use crate::Principal;
use crate::people::directory::{Actor, Directory, Refused, SignIn};
use crate::people::tenants::{
    self, MemberRemoved, Purging, TemplateChange, User, current_member, plan_template, plan_tenant,
    service_user, tenant_org,
};

/// The header naming the tenant.
pub(super) const TENANT: &str = "x-memstore-tenant";
const TENANT_NAME: &str = "x-memstore-tenant-name";
const USER: &str = "x-memstore-user";
const USER_EMAIL: &str = "x-memstore-user-email";
const USER_NAME: &str = "x-memstore-user-name";

/// The routes a platform request naming a user reaches, as that member.
const USER_ROUTES: &[&str] = &[
    "/sessions",
    "/sessions/end",
    "/advice",
    "/runs",
    "/runs/reward",
    "/search",
    "/query",
    "/episode",
    "/document",
    "/ingest",
    "/remember",
    "/forget",
    "/mcp",
];

/// The routes a platform request naming no user reaches, as the tenant's own service identity: reading what is
/// shared with the whole tenant, and ingesting.
const TENANT_ROUTES: &[&str] = &["/search", "/query", "/episode", "/ingest"];

/// The routes where the member is acted for as a recorder key acts for one (`x-memstore-as`): the session loop and
/// runs, with a recorder's rules and caps, and over `/mcp` only its tools.
const ACTING_ROUTES: &[&str] = &[
    "/sessions",
    "/sessions/end",
    "/advice",
    "/runs",
    "/runs/reward",
    "/mcp",
];

/// What a platform request's headers say: its tenant and user, and their attributes.
#[derive(Default)]
pub(super) struct Headers {
    pub tenant: Option<String>,
    name: Option<String>,
    user: Option<String>,
    email: Option<String>,
    display: Option<String>,
}

impl Headers {
    /// The tenant headers of `h`, checked: an id is 1 to 128 bytes of printable ASCII as sent, an attribute
    /// percent-encoded UTF-8 (RFC 3986). Anything else is the caller's mistake.
    pub(super) fn of(h: &HeaderMap) -> Result<Headers, String> {
        let id = |name: &str| -> Result<Option<String>, String> {
            match h.get(name) {
                None => Ok(None),
                Some(v) => match std::str::from_utf8(v.as_bytes()) {
                    Ok(s) if tenants::valid_id(s) => Ok(Some(s.to_string())),
                    _ => Err(format!("{name} is 1 to 128 bytes of printable ASCII")),
                },
            }
        };
        let attr = |name: &str| -> Result<Option<String>, String> {
            let Some(v) = h.get(name) else {
                return Ok(None);
            };
            let bad = || format!("{name} is percent-encoded UTF-8, at most 512 bytes decoded");
            let s = std::str::from_utf8(v.as_bytes()).map_err(|_| bad())?;
            let decoded = percent_decode(s).ok_or_else(bad)?;
            let s = String::from_utf8(decoded).map_err(|_| bad())?;
            if s.len() > 512 || s.chars().any(char::is_control) {
                return Err(bad());
            }
            Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty()))
        };
        Ok(Headers {
            tenant: id(TENANT)?,
            name: attr(TENANT_NAME)?,
            user: id(USER)?,
            email: attr(USER_EMAIL)?,
            display: attr(USER_NAME)?,
        })
    }

    /// Whether the request sent any of them.
    pub(super) fn any(&self) -> bool {
        self.tenant.is_some()
            || self.name.is_some()
            || self.user.is_some()
            || self.email.is_some()
            || self.display.is_some()
    }

    fn user(&self) -> Option<User> {
        Some(User {
            id: self.user.clone()?,
            email: self.email.clone(),
            name: self.display.clone(),
        })
    }
}

/// `s` with its `%XX` escapes decoded; None if one is malformed.
fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// What the rest of the request is, once its tenant is entered.
pub(super) enum Entered {
    /// Answered here: refused, or a `/tenants`, `/me` or `/admin` request.
    Reply(Reply),
    /// The request goes on in the tenant's `org`, as `who` (`via`): the member, or the tenant's service identity;
    /// `acting` names the member where they are acted for as a recorder key acts for one, and `body` is the body
    /// with its ACLs in member names.
    As {
        who: Principal,
        via: Box<SignIn>,
        org: String,
        acting: Option<String>,
        body: Bytes,
    },
}

/// The request a platform key made, as `handle` read it.
pub(super) struct Request<'a> {
    pub path: &'a str,
    pub method: &'a Method,
    pub query: &'a str,
    pub headers: &'a Headers,
    /// It selected an org, a team or a scope, or a branch or snapshot to read.
    pub selects: bool,
    pub route: &'static str,
}

fn refuse(status: StatusCode, code: &str, error: String) -> Entered {
    Entered::Reply(json_reply(status, json!({"error": error, "code": code})))
}

/// The platform's caller as directory changes see it: the platform, managing its own tenants only.
fn actor(platform: &str) -> Actor {
    Actor {
        user: service_user(platform),
        via: SignIn {
            platform: Some(platform.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The directory read from the catalog now, never a fold from before this call.
async fn fresh(store: &crate::Store) -> anyhow::Result<Arc<crate::people::directory::Folded>> {
    store
        .directory_loaded_after(Some(std::time::Instant::now()))
        .await
}

/// The tenant `tenant` of `platform`, from this server's fold if it holds it, else the catalog read now (servers'
/// folds lag each other by up to `AMPUP_MEMORY_MAX_STALENESS_MS`): its org, if it is live.
async fn find(state: &State, platform: &str, tenant: &str) -> anyhow::Result<Option<String>> {
    let store = state.store.read().unwrap().clone();
    let f = store.directory().await?;
    if let Some(o) = tenant_org(&f.dir, platform, tenant) {
        return Ok(Some(o.to_string()));
    }
    let f = fresh(&store).await?;
    Ok(tenant_org(&f.dir, platform, tenant).map(String::from))
}

/// What [`ensure`] found or made: the tenant's org, whether it was made now, and the user's member name there.
pub(super) struct Ensured {
    pub org: String,
    pub created: bool,
    pub member: Option<String>,
}

/// The tenant orgs this server has seen hold a schema, or given their template's (by the template's schema name
/// then): checked once an org and name per server, on the request after a template names one or a give failed.
static SCHEMA_SEEN: std::sync::LazyLock<std::sync::Mutex<BTreeSet<(String, String)>>> =
    std::sync::LazyLock::new(Default::default);

/// The org of `platform`'s `tenant`, made now (from the platform's template; `label` names it) if it has none, with
/// `user` enrolled in it and their attributes as given. Nothing is written when this server's fold, or failing that
/// the catalog read now, holds all of it already; otherwise it is planned inside the directory's compare-and-set,
/// so two servers making one tenant at once make one org. A tenant deleted and named again gets an org of a name
/// never used ([`tenants::org_name`]); the request that made it purges what is left under the name before.
pub(super) async fn ensure(
    state: &State,
    platform: &str,
    tenant: &str,
    label: Option<&str>,
    user: Option<&User>,
) -> anyhow::Result<Ensured> {
    let store = state.store.read().unwrap().clone();
    let me = service_user(platform);
    let settled = |dir: &Directory| -> anyhow::Result<Option<Ensured>> {
        let p = plan_tenant(dir, &me, platform, tenant, label, user)?;
        Ok(p.events.is_empty().then_some(Ensured {
            org: p.org,
            created: false,
            member: p.member,
        }))
    };
    // The fold this server holds, shared, never copied: the path every request after a tenant's first takes. A
    // refusal there (a user removed, a tenant being deleted) is read again from the catalog before it stands.
    let found = match settled(&store.directory().await?.dir) {
        Ok(Some(e)) => Some(e),
        _ => settled(&fresh(&store).await?.dir)?,
    };
    if let Some(e) = found {
        given_schema(&store, platform, &e.org).await?;
        return Ok(e);
    }
    let made = std::sync::Mutex::new(None);
    let events = store
        .append_directory_with(|dir| {
            let p = plan_tenant(dir, &me, platform, tenant, label, user)?;
            if p.events.iter().any(|e| e.op == "org.create") {
                crate::cell::check_org(&p.org)?;
            }
            *made.lock().unwrap() = Some((p.org, p.member));
            Ok(p.events)
        })
        .await?;
    let (org, member) = made
        .into_inner()
        .unwrap()
        .ok_or_else(|| anyhow::anyhow!("tenant {tenant:?}: no org planned"))?;
    let created = events.iter().any(|e| e.op == "org.create");
    if created {
        store.ensure_scope(&crate::Scope::org(&org)?).await?;
        eprintln!("platform {platform}: tenant {tenant:?} is org {org}, made now");
        // What a server that hadn't seen the tenant's delete wrote to its org before this one: no org holds that
        // name now or ever will again, so only this request, the one that made the new org, purges it.
        if let Some(old) = tenants::previous_incarnation(&org) {
            let dir = Directory::fresh(&store).await?;
            if dir.purged.contains(&old) && !dir.orgs.contains_key(&old) {
                crate::offboard::purge_memory(&store, &old).await?;
            }
        }
    }
    given_schema(&store, platform, &org).await?;
    Ok(Ensured {
        org,
        created,
        member,
    })
}

/// Gives tenant org `org` its platform's template schema (a built-in, by name) if it has none: as it is made, or
/// on a later request (one made before the template named it, or one whose give didn't land). A schema it has is
/// never replaced. Checked once an org and schema name per server.
async fn given_schema(store: &crate::Store, platform: &str, org: &str) -> anyhow::Result<()> {
    let dir = store.directory().await?;
    let Some(name) = dir
        .dir
        .templates
        .get(platform)
        .and_then(|t| t.schema.clone())
    else {
        return Ok(());
    };
    let seen = (org.to_string(), name.clone());
    if SCHEMA_SEEN.lock().unwrap().contains(&seen) {
        return Ok(());
    }
    if let Some(text) = crate::query::schema::builtin(&name) {
        let scope = crate::Scope::org(org)?;
        if store.schema(&scope, crate::MAIN).await?.is_none() {
            let schema = crate::query::schema::Schema::parse(text)?;
            store
                .set_schema(&scope, crate::MAIN, &service_user(platform), &schema, false)
                .await?;
            eprintln!("platform {platform}: org {org} given its template's schema {name}");
        }
    }
    SCHEMA_SEEN.lock().unwrap().insert(seen);
    Ok(())
}

/// A platform request's tenant entered: answered here, or handed on as the member (or the tenant's service
/// identity) in the tenant's org. `via` is the platform key's sign-in.
pub(super) async fn enter(
    state: &Arc<State>,
    req: Request<'_>,
    via: SignIn,
    body: Bytes,
) -> Entered {
    let platform = via.platform.clone().unwrap_or_default();
    // Its tenant decides where it reads and writes: nothing a request names moves it elsewhere.
    if req.selects {
        return refuse(
            StatusCode::BAD_REQUEST,
            "tenant_only",
            "a platform request names its tenant (x-memstore-tenant), never an org, a team, a scope, a branch or a snapshot".into(),
        );
    }
    if req.path == "/tenants" || req.path.starts_with("/tenants/") {
        return Entered::Reply(Box::pin(tenants_route(state, &platform, &req, &body)).await);
    }
    let h = req.headers;
    if req.path == "/admin" {
        return Entered::Reply(Box::pin(admin(state, &platform, h, &body)).await);
    }
    let Some(tenant) = h.tenant.clone() else {
        if req.path == "/me" {
            return Entered::Reply(json_reply(
                StatusCode::OK,
                json!({"user": service_user(&platform), "platform": platform, "groups": [], "scope": null}),
            ));
        }
        return refuse(
            StatusCode::FORBIDDEN,
            "tenant_required",
            format!(
                "a platform key names its tenant in {TENANT} on every request (but GET /me and GET /tenants)"
            ),
        );
    };
    let user = h.user();
    let allowed = match user {
        Some(_) => USER_ROUTES.contains(&req.path) || req.path == "/me",
        None => TENANT_ROUTES.contains(&req.path) || req.path == "/me",
    };
    if !allowed {
        if user.is_none() && USER_ROUTES.contains(&req.path) {
            return refuse(
                StatusCode::FORBIDDEN,
                "user_required",
                format!("{} is a person's: name the user in {USER}", req.path),
            );
        }
        return refuse(
            StatusCode::FORBIDDEN,
            "platform_route",
            format!(
                "a platform key reaches {} (and, for no user, {})",
                USER_ROUTES.join(", "),
                TENANT_ROUTES.join(", ")
            ),
        );
    }
    let (org, member) =
        match ensure(state, &platform, &tenant, h.name.as_deref(), user.as_ref()).await {
            Ok(e) => (e.org, e.member),
            Err(e) => return failed(&platform, &tenant, e),
        };
    if !memstore_core::cell::holds_org(&org) {
        return Entered::Reply(super::misdirected(&org));
    }
    let all = crate::auth::acl::org_wide(&org);
    let orgs = Some(BTreeSet::from([org.clone()]));
    let (who, via, acting) = match &user {
        Some(u) => {
            let Some(member) = member.clone() else {
                return refuse(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "unavailable",
                    "the user's member name wasn't found".into(),
                );
            };
            if state.closures.any_disabled(std::slice::from_ref(&member)) {
                return refuse(
                    StatusCode::FORBIDDEN,
                    "disabled",
                    format!("{} is disabled", u.id),
                );
            }
            let acting = ACTING_ROUTES.contains(&req.path);
            let via = SignIn {
                person: true,
                orgs,
                recorded: acting,
                platform: Some(platform.clone()),
                tenant: Some(tenant.clone()),
                ..Default::default()
            };
            (
                Principal::from_parts(member.clone(), vec![all]),
                via,
                acting.then_some(member),
            )
        }
        None => {
            let via = SignIn {
                orgs,
                platform: Some(platform.clone()),
                tenant: Some(tenant.clone()),
                ..Default::default()
            };
            (
                Principal::from_parts(service_user(&platform), vec![all]),
                via,
                None,
            )
        }
    };
    // What it writes names its readers in the platform's ids: put in member names, in this tenant.
    let store = state.store.read().unwrap().clone();
    let body = match req.path {
        "/ingest" | "/remember" => {
            match translate(&org, &members_of(&store, &org).await, req.path, &body) {
                Ok(b) => b,
                Err(e) => return refuse(StatusCode::BAD_REQUEST, "acl", e),
            }
        }
        _ => body,
    };
    // The platform's use of its tenant, in the tenant's audit: for which member (none: the tenant itself).
    if let Ok(f) = store.directory().await
        && let Some(o) = f.dir.orgs.get(&org)
    {
        let write = !matches!(
            req.path,
            "/search" | "/query" | "/episode" | "/document" | "/me"
        );
        let person = acting_or(&who, user.is_some());
        state.audit.key_use_as(
            (&org, o.created_at),
            &service_user(&platform),
            &format!("org:{org}"),
            req.route,
            write,
            &person,
        );
    }
    Entered::As {
        who,
        via: Box::new(via),
        org,
        acting,
        body,
    }
}

fn acting_or(who: &Principal, member: bool) -> String {
    match member {
        true => who.user().to_string(),
        false => "-".into(),
    }
}

/// A tenant that couldn't be entered: 409 while its org is being deleted, 403 when the directory refused it, 503
/// when the catalog or the object store is busy, 500 otherwise.
fn failed(platform: &str, tenant: &str, e: anyhow::Error) -> Entered {
    if let Some(p) = e.downcast_ref::<Purging>() {
        return Entered::Reply(json_reply(
            StatusCode::CONFLICT,
            json!({"error": p.0, "code": "tenant_purging", "purging": true}),
        ));
    }
    if let Some(r) = e.downcast_ref::<MemberRemoved>() {
        return refuse(StatusCode::FORBIDDEN, "member_removed", r.to_string());
    }
    if let Some(r) = e.downcast_ref::<Refused>() {
        return refuse(StatusCode::FORBIDDEN, "refused", r.0.clone());
    }
    if memstore_core::is_transient(&e) {
        let mut r = json_reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "the catalog is busy: try again shortly"}),
        );
        r.headers_mut()
            .insert(hyper::header::RETRY_AFTER, "5".parse().unwrap());
        return Entered::Reply(r);
    }
    eprintln!("platform {platform}: tenant {tenant:?} unavailable: {e:#}");
    crate::report::failure("tenant", &e);
    refuse(
        StatusCode::INTERNAL_SERVER_ERROR,
        "unavailable",
        "the tenant is unavailable".into(),
    )
}

/// A user's member name in tenant org `org` as this server's directory holds it ([`current_member`]): a user an
/// admin removed keeps the name they had, which no one reads as.
async fn members_of(store: &crate::Store, org: &str) -> impl Fn(&str) -> String + use<> {
    let dir = store.directory().await.ok();
    let org = org.to_string();
    move |user: &str| {
        let held = dir.as_ref().and_then(|f| f.dir.orgs.get(&org));
        current_member(&org, held, user).unwrap_or_else(|MemberRemoved(name)| name)
    }
}

/// An ACL as a platform writes it, in `org`'s member names (`member`): `user:<platform user id>` is that user's
/// member, `group:all` the tenant's org-wide group. Any other entry is refused.
fn translate_acl(
    org: &str,
    member: &dyn Fn(&str) -> String,
    acl: &Value,
    at: &str,
) -> Result<Value, String> {
    let Some(entries) = acl.as_array() else {
        return match acl.is_null() {
            true => Ok(Value::Null),
            false => Err(format!("{at} is a list")),
        };
    };
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let e = e.as_str().unwrap_or_default();
        let entry = match e.split_once(':') {
            Some(("user", id)) if tenants::valid_id(id) => format!("user:{}", member(id)),
            Some(("group", "all")) => format!("group:{}", crate::auth::acl::org_wide(org)),
            _ => {
                return Err(format!(
                    "{at}: {e:?} is not an entry a platform writes: user:<user id> or group:all"
                ));
            }
        };
        out.push(Value::String(entry));
    }
    Ok(Value::Array(out))
}

/// `body` of a `/ingest` (each episode's `acl`) or `/remember` (its `acl`) with its ACLs in `org`'s member names.
fn translate(
    org: &str,
    member: &dyn Fn(&str) -> String,
    path: &str,
    body: &Bytes,
) -> Result<Bytes, String> {
    let mut v: Value = serde_json::from_slice(body).map_err(|_| "body is not JSON".to_string())?;
    match path {
        "/ingest" => {
            if let Some(eps) = v["episodes"].as_array_mut() {
                for (i, e) in eps.iter_mut().enumerate() {
                    if let Some(o) = e.as_object_mut()
                        && let Some(acl) = o.get("acl")
                    {
                        let t = translate_acl(org, member, acl, &format!("episodes[{i}].acl"))?;
                        o.insert("acl".into(), t);
                    }
                }
            }
        }
        _ => {
            if let Some(o) = v.as_object_mut()
                && let Some(acl) = o.get("acl")
            {
                let t = translate_acl(org, member, acl, "acl")?;
                o.insert("acl".into(), t);
            }
        }
    }
    Ok(Bytes::from(v.to_string()))
}

/// `/admin` for a platform key: `show` of the tenant its request names, and its template
/// (`get_tenant_template`, `set_tenant_template`). Nothing else.
async fn admin(state: &Arc<State>, platform: &str, h: &Headers, body: &[u8]) -> Reply {
    let req: Value = serde_json::from_slice(body).unwrap_or_default();
    match req["op"].as_str().unwrap_or_default() {
        "get_tenant_template" | "set_tenant_template" => {
            match template_op(state, &service_user(platform), platform, false, &req).await {
                Ok(v) => json_reply(StatusCode::OK, v),
                Err(e) => error_reply(e),
            }
        }
        "show" => {
            let Some(tenant) = &h.tenant else {
                return json_reply(
                    StatusCode::FORBIDDEN,
                    json!({"error": format!("show names its tenant in {TENANT}"), "code": "tenant_required"}),
                );
            };
            match find(state, platform, tenant).await {
                Ok(Some(org)) if req["org"].as_str().is_none_or(|o| o == org) => {
                    let store = state.store.read().unwrap().clone();
                    let dir = match Directory::load(&store).await {
                        Ok(d) => d,
                        Err(e) => return error_reply(e),
                    };
                    let Some(o) = dir.orgs.get(&org) else {
                        return not_found(tenant);
                    };
                    let mut v = json!({"view": o, "tenant": tenant, "org": org});
                    with_limits(&store, &org, o, &mut v).await;
                    json_reply(StatusCode::OK, v)
                }
                Ok(Some(_)) => json_reply(
                    StatusCode::FORBIDDEN,
                    json!({"error": "a platform key shows only the tenant its request names"}),
                ),
                Ok(None) => not_found(tenant),
                Err(e) => error_reply(e),
            }
        }
        op => json_reply(
            StatusCode::FORBIDDEN,
            json!({"error": format!("a platform key may show, get_tenant_template and set_tenant_template only, not {op:?}")}),
        ),
    }
}

/// `get_tenant_template` and `set_tenant_template` of `platform`, by its own key or the operator (`superuser`).
pub(super) async fn template_op(
    state: &State,
    actor: &str,
    platform: &str,
    superuser: bool,
    req: &Value,
) -> anyhow::Result<Value> {
    use crate::people::directory::Invalid;
    let store = state.store.read().unwrap().clone();
    if req["op"] == "get_tenant_template" {
        let dir = Directory::fresh(&store).await?;
        return Ok(
            json!({"platform": platform, "template": dir.templates.get(platform).cloned().unwrap_or_default()}),
        );
    }
    let change: TemplateChange =
        serde_json::from_value(req.clone()).map_err(|e| Invalid(format!("template: {e}")))?;
    if let Some(k) = unknown_template_key(req, &change) {
        return Err(Invalid(format!(
            "set_tenant_template sets no {k:?}: it takes settings (as set_settings), limits (as set_limits), schema and clear"
        ))
        .into());
    }
    let events = store
        .append_directory_with(|dir| plan_template(dir, actor, platform, superuser, &change))
        .await?;
    let dir = Directory::load(&store).await?;
    Ok(
        json!({"platform": platform, "changes": events.iter().map(|e| e.describe()).collect::<Vec<_>>(),
              "template": dir.templates.get(platform).cloned().unwrap_or_default()}),
    )
}

/// A key `req` names that `set_tenant_template` would drop unread: one it doesn't take, or a setting or limit
/// that isn't one (what was parsed, written back, holds every one it read).
fn unknown_template_key(req: &Value, change: &TemplateChange) -> Option<String> {
    let given = |v: &Value| -> Vec<String> {
        v.as_object()
            .map(|m| {
                m.iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    let top = ["op", "platform", "settings", "limits", "schema", "clear"];
    if let Some(k) = given(req).into_iter().find(|k| !top.contains(&k.as_str())) {
        return Some(k);
    }
    let read = [
        ("settings", serde_json::to_value(&change.settings).ok()),
        ("limits", serde_json::to_value(&change.limits).ok()),
    ];
    read.into_iter().find_map(|(part, back)| {
        let back = back.unwrap_or_default();
        given(&req[part])
            .into_iter()
            .find(|k| back.get(k).is_none_or(Value::is_null))
            .map(|k| format!("{part}.{k}"))
    })
}

fn not_found(tenant: &str) -> Reply {
    json_reply(
        StatusCode::NOT_FOUND,
        json!({"error": format!("no tenant {tenant:?}"), "code": "unknown_tenant"}),
    )
}

/// An error as an answer: the caller's mistake 400, a refusal 403, a busy catalog 503, ours 500.
fn error_reply(e: anyhow::Error) -> Reply {
    use crate::people::directory::Invalid;
    if let Some(Invalid(m)) = e.downcast_ref::<Invalid>() {
        return json_reply(StatusCode::BAD_REQUEST, json!({"error": m}));
    }
    if let Some(Refused(m)) = e.downcast_ref::<Refused>() {
        return json_reply(StatusCode::FORBIDDEN, json!({"error": m}));
    }
    if let Some(p) = e.downcast_ref::<Purging>() {
        return json_reply(
            StatusCode::CONFLICT,
            json!({"error": p.0, "code": "tenant_purging", "purging": true}),
        );
    }
    if memstore_core::is_transient(&e) {
        return json_reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "the catalog is busy: try again shortly"}),
        );
    }
    eprintln!("tenants: {e:#}");
    crate::report::failure("tenants", &e);
    json_reply(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error": "the request failed"}),
    )
}

/// One path segment, percent-decoded: a tenant or user id.
fn segment(s: &str) -> Option<String> {
    let s = String::from_utf8(percent_decode(s)?).ok()?;
    tenants::valid_id(&s).then_some(s)
}

/// The tenants being deleted on this server now: a second delete of one answers 202 while the first runs.
static DELETING: std::sync::LazyLock<std::sync::Mutex<BTreeSet<(String, String)>>> =
    std::sync::LazyLock::new(Default::default);

/// A tenant in [`DELETING`], taken out when this is dropped: a delete that ends any way (done, failed, its task
/// gone) frees it.
struct Deleting((String, String));

impl Drop for Deleting {
    fn drop(&mut self) {
        DELETING.lock().unwrap().remove(&self.0);
    }
}

/// `/tenants`: the platform's tenants. `GET /tenants` lists them, `PUT /tenants/{id}` makes one, `GET` shows one,
/// `DELETE` purges one, and `POST /tenants/{id}/users/{user}/forget` forgets a member.
async fn tenants_route(
    state: &Arc<State>,
    platform: &str,
    req: &Request<'_>,
    body: &[u8],
) -> Reply {
    // Exactly one `/tenants` off the front: `/tenants/tenantsacme` names the tenant `tenantsacme`.
    let rest = req.path.strip_prefix("/tenants").unwrap_or_default();
    let parts: Vec<&str> = match rest.strip_prefix('/') {
        Some(r) => r.split('/').collect(),
        None => vec![""],
    };
    let bad = |m: &str| json_reply(StatusCode::BAD_REQUEST, json!({"error": m}));
    let store = state.store.read().unwrap().clone();
    match (req.method, parts.as_slice()) {
        (&Method::GET, [""]) => list(state, platform, req.query).await,
        (m, [id]) if !id.is_empty() => {
            let Some(tenant) = segment(id) else {
                return bad(
                    "a tenant id is 1 to 128 bytes of printable ASCII, percent-encoded in the path",
                );
            };
            match *m {
                Method::PUT => {
                    let v: Value = match body.iter().all(u8::is_ascii_whitespace) {
                        true => json!({}),
                        false => match serde_json::from_slice(body) {
                            Ok(v) => v,
                            Err(_) => return bad("the body is {name?}"),
                        },
                    };
                    let name = v["name"].as_str().map(str::trim).filter(|n| !n.is_empty());
                    match ensure(state, platform, &tenant, name, None).await {
                        Ok(e) => json_reply(
                            StatusCode::OK,
                            json!({"tenant": tenant, "org": e.org, "created": e.created}),
                        ),
                        Err(e) => error_reply(e),
                    }
                }
                Method::GET => match find(state, platform, &tenant).await {
                    Ok(Some(org)) => {
                        let dir = match Directory::load(&store).await {
                            Ok(d) => d,
                            Err(e) => return error_reply(e),
                        };
                        let Some(o) = dir.orgs.get(&org) else {
                            return not_found(&tenant);
                        };
                        let mut v = json!({"tenant": tenant, "org": org,
                                           "created_at": crate::audit::rfc3339(o.created_at)});
                        with_limits(&store, &org, o, &mut v).await;
                        if let Some(m) = v.as_object_mut() {
                            m.remove("view");
                        }
                        json_reply(StatusCode::OK, v)
                    }
                    Ok(None) => not_found(&tenant),
                    Err(e) => error_reply(e),
                },
                Method::DELETE => delete(state, platform, &tenant).await,
                _ => json_reply(
                    StatusCode::METHOD_NOT_ALLOWED,
                    json!({"error": "PUT, GET or DELETE /tenants/{id}"}),
                ),
            }
        }
        (&Method::POST, [id, "users", user, "forget"]) => {
            let (Some(tenant), Some(user)) = (segment(id), segment(user)) else {
                return bad(
                    "tenant and user ids are 1 to 128 bytes of printable ASCII, percent-encoded in the path",
                );
            };
            let org = match find(state, platform, &tenant).await {
                Ok(Some(org)) => org,
                Ok(None) => return not_found(&tenant),
                Err(e) => return error_reply(e),
            };
            if !memstore_core::cell::holds_org(&org) {
                return super::misdirected(&org);
            }
            // The member they are now: a user forgotten before is another member since.
            let member = match Directory::fresh(&store).await {
                Ok(d) => current_member(&org, d.orgs.get(&org), &user)
                    .unwrap_or_else(|MemberRemoved(name)| name),
                Err(e) => return error_reply(e),
            };
            match super::forget_user_for_good(state, &actor(platform), &member, Some(&org)).await {
                Ok(mut v) => {
                    v["tenant"] = json!(tenant);
                    v["user"] = json!(user);
                    json_reply(StatusCode::OK, v)
                }
                Err(e) => error_reply(e),
            }
        }
        _ => json_reply(
            StatusCode::NOT_FOUND,
            json!({"error": "GET /tenants, PUT|GET|DELETE /tenants/{id}, POST /tenants/{id}/users/{user}/forget"}),
        ),
    }
}

/// `GET /tenants?after=&limit=`: the platform's tenants in id order, each with its org, when it was made and this
/// month's model tokens (this server's count of every server's); `next` is the `after` of the next page.
async fn list(state: &State, platform: &str, query: &str) -> Reply {
    let q: std::collections::HashMap<String, String> = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .filter_map(|(k, v)| Some((k.to_string(), String::from_utf8(percent_decode(v)?).ok()?)))
        .collect();
    let limit = q
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let after = q.get("after").cloned();
    let store = state.store.read().unwrap().clone();
    let dir = match Directory::load(&store).await {
        Ok(d) => d,
        Err(e) => return error_reply(e),
    };
    let month = crate::limits::month_of(chrono::Utc::now());
    let all = tenants::tenants_of(&dir, platform);
    let page: Vec<(&str, &str)> = all
        .into_iter()
        .filter(|(t, _)| after.as_deref().is_none_or(|a| *t > a))
        .take(limit + 1)
        .collect();
    let next = (page.len() > limit).then(|| page[limit - 1].0.to_string());
    let tenants: Vec<Value> = page
        .iter()
        .take(limit)
        .map(|(t, o)| match dir.orgs.get(*o) {
            Some(org) => json!({"tenant": t, "org": o, "created_at": crate::audit::rfc3339(org.created_at),
                                "limits": crate::limits::limits_json(&org.limits),
                                "usage": {"month": month, "model_tokens": crate::limits::used(o, &month)}}),
            None => json!({"tenant": t, "org": o, "purging": true}),
        })
        .collect();
    json_reply(StatusCode::OK, json!({"tenants": tenants, "next": next}))
}

/// `DELETE /tenants/{id}`: the tenant's org deleted and its memory purged, as `delete_org` does; the tenant is
/// free once the purge is done, and the next request naming it makes a new, empty org. 404 for a tenant the
/// platform doesn't have; 202 while a delete of it is running here, or one that stopped halfway (repeating it
/// finishes it).
async fn delete(state: &Arc<State>, platform: &str, tenant: &str) -> Reply {
    let store = state.store.read().unwrap().clone();
    let dir = match Directory::fresh(&store).await {
        Ok(d) => d,
        Err(e) => return error_reply(e),
    };
    let key = (platform.to_string(), tenant.to_string());
    let Some(org) = dir.tenants.get(&key).cloned() else {
        return not_found(tenant);
    };
    if !memstore_core::cell::holds_org(&org) {
        return super::misdirected(&org);
    }
    if !DELETING.lock().unwrap().insert(key.clone()) {
        return json_reply(
            StatusCode::ACCEPTED,
            json!({"tenant": tenant, "org": org, "purging": true}),
        );
    }
    let deleting = Deleting(key);
    // In a task of its own: a caller that hangs up doesn't stop the purge halfway, or leave the tenant marked.
    let (st, p, o) = (state.clone(), platform.to_string(), org.clone());
    let done = tokio::spawn(async move {
        let _deleting = deleting;
        super::delete_org_for_good(&st, &actor(&p), &o).await
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("the delete stopped: {e}")));
    match done {
        Ok(_) => {
            eprintln!("platform {platform}: tenant {tenant:?} deleted (org {org} purged)");
            json_reply(
                StatusCode::OK,
                json!({"tenant": tenant, "org": org, "deleted": true}),
            )
        }
        Err(e) => {
            eprintln!("platform {platform}: deleting tenant {tenant:?} (org {org}) stopped: {e:#}");
            // Deleted in the directory, its purge not finished: nothing is served, and a repeat finishes it.
            match Directory::fresh(&store).await {
                Ok(d) if d.purging.contains(&org) => json_reply(
                    StatusCode::ACCEPTED,
                    json!({"tenant": tenant, "org": org, "purging": true,
                           "error": "the purge stopped partway: repeat the delete to finish it"}),
                ),
                _ => error_reply(e),
            }
        }
    }
}
