//! Errors to Sentry, for an operator who wants them in their own Sentry project. Nothing is sent unless
//! `SENTRY_DSN` is set, which is never compiled in (a default install has none), and `SENTRY_ENVIRONMENT` isn't
//! `local`; the alerts match `staging` and `prod` exactly. Only the server and the deployment agent turn it on.

/// Starts Sentry when the environment asks for it; reporting stops when the guard drops (flushing what's queued).
pub fn init() -> Option<sentry::ClientInitGuard> {
    let (dsn, environment) = wanted(|k| std::env::var(k).ok())?;
    let dsn = match dsn.parse::<sentry::types::Dsn>() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SENTRY_DSN ignored: {e}");
            return None;
        }
    };
    let mut options = sentry::ClientOptions::default();
    options.dsn = Some(dsn);
    options.environment = Some(environment.into());
    options.release = sentry::release_name!();
    options.send_default_pii = false;
    options.before_send = Some(std::sync::Arc::new(|e| Some(trim(e))));
    Some(sentry::init(options))
}

/// The longest exception message sent: an error's chain can quote what a model or an upstream API answered, or the
/// text being written, and what identifies the failure comes first.
const MAX_MESSAGE: usize = 300;

/// Cuts each exception message to [`MAX_MESSAGE`] characters.
fn trim(mut e: sentry::protocol::Event<'static>) -> sentry::protocol::Event<'static> {
    for x in &mut e.exception.values {
        if let Some(v) = &mut x.value
            && let Some((i, _)) = v.char_indices().nth(MAX_MESSAGE)
        {
            v.truncate(i);
            v.push('…');
        }
    }
    e
}

/// The DSN and environment to report with, if any.
fn wanted(var: impl Fn(&str) -> Option<String>) -> Option<(String, String)> {
    let dsn = var("SENTRY_DSN").filter(|d| !d.is_empty())?;
    let environment = var("SENTRY_ENVIRONMENT").unwrap_or_else(|| "local".into());
    (environment != "local").then_some((dsn, environment))
}

/// A request that failed on the server's side (answered 500), already logged. `what` is the operation in fixed words,
/// never a person or a scope: it names and groups the issue. A no-op without Sentry.
pub fn failure(what: &'static str, e: &anyhow::Error) {
    sentry::with_scope(
        |s| {
            s.set_tag("op", what);
            s.set_fingerprint(Some(&["{{ default }}", what]));
        },
        || sentry_anyhow::capture_anyhow(e),
    );
}
