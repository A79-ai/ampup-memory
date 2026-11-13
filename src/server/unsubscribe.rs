//! The weekly digest's unsubscribe link (`crate::digest`): `GET /digest/unsubscribe?t=…` asks, and its button
//! (`POST`) unsubscribes the person the link was mailed to, or subscribes them again. A mail client's one-click
//! unsubscribe (RFC 8058: `POST` with `List-Unsubscribe=One-Click`) unsubscribes at once. The link is a token this
//! server sealed under its `AMPUP_MEMORY_SECRET_KEY`, naming the org and the person: it is the credential, since a
//! mail client has no bearer token, and it can unsubscribe no one else.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;

use super::State;
use super::join::{Shown, shown, token_in};
use crate::Scope;
use crate::digest;
use crate::mail::esc;

/// `GET` or `POST /digest/unsubscribe`. Boxed, as `join::route` is, to keep the caller's frame small.
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
        hyper::Method::GET => html(ask(&state, token_in(&query).as_deref()).await),
        hyper::Method::POST => {
            let form = http_body_util::Limited::new(req.into_body(), 4 << 10)
                .collect()
                .await
                .map(|b| String::from_utf8_lossy(&b.to_bytes()).into_owned())
                .unwrap_or_default();
            // One-click from the mail client carries the token in the link's query; the page's form in its body.
            let token = token_in(&form).or_else(|| token_in(&query));
            let on = form.split('&').any(|kv| kv == "subscribe=on");
            html(answer(&state, token.as_deref(), on).await)
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

const BAD_LINK: &str = "<p>This link doesn't work: it isn't one this server made.</p>";

/// The link's person and where their subscription is kept: their org's scope, or this server's own scope on a
/// server that identifies no one.
fn opened(state: &State, t: Option<&str>) -> Result<(digest::Unsubscribe, Scope), Shown> {
    let bad = || shown(400, "AmpUp Memory", BAD_LINK);
    let (Some(t), Some(key)) = (t, state.secret_key.as_ref()) else {
        return Err(bad());
    };
    let u = digest::open_link(key, t).map_err(|e| {
        eprintln!("digest: an unsubscribe link didn't open: {e:#}");
        bad()
    })?;
    let scope = match &u.org {
        Some(o) => Scope::org(o).map_err(|_| bad())?,
        None if state.fixed() => state.scope.clone(),
        None => return Err(bad()),
    };
    Ok((u, scope))
}

fn form(t: &str, subscribe: bool, label: &str) -> String {
    format!(
        "<form method=\"post\"><input type=\"hidden\" name=\"t\" value=\"{}\">{}<button type=\"submit\">{}</button></form>",
        esc(t),
        if subscribe {
            "<input type=\"hidden\" name=\"subscribe\" value=\"on\">"
        } else {
            ""
        },
        esc(label)
    )
}

/// What the link would do, with the button that does it. Nothing changes on a `GET` (mail scanners open links).
async fn ask(state: &State, t: Option<&str>) -> Shown {
    let (u, scope) = match opened(state, t) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let t = t.unwrap_or_default();
    let store = state.store.read().unwrap().clone();
    let off = crate::qa_log::list(&store, &scope, None)
        .await
        .map(|log| digest::unsubscribed(&log, &u.user))
        .unwrap_or(false);
    let whose = match &u.org {
        Some(o) => format!(" from <b>{}</b>", esc(o)),
        None => String::new(),
    };
    if off {
        return shown(
            200,
            "Weekly digest",
            &format!(
                "<p><b>{}</b> is unsubscribed from the AmpUp Memory weekly digest{whose}.</p>{}",
                esc(&u.user),
                form(t, true, "Subscribe again")
            ),
        );
    }
    shown(
        200,
        "Weekly digest",
        &format!(
            "<p>Stop the AmpUp Memory weekly digest{whose} for <b>{}</b>?</p>{}",
            esc(&u.user),
            form(t, false, "Unsubscribe")
        ),
    )
}

/// Unsubscribes (or, with `on`, subscribes again) the link's person.
async fn answer(state: &State, t: Option<&str>, on: bool) -> Shown {
    let (u, scope) = match opened(state, t) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let store = state.store.read().unwrap().clone();
    match digest::subscribe(&store, &scope, &u.user, on).await {
        Ok(()) => {
            eprintln!(
                "digest {scope}: {} {} by link",
                u.user,
                if on {
                    "subscribed again"
                } else {
                    "unsubscribed"
                }
            );
            let t = t.unwrap_or_default();
            if on {
                shown(
                    200,
                    "Subscribed",
                    &format!(
                        "<p>Done: <b>{}</b> gets the weekly digest again.</p>{}",
                        esc(&u.user),
                        form(t, false, "Unsubscribe")
                    ),
                )
            } else {
                shown(
                    200,
                    "Unsubscribed",
                    &format!(
                        "<p>Done: <b>{}</b> gets no more weekly digests.</p>{}",
                        esc(&u.user),
                        form(t, true, "Subscribe again")
                    ),
                )
            }
        }
        Err(e) => {
            eprintln!(
                "digest {scope}: {}'s subscription not changed: {e:#}",
                u.user
            );
            shown(
                503,
                "AmpUp Memory",
                "<p>That didn't go through. Try the link again in a minute.</p>",
            )
        }
    }
}
