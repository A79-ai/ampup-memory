//! Signing in with a password whose email nobody verified. The platform's Auth0 tenant lets anyone make a
//! password account under any email, so such a sign-in proves nothing about the email, and on its own reaches
//! nothing ([`crate::people::directory::Directory::access`]). A member of an org who signs in that way, for
//! the org's own Auth0 Organization, asks its admins instead: `/me` records the request
//! ([`Change::RequestSignIn`], the directory's `signin_requests`) and each admin is emailed. An admin approves
//! it in the console (the org's People page) or with `/admin` op `approve_signin`: that Auth0 account, and no
//! other, then signs in to that org as them, and to no other org. Verifying the email in Auth0 still works
//! too, and needs no approval.

use std::sync::Arc;

use serde_json::{Value, json};

use super::State;
use crate::mail;
use crate::people::directory::{Actor, Change, Directory, Role, SignIn, apply, user_id};

/// What `/me` says about an unverified sign-in that the directory doesn't let in yet, after asking the
/// org's admins if it hadn't: `{"org", "status"}`, where status is `waiting` (an admin is to approve it),
/// `declined` (an admin turned it down today), `not_recorded` (the request couldn't be recorded: the log
/// says why) or `not_member` (the org doesn't hold this email: no admin
/// is asked, since approving would let in someone nobody added). None: not such a sign-in, or approved.
pub(super) async fn on_me(
    state: &Arc<State>,
    user: &str,
    via: &SignIn,
    dir: &Directory,
) -> Option<Value> {
    let sub = via.unverified.as_deref()?;
    let org = dir.auth0_bound(via.auth0_org.as_deref())?.to_string();
    let o = dir.orgs.get(&org)?;
    let u = user_id(user);
    if o.signin_approved.get(&u).map(String::as_str) == Some(sub) {
        return None;
    }
    let status = |s: &str| Some(json!({"org": org, "status": s}));
    if !o.members.contains_key(&u) || o.removed(&u) {
        return status("not_member");
    }
    if o.signin_requests.get(&u).is_some_and(|r| r.sub == sub) {
        return status("waiting");
    }
    let now = chrono::Utc::now().timestamp_micros();
    if o.signin_declined_since(&u, now) {
        return status("declined");
    }
    let actor = Actor {
        user: user.to_string(),
        superuser: false,
        via: via.clone(),
        owner: false,
        file_keys: Default::default(),
    };
    let change = Change::RequestSignIn { org: org.clone() };
    let store = state.store.read().unwrap().clone();
    match apply(&store, &actor, &change).await {
        Ok(events) => {
            if !events.is_empty() {
                eprintln!(
                    "signin request: {u} asked {org}'s admins to approve an unverified sign-in"
                );
                state.background(notify(Arc::clone(state), org.clone(), u.clone()));
            }
            status("waiting")
        }
        // Refused (too many waiting, or answered a moment ago on another server): nothing recorded.
        Err(e) => {
            eprintln!("signin request: {u} in {org}: not recorded: {e:#}");
            status("not_recorded")
        }
    }
}

/// Tell `org`'s admins that `user` signed in with an unverified password and waits for them. Runs after the
/// request is recorded; a failure is logged, and the request stays (the console's People page lists it).
pub(super) async fn notify(state: Arc<State>, org: String, user: String) {
    let store = state.store.read().unwrap().clone();
    let dir = match Directory::load(&store).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "signin request: {user} in {org}: the directory can't be read to tell its admins: {e:#}"
            );
            return;
        }
    };
    let Some(o) = dir.orgs.get(&org) else {
        return;
    };
    let Some(sub) = o.signin_requests.get(&user).map(|r| r.sub.clone()) else {
        return;
    };
    let role = match o.members.get(&user) {
        Some(Role::Admin) => "an admin",
        _ => "a member",
    };
    // Its admins with an email, less anyone disabled, and never the person asking.
    let admins: Vec<String> = o
        .members
        .iter()
        .filter(|(u, r)| **r == Role::Admin && u.contains('@') && **u != user)
        .map(|(u, _)| u.clone())
        .filter(|u| {
            !dir.disabled
                .get(&crate::auth::acl::disabled_id(u))
                .copied()
                .unwrap_or(false)
                && !state.closures.any_disabled(std::slice::from_ref(u))
        })
        .collect();
    let console = std::env::var("AMPUP_MEMORY_CONSOLE_URL")
        .ok()
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty());
    let page = console.as_ref().map(|c| format!("{c}/o/{org}/people"));
    let op = format!(
        "{{\"op\": \"approve_signin\", \"org\": \"{org}\", \"user\": \"{user}\", \"sub\": \"{sub}\"}} (or \"decline_signin\")"
    );
    if admins.is_empty() {
        eprintln!(
            "signin request: {user} in {org}: the org has no other admin with an email to tell: /admin {op}"
        );
        return;
    }
    let Some(mailer) = mail::from_env() else {
        eprintln!(
            "signin request: {user} in {org}: no mail is configured, so its admins ({}) weren't emailed",
            admins.join(", ")
        );
        return;
    };
    let subject = format!("Approve {user}'s sign-in to {org} on AmpUp Memory");
    let how_text = match &page {
        Some(p) => format!(
            "Approve or decline it on {org}'s People page, under \"Sign-ins to approve\":\n{p}"
        ),
        None => format!("Approve it as an admin of {org} with the /admin op {op}."),
    };
    let text = format!(
        "{user} signed in to {org} on AmpUp Memory with a password, but hasn't verified that email address. Anyone can make a password account under any address, so {org} lets this sign-in in only once one of its admins approves it.\n\nApprove only if you know this is {user}: that account ({sub}) then reads and writes {org}'s memory as {role} of {org}, as {user} is. Approving lets in that one account, in {org} only.\n\n{how_text}\n\nIf you don't recognise this, decline it: {user} stays out until they verify their email."
    );
    let e = mail::esc;
    let p_style = "margin:0 0 14px;font:15px/1.5 -apple-system,Segoe UI,Helvetica,Arial,sans-serif;color:#18181b";
    let mut body = format!(
        "<h2 style=\"margin:0 0 14px;font:600 20px/1.3 -apple-system,Segoe UI,Helvetica,Arial,sans-serif;color:#18181b\">Approve {}'s sign-in to {}?</h2><p style=\"{p_style}\">{} signed in with a password, but hasn't verified that email address. Anyone can make a password account under any address, so {} lets this sign-in in only once one of its admins approves it.</p><p style=\"{p_style}\">Approve only if you know this is {}: that account (<code>{}</code>) then reads and writes {}'s memory as {} of {}, as {} is. Approving lets in that one account, in {} only.</p>",
        e(&user),
        e(&org),
        e(&user),
        e(&org),
        e(&user),
        e(&sub),
        e(&org),
        e(role),
        e(&org),
        e(&user),
        e(&org),
    );
    match &page {
        Some(p) => body.push_str(&format!(
            "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"margin:0 0 14px\"><tr><td style=\"background:#18181b;border-radius:6px\"><a href=\"{}\" style=\"display:inline-block;padding:11px 20px;font:600 15px -apple-system,Segoe UI,Helvetica,Arial,sans-serif;color:#ffffff;text-decoration:none\">Review on the People page</a></td></tr></table>",
            e(p)
        )),
        None => body.push_str(&format!(
            "<p style=\"{p_style}\">Approve it as an admin of {} with the <code>/admin</code> op <code>{}</code>.</p>",
            e(&org),
            e(&op)
        )),
    }
    body.push_str(&format!(
        "<p style=\"{p_style}\">If you don't recognise this, decline it: {} stays out until they verify their email.</p>",
        e(&user)
    ));
    let html = format!(
        "<!doctype html><html><body style=\"margin:0;padding:24px;background:#ffffff\"><div style=\"max-width:640px\">{body}</div></body></html>"
    );
    for admin in admins {
        match mailer.send(&admin, &subject, &text, &html).await {
            Ok(()) => eprintln!(
                "signin request: {user} in {org}; told {admin} by {}",
                mailer.provider()
            ),
            Err(e) => eprintln!("signin request: {user} in {org}; mail to {admin} failed: {e:#}"),
        }
    }
}
