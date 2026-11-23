//! Where a slow request spent its time. A request runs inside [`timed`]; the code it passes through calls
//! [`mark`] at its steps (signed in, scope chosen, the session's row written), and the server's slow-request
//! line lists them as milliseconds since the request began. Outside a timed request a mark does nothing.

use std::cell::RefCell;
use std::time::Instant;

tokio::task_local! {
    static MARKS: RefCell<(Instant, Vec<(&'static str, u128)>)>;
}

/// Runs `f` with marks recorded, and returns its output with them as `step ms, step ms, …`.
pub async fn timed<F: std::future::Future>(f: F) -> (F::Output, String) {
    MARKS
        .scope(RefCell::new((Instant::now(), vec![])), async move {
            let out = f.await;
            let marks = MARKS.with(|m| {
                m.borrow()
                    .1
                    .iter()
                    .map(|(what, ms)| format!("{what} {ms}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            });
            (out, marks)
        })
        .await
}

/// Notes that the timed request (if any) reached `what`.
pub fn mark(what: &'static str) {
    let _ = MARKS.try_with(|m| {
        let mut m = m.borrow_mut();
        let ms = m.0.elapsed().as_millis();
        m.1.push((what, ms));
    });
}
