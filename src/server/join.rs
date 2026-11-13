//! Joining an org by asking: a person signed in through an identity provider, whom the directory doesn't
//! hold in the org they want (`memstore join`, the plugin's `/ampup-memory:login`), asks to join it (`/admin`
//! op `request_join`: [`crate::people::directory::Change::RequestJoin`]). The request is recorded in the
//! directory (`pending`, granting nothing), and each admin of the org is emailed ([`crate::mail`]) a link
//! that approves it: the person becomes a member, as `memstore invite` makes them. `memstore requests`
//! lists what waits; `memstore invite` and `memstore decline` answer from a terminal.
//!
//! The link is a token this server sealed under its `AMPUP_MEMORY_SECRET_KEY`: the org, the person, the admin
//! it was sent to and when it expires ([`APPROVE_DAYS`]). It opens a page that asks before anything changes
//! (a GET changes nothing); the page's POST approves, as that admin, through the directory's own checks:
//! an admin removed since, or a request answered since, approves nothing. Without a secret key, or a
//! public URL, the mail names the commands instead.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use serde::{Deserialize, Serialize};

use super::State;
use crate::mail;
use crate::people::directory::{
    Actor, Change, Directory, Role, SignIn, SyncMember, apply, user_id,
};

/// How long an approval link works.
pub const APPROVE_DAYS: i64 = 7;
/// The path the link opens, under the server's public URL.
pub(super) const APPROVE_PATH: &str = "/join/approve";
const CONTEXT: &str = "join-approve";

/// What an approval link carries.
#[derive(Serialize, Deserialize)]
struct Approval {
    org: String,
    user: String,
    /// The admin the link was mailed to: the approval is theirs.
    admin: String,
    /// When the person asked (the directory's `pending` time): the link answers that request, not a later one.
    at: i64,
    /// Seconds since the epoch.
    exp: i64,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A sealed token as a URL carries it (base64's `+/=` would need escaping).
fn to_url(sealed: &str) -> String {
    sealed
        .trim_start_matches("sealed:")
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_string()
}

fn from_url(t: &str) -> String {
    let mut s = t.replace('-', "+").replace('_', "/");
    while !s.len().is_multiple_of(4) {
        s.push('=');
    }
    format!("sealed:{s}")
}

/// The link that lets `admin` approve `user` joining `org`, if this server can make one.
fn approve_link(state: &State, org: &str, user: &str, at: i64, admin: &str) -> Option<String> {
    let key = state.secret_key.as_ref()?;
    let base = std::env::var("AMPUP_MEMORY_PUBLIC_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())?;
    let a = Approval {
        org: org.to_string(),
        user: user.to_string(),
        admin: admin.to_string(),
        at,
        exp: now() + APPROVE_DAYS * 86_400,
    };
    let sealed = key.seal(CONTEXT, &serde_json::to_string(&a).ok()?);
    Some(format!(
        "{}{APPROVE_PATH}?t={}",
        base.trim_end_matches('/'),
        to_url(&sealed)
    ))
}

/// What a link says, if it is one this server made and it hasn't expired.
fn open_link(state: &State, t: &str) -> Result<Approval> {
    let key = state
        .secret_key
        .as_ref()
        .context("this server has no AMPUP_MEMORY_SECRET_KEY: it made no approval links")?;
    let plain = key.unseal(CONTEXT, &from_url(t))?;
    let a: Approval = serde_json::from_str(&plain)?;
    anyhow::ensure!(a.exp > now(), "expired");
    Ok(a)
}

/// What a link that doesn't open is told: the same whatever the reason (the log has it).
const BAD_LINK: &str = "<p>This link doesn't work: it isn't one this server made, or it has expired (a link works for 7 days). Ask the person to request again, or approve from a terminal: <code>ampup-memory requests</code>, then <code>ampup-memory invite</code>.</p>";

/// Tell `org`'s admins that `user` asked to join: one mail each, with a link that is that admin's to
/// use. Runs in the background after the request is recorded; a failure is logged, and the request stays
/// (`memstore requests --org {org}`).
pub(super) async fn notify(state: Arc<State>, org: String, user: String) {
    let store = state.store.read().unwrap().clone();
    let dir = match Directory::load(&store).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "join request: {user} asked to join {org}, but the directory can't be read to tell its admins: {e:#}"
            );
            return;
        }
    };
    let Some(at) = dir
        .orgs
        .get(&org)
        .and_then(|o| o.pending.get(&user).copied())
    else {
        eprintln!(
            "join request: {user} asked to join {org}, and was answered before its admins were told"
        );
        return;
    };
    // Its admins with an email, less anyone disabled (`user.disable`): no link goes to someone locked out.
    let admins: Vec<String> = dir
        .orgs
        .get(&org)
        .map(|o| {
            o.members
                .iter()
                .filter(|(u, r)| **r == Role::Admin && u.contains('@'))
                .map(|(u, _)| u.clone())
                .filter(|u| {
                    !dir.disabled
                        .get(&crate::auth::acl::disabled_id(u))
                        .copied()
                        .unwrap_or(false)
                        && !state.closures.any_disabled(std::slice::from_ref(u))
                })
                .collect()
        })
        .unwrap_or_default();
    let how = format!(
        "ampup-memory invite --org {org} --email {user} (or ampup-memory decline --org {org} --email {user})"
    );
    if admins.is_empty() {
        eprintln!(
            "join request: {user} asked to join {org}, which has no admin with an email address to tell: {how}"
        );
        return;
    }
    let Some(mailer) = mail::from_env() else {
        eprintln!(
            "join request: {user} asked to join {org}; no mail is configured (AMPUP_MEMORY_MAIL_FROM with RESEND_API_KEY or MAILERSEND_API_KEY), so its admins ({}) weren't emailed: {how}",
            admins.join(", ")
        );
        return;
    };
    let server = std::env::var("AMPUP_MEMORY_PUBLIC_URL").unwrap_or_else(|_| "AmpUp Memory".into());
    let subject = format!("{user} asked to join {org} on AmpUp Memory");
    // The terminal way, one command a step, each on its own so it copies whole: sign in as an admin, then
    // invite (or decline).
    let sign_in = format!("ampup-memory join {server} --org {org}");
    let invite = format!("ampup-memory invite --org {org} --email {user}");
    let decline = format!("ampup-memory decline --org {org} --email {user}");
    for admin in admins {
        let link = approve_link(&state, &org, &user, at, &admin);
        let text = match &link {
            Some(l) => format!(
                "{user} asked to join {org} on AmpUp Memory ({server}).\n\nApproving makes them a member: they read and write {org}'s memory as its members do. Approve people you know.\n\nApprove, as {admin}:\n{l}\n\nOr from a terminal:\n\n1. Sign in as an admin of {org}:\n   {sign_in}\n\n2. Let them in:\n   {invite}\n\nOr turn them away (they can ask again tomorrow):\n   {decline}\n\nThe link works for {APPROVE_DAYS} days, for this request only, and only for you."
            ),
            None => format!(
                "{user} asked to join {org} on AmpUp Memory ({server}).\n\nApproving makes them a member: they read and write {org}'s memory as its members do. Approve people you know.\n\nFrom a terminal:\n\n1. Sign in as an admin of {org}:\n   {sign_in}\n\n2. Let them in:\n   {invite}\n\nOr turn them away (they can ask again tomorrow):\n   {decline}"
            ),
        };
        let html = mail_html(MailParts {
            user: &user,
            org: &org,
            server: &server,
            admin: &admin,
            link: link.as_deref(),
            sign_in: &sign_in,
            invite: &invite,
            decline: &decline,
        });
        match mailer.send(&admin, &subject, &text, &html).await {
            Ok(()) => eprintln!(
                "join request: {user} asked to join {org}; told {admin} by {}",
                mailer.provider()
            ),
            Err(e) => eprintln!(
                "join request: {user} asked to join {org}; mail to {admin} failed: {e:#} ({how})"
            ),
        }
    }
}

struct MailParts<'a> {
    user: &'a str,
    org: &'a str,
    server: &'a str,
    admin: &'a str,
    link: Option<&'a str>,
    sign_in: &'a str,
    invite: &'a str,
    decline: &'a str,
}

/// The mail's HTML: what mail clients render (a table, inline styles, no script), the approve button first,
/// then the terminal way with each command in a block of its own, so a triple-click selects one whole.
fn mail_html(p: MailParts<'_>) -> String {
    let e = mail::esc;
    let code = |c: &str| {
        format!(
            "<pre style=\"margin:6px 0 14px;padding:10px 12px;background:#f4f4f5;border:1px solid #e4e4e7;border-radius:6px;font:13px/1.5 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;white-space:pre-wrap;word-break:break-all\">{}</pre>",
            e(c)
        )
    };
    let p_style = "margin:0 0 14px;font:15px/1.5 -apple-system,Segoe UI,Helvetica,Arial,sans-serif;color:#18181b";
    let muted = "margin:0 0 14px;font:13px/1.5 -apple-system,Segoe UI,Helvetica,Arial,sans-serif;color:#71717a";
    let mut body = String::new();
    body.push_str(&format!(
        "<h2 style=\"margin:0 0 14px;font:600 20px/1.3 -apple-system,Segoe UI,Helvetica,Arial,sans-serif;color:#18181b\">{} asked to join {}</h2>",
        e(p.user),
        e(p.org)
    ));
    body.push_str(&format!(
        "<p style=\"{p_style}\">They signed in to AmpUp Memory at <a href=\"{0}\" style=\"color:#2563eb\">{0}</a> for <b>{1}</b>, which doesn't hold them. Approving makes them a member: they read and write {1}'s memory as its members do. Approve people you know.</p>",
        e(p.server),
        e(p.org)
    ));
    if let Some(l) = p.link {
        body.push_str(&format!(
            "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"margin:0 0 8px\"><tr><td style=\"background:#18181b;border-radius:6px\"><a href=\"{}\" style=\"display:inline-block;padding:11px 20px;font:600 15px -apple-system,Segoe UI,Helvetica,Arial,sans-serif;color:#ffffff;text-decoration:none\">Approve {}</a></td></tr></table>",
            e(l),
            e(p.user)
        ));
        body.push_str(&format!(
            "<p style=\"{muted}\">The button works for {APPROVE_DAYS} days, for this request only, and only for you ({}). If it doesn't open, paste this:</p>",
            e(p.admin)
        ));
        body.push_str(&code(l));
    }
    body.push_str(&format!(
        "<h3 style=\"margin:22px 0 10px;font:600 16px/1.3 -apple-system,Segoe UI,Helvetica,Arial,sans-serif;color:#18181b\">Or from a terminal</h3>         <p style=\"{p_style}\"><b>1.</b> Sign in as an admin of {}:</p>{}         <p style=\"{p_style}\"><b>2.</b> Let them in:</p>{}         <p style=\"{p_style}\">Or turn them away (they can ask again tomorrow):</p>{}",
        e(p.org),
        code(p.sign_in),
        code(p.invite),
        code(p.decline)
    ));
    format!(
        "<!doctype html><html><body style=\"margin:0;padding:24px;background:#ffffff\"><div style=\"max-width:640px\">{body}</div></body></html>"
    )
}

/// `GET` or `POST /join/approve`: the page that asks, or the approval.
///
/// Boxed before the caller sees it: a debug build reserves a temporary's room in the caller's poll frame
/// even for a future it boxes at once, and the server's request frames sit at what a test thread's stack
/// takes (`advice_never_blocks_agents` overflowed on CI with the future inline).
pub(super) fn route(
    state: Arc<State>,
    req: hyper::Request<hyper::body::Incoming>,
    query: String,
) -> Pin<Box<dyn Future<Output = hyper::Response<Full<Bytes>>> + Send>> {
    Box::pin(serve(state, req, query))
}

async fn serve(
    state: Arc<State>,
    req: hyper::Request<hyper::body::Incoming>,
    query: String,
) -> hyper::Response<Full<Bytes>> {
    let html = |s: Shown| {
        hyper::Response::builder()
            .status(s.status)
            .header("content-type", "text/html; charset=utf-8")
            .header("cache-control", "no-store")
            .body(Full::new(Bytes::from(s.html)))
            .unwrap()
    };
    match *req.method() {
        hyper::Method::GET => html(ask(&state, token_in(&query).as_deref())),
        hyper::Method::POST => {
            // The token is a few hundred bytes; the route is reached before any sign-in.
            let form = http_body_util::Limited::new(req.into_body(), 4 << 10)
                .collect()
                .await
                .map(|b| String::from_utf8_lossy(&b.to_bytes()).into_owned())
                .unwrap_or_default();
            html(approve(&state, token_in(&form).as_deref()).await)
        }
        _ => hyper::Response::builder()
            .status(405)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(
                serde_json::json!({"error": "GET or POST"}).to_string(),
            )))
            .unwrap(),
    }
}

/// A page: the server's own, plain, with no script.
pub(super) fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>body{{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;color:#111}}button{{font:inherit;padding:.5rem 1rem;border-radius:.4rem;border:1px solid #333;background:#fff;cursor:pointer}}code{{background:#f3f3f3;padding:.1rem .3rem}}</style></head><body>{}</body></html>",
        mail::esc(title),
        body
    )
}

/// The reply to a link: the page (`GET`, the question) or the outcome (`POST`, the approval).
pub(super) struct Shown {
    pub(super) status: u16,
    pub(super) html: String,
}

pub(super) fn shown(status: u16, title: &str, body: &str) -> Shown {
    Shown {
        status,
        html: page(title, body),
    }
}

/// The token in a link's query or a form's body (`t=…`).
pub(super) fn token_in(query_or_form: &str) -> Option<String> {
    query_or_form
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == "t")
        .map(|(_, v)| v.to_string())
        .filter(|v| {
            !v.is_empty()
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// `GET /join/approve?t=…`: what the link would do, and a button that does it. Nothing changes here.
pub(super) fn ask(state: &State, token: Option<&str>) -> Shown {
    let Some(t) = token else {
        return shown(400, "AmpUp Memory", "<p>This isn't an approval link.</p>");
    };
    match open_link(state, t) {
        Ok(a) => shown(
            200,
            "Approve joining AmpUp Memory",
            &format!(
                "<p><b>{}</b> asked to join <b>{}</b> on AmpUp Memory.</p><p>Approving makes them a member: they read and write {}'s memory as its members do. Approve people you know.</p><form method=\"post\" action=\"{APPROVE_PATH}\"><input type=\"hidden\" name=\"t\" value=\"{}\"><button type=\"submit\">Approve, as {}</button></form><p>To turn them away instead, from a terminal: <code>ampup-memory decline --org {} --email {}</code>.</p>",
                mail::esc(&a.user),
                mail::esc(&a.org),
                mail::esc(&a.org),
                mail::esc(t),
                mail::esc(&a.admin),
                mail::esc(&a.org),
                mail::esc(&a.user)
            ),
        ),
        Err(e) => {
            eprintln!("join request: an approval link didn't open: {e:#}");
            shown(400, "AmpUp Memory", BAD_LINK)
        }
    }
}

/// `POST /join/approve` (`t=…`): approve, as the admin the link was sent to, through the directory's checks.
pub(super) async fn approve(state: &State, token: Option<&str>) -> Shown {
    let Some(t) = token else {
        return shown(400, "AmpUp Memory", "<p>This isn't an approval link.</p>");
    };
    let a = match open_link(state, t) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("join request: an approval link didn't open: {e:#}");
            return shown(400, "AmpUp Memory", BAD_LINK);
        }
    };
    // A link mailed to an admin disabled since (`user.disable`, which a bearer sign-in would meet at
    // authentication): locked out here too.
    if state.closures.any_disabled(std::slice::from_ref(&a.admin)) {
        eprintln!(
            "join request: approval of {} in {} refused: {} is disabled",
            a.user, a.org, a.admin
        );
        return shown(
            403,
            "AmpUp Memory",
            "<p>Not approved: this account is disabled.</p>",
        );
    }
    let store = state.store.read().unwrap().clone();
    // Answered since (invited, or turned away, or asked again after that): the link is spent. Read fresh,
    // not from this server's cache: a decline a moment ago counts.
    let fresh = Directory::fresh(&store).await.ok();
    // Disabled in the directory's own log too (this server's closures may lag another server's disable).
    if fresh.as_ref().is_some_and(|d| {
        d.disabled
            .get(&crate::auth::acl::disabled_id(&a.admin))
            .copied()
            .unwrap_or(false)
    }) {
        eprintln!(
            "join request: approval of {} in {} refused: {} is disabled",
            a.user, a.org, a.admin
        );
        return shown(
            403,
            "AmpUp Memory",
            "<p>Not approved: this account is disabled.</p>",
        );
    }
    let waiting = fresh
        .as_ref()
        .and_then(|d| {
            d.orgs
                .get(&a.org)
                .map(|o| o.pending.get(&user_id(&a.user)) == Some(&a.at))
        })
        .unwrap_or(false);
    if !waiting {
        return shown(
            409,
            "AmpUp Memory",
            &format!(
                "<p>This request was answered already: {} is a member of {}, or was turned away.</p>",
                mail::esc(&a.user),
                mail::esc(&a.org)
            ),
        );
    }
    let actor = Actor {
        user: a.admin.clone(),
        superuser: false,
        via: SignIn {
            person: true,
            verified_email: true,
            ..Default::default()
        },
        owner: false,
        file_keys: Default::default(),
    };
    let change = Change::Invite {
        org: a.org.clone(),
        members: vec![SyncMember {
            user: a.user.clone(),
            role: Role::Member,
            teams: vec![],
        }],
    };
    match apply(&store, &actor, &change).await {
        Ok(events) => {
            for e in &events {
                eprintln!(
                    "join request: {} approved by link: {}",
                    a.admin,
                    e.describe()
                );
            }
            *state.providers.lock().unwrap() = None;
            shown(
                200,
                "Approved",
                &format!(
                    "<p>Done: <b>{}</b> is a member of <b>{}</b>. They sign in with <code>/ampup-memory:login</code> (or <code>ampup-memory join</code>) and memory is on from their next session.</p>",
                    mail::esc(&a.user),
                    mail::esc(&a.org)
                ),
            )
        }
        Err(e) => {
            eprintln!(
                "join request: approval of {} in {} by {} refused: {e:#}",
                a.user, a.org, a.admin
            );
            shown(
                403,
                "AmpUp Memory",
                &format!("<p>Not approved: {}.</p>", mail::esc(&format!("{e:#}"))),
            )
        }
    }
}
