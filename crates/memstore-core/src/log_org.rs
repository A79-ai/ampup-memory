//! The org a log line is about. A server's pod serves many orgs (its cell's), so each line written while
//! a request or a background round works for one org starts `org=<org> ` (the crates' `eprintln!`), and a
//! pod's lines filter by org. Only a valid org name ([`crate::check_name`]) is ever written.

use std::cell::RefCell;
use std::future::Future;

use tokio::task::futures::TaskLocalFuture;

tokio::task_local! {
    static ORG: RefCell<Option<String>>;
}

/// `fut`, its lines naming `org` (until [`set`] names another). `org` is read now, not held.
pub fn with<F: Future>(org: Option<&str>, fut: F) -> TaskLocalFuture<RefCell<Option<String>>, F> {
    ORG.scope(RefCell::new(valid(org)), fut)
}

/// From here on in this task (one run by [`with`]), lines name `org`: a request's, once its scope is known.
pub fn set(org: Option<&str>) {
    let _ = ORG.try_with(|o| *o.borrow_mut() = valid(org));
}

/// The org this task's lines name now.
pub fn current() -> Option<String> {
    ORG.try_with(|o| o.borrow().clone()).ok().flatten()
}

fn valid(org: Option<&str>) -> Option<String> {
    org.filter(|o| crate::check_name("org", o).is_ok())
        .map(str::to_string)
}
