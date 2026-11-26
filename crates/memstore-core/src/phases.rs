//! Where a slow request spent its time. A request runs inside [`timed`]; the code it passes through calls
//! [`mark`] at its steps (signed in, scope chosen, the session's row written), and the server's slow-request
//! line lists them as milliseconds since the request began. Outside a timed request a mark does nothing.

use std::cell::RefCell;
use std::time::Instant;

tokio::task_local! {
    static MARKS: RefCell<(Instant, Vec<(&'static str, u128)>)>;
    static STUDIO_READ: RefCell<Option<StudioRead>>;
}

/// What [`timed`] saw of a request: its marks as `step ms, step ms, …`, and the Studio read it was, if it named
/// itself one ([`studio_read`]).
pub struct Timed {
    pub marks: String,
    pub studio_read: Option<StudioRead>,
}

/// One of the Studio's reads: which op, of which org's memory.
pub struct StudioRead {
    pub op: &'static str,
    pub org: String,
}

/// Runs `f` with marks recorded, and returns its output with what was seen of it.
pub async fn timed<F: std::future::Future>(f: F) -> (F::Output, Timed) {
    MARKS
        .scope(RefCell::new((Instant::now(), vec![])), async move {
            STUDIO_READ
                .scope(RefCell::new(None), async move {
                    let out = f.await;
                    let marks = MARKS.with(|m| {
                        m.borrow()
                            .1
                            .iter()
                            .map(|(what, ms)| format!("{what} {ms}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    });
                    let studio_read = STUDIO_READ.with(|r| r.borrow_mut().take());
                    (out, Timed { marks, studio_read })
                })
                .await
        })
        .await
}

/// Names the timed request (if any) as the Studio's read `op` of `org`'s memory, once it is signed in and its
/// scope chosen: the server times it apart (`memstore_studio_read_seconds`).
pub fn studio_read(op: &'static str, org: &str) {
    let _ = STUDIO_READ.try_with(|r| {
        *r.borrow_mut() = Some(StudioRead {
            op,
            org: org.to_string(),
        })
    });
}

/// Notes that the timed request (if any) reached `what`.
pub fn mark(what: &'static str) {
    crate::running::at(what);
    let _ = MARKS.try_with(|m| {
        let mut m = m.borrow_mut();
        let ms = m.0.elapsed().as_millis();
        m.1.push((what, ms));
    });
}

/// How long ago the timed request (if any) began: none outside one.
pub fn elapsed() -> Option<std::time::Duration> {
    MARKS.try_with(|m| m.borrow().0.elapsed()).ok()
}
