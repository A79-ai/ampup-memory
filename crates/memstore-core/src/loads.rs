//! Catalog table loads, counted by who asked (a request, or a server's background loop), by table and by
//! namespace, and said once a minute ([`report`]). A hosted catalog meters these reads per project (BigLake's
//! Iceberg REST catalog: 3,000 a minute), and every server sharing the project spends the same quota, so a
//! server's log says what it spent and on what.
//!
//! A caller names itself with [`by`]; what it starts in the background (a cached table's refresh) keeps its
//! name ([`later`]). A load nobody named counts as `other`.
//!
//! When the catalog says its quota is spent (429), every server loop's load here waits a few seconds before it
//! asks ([`throttled`]): a request's loads go on, so what someone is waiting for gets the quota that is left,
//! rather than every loop asking again at once and spending the next minute's.
//!
//! Loops' table reads can also be held to a rate, `AMPUP_MEMORY_BACKGROUND_CATALOG_READS_PER_MIN` a minute per
//! process (default 0, no limit): where several servers share one quota, a rolling deploy's new servers warming
//! every recent scope at once, beside the old ones, otherwise spend it before anything says 429. A loop takes its
//! turn ([`turn`]) before it takes a table's load lock, never while holding one, so a request that needs the
//! table never queues behind the wait. Requests, and what a starting server needs before it takes sign-ins
//! (caller `start`), never wait for it.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use iceberg::table::Table;
use iceberg::{Catalog, NamespaceIdent, TableCommit, TableCreation, TableIdent};

tokio::task_local! {
    static CALLER: &'static str;
    static LATER: bool;
}

/// `fut`, its catalog loads counted as `caller`'s.
pub async fn by<F: Future>(caller: &'static str, fut: F) -> F::Output {
    CALLER.scope(caller, fut).await
}

/// `fut`, its catalog loads counted as [`caller`]'s (read now, before it is spawned) but made in the background:
/// a cached table's refresh no one waits for, which waits out a pause ([`throttled`]) even when a request
/// started it.
pub fn later<F: Future>(fut: F) -> impl Future<Output = F::Output> {
    CALLER.scope(caller(), LATER.scope(true, fut))
}

/// Who this task's loads count for.
pub fn caller() -> &'static str {
    CALLER.try_with(|c| *c).unwrap_or("other")
}

/// Until when a loop's loads wait, after the catalog last said its quota is spent.
static PAUSED: Mutex<Option<Instant>> = Mutex::new(None);

/// Whether this caller is a request, waiting on its loads: they never wait (what a request left to the background
/// does, [`later`]).
fn is_request() -> bool {
    caller() == "request" && !LATER.try_with(|l| *l).unwrap_or(false)
}

/// A loop's load waits out the pause the last throttled load started.
async fn wait_turn() {
    if is_request() {
        return;
    }
    let until = *PAUSED.lock().unwrap();
    if let Some(t) = until.filter(|t| *t > Instant::now()) {
        tokio::time::sleep_until(t.into()).await;
    }
}

/// The background loops whose work steps aside for interactive reads ([`crate::priority`]): what no one waits for,
/// and that holds nothing a read waits on between its steps.
const LOOPS: [&str; 5] = [
    "keep_warm",
    "dream",
    "maintenance",
    "maintenance_window",
    "directory_compaction",
];

/// Whether this task is one of the background [`LOOPS`] (its name kept by what it spawned through [`later`]).
pub fn is_loop() -> bool {
    LOOPS.contains(&caller())
}

/// A loop's turn to read a table, within the rate: waited for (and out of any pause) before the table's load lock
/// is taken. A request, or a starting server's `start` reads, go at once. A background loop's load also holds a
/// permit of [`crate::priority::load_permit`]'s pool while it loads: the turn returned, dropped once the load is
/// done.
#[must_use]
pub async fn turn() -> Option<tokio::sync::SemaphorePermit<'static>> {
    if is_request() || caller() == "start" {
        return None;
    }
    wait_turn().await;
    loop {
        let wait = RATE.lock().unwrap().take(Instant::now());
        match wait {
            Some(w) => tokio::time::sleep(w).await,
            None => break,
        }
    }
    crate::priority::load_permit().await
}

/// Loops' loads a second ([`RATE`]), as a token bucket holding at most [`BURST_SECS`] of them.
struct Rate {
    per_sec: f64,
    tokens: f64,
    at: Instant,
}

/// The most loops may load at once after an idle while, in seconds of their rate.
const BURST_SECS: f64 = 5.0;

impl Rate {
    fn refill(&mut self, now: Instant) {
        let cap = (self.per_sec * BURST_SECS).max(1.0);
        self.tokens = (self.tokens
            + now.saturating_duration_since(self.at).as_secs_f64() * self.per_sec)
            .min(cap);
        self.at = now;
    }

    /// Take a turn: None when taken (or there is no limit), else how long until one is free.
    fn take(&mut self, now: Instant) -> Option<Duration> {
        if self.per_sec <= 0.0 {
            return None;
        }
        self.refill(now);
        if self.tokens < 1.0 {
            return Some(Duration::from_secs_f64((1.0 - self.tokens) / self.per_sec));
        }
        self.tokens -= 1.0;
        None
    }
}

static RATE: LazyLock<Mutex<Rate>> = LazyLock::new(|| {
    let per_min = std::env::var("AMPUP_MEMORY_BACKGROUND_CATALOG_READS_PER_MIN")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0)
        .max(0.0);
    let per_sec = per_min / 60.0;
    if per_sec > 0.0 {
        eprintln!("memstore: background catalog loads held to {per_min} a minute");
    }
    Mutex::new(Rate {
        per_sec,
        tokens: (per_sec * BURST_SECS).max(1.0),
        at: Instant::now(),
    })
});

/// The catalog said its quota is spent: loops' loads wait what it asked (`Retry-After`), or 5 s, give or take a
/// second, so servers sharing the quota don't all ask again at the same instant.
fn throttled<T>(r: &iceberg::Result<T>) {
    let Err(e) = r else {
        return;
    };
    let e = anyhow::anyhow!("{e:?}");
    if !crate::retry::is_throttled(&e) {
        return;
    }
    let jitter = Duration::from_millis(uuid::Uuid::new_v4().as_u128() as u64 % 2000);
    let until = Instant::now() + crate::retry::wait_hint(&e) + jitter - Duration::from_secs(1);
    let mut paused = PAUSED.lock().unwrap();
    if paused.is_none_or(|t| t < until) {
        if paused.is_none_or(|t| t < Instant::now()) {
            eprintln!(
                "memstore: the catalog's read quota is spent: background loads wait {} ms",
                until.saturating_duration_since(Instant::now()).as_millis()
            );
        }
        *paused = Some(until);
    }
}

/// Who loaded which table of which namespace: (caller, table, namespace).
type Key = (&'static str, String, String);

/// Loads since the last report, by [`Key`].
static COUNTS: LazyLock<Mutex<HashMap<Key, u64>>> = LazyLock::new(Default::default);

fn count(table: &TableIdent) {
    let ns = table.namespace().to_url_string();
    *COUNTS
        .lock()
        .unwrap()
        .entry((caller(), table.name().to_string(), ns))
        .or_default() += 1;
}

/// The loads since the last report, as one line (none when there were none): the total, then the most by caller,
/// by table and by namespace. Counting starts again.
pub fn report() -> Option<String> {
    let counts = std::mem::take(&mut *COUNTS.lock().unwrap());
    let total: u64 = counts.values().sum();
    if total == 0 {
        return None;
    }
    let top = |key: &dyn Fn(&Key) -> String, n: usize| -> String {
        let mut by: HashMap<String, u64> = HashMap::new();
        for (k, v) in &counts {
            *by.entry(key(k)).or_default() += v;
        }
        let mut by: Vec<(String, u64)> = by.into_iter().collect();
        by.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let more = by.len().saturating_sub(n);
        let mut s = by
            .iter()
            .take(n)
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        if more > 0 {
            s += &format!(" and {more} more");
        }
        s
    };
    Some(format!(
        "{total} table loads (by caller: {}; by table: {}; by namespace: {})",
        top(&|k| k.0.to_string(), 12),
        top(&|k| k.1.clone(), 8),
        top(&|k| k.2.clone(), 8),
    ))
}

/// The catalog a [`Store`](crate::Store) reads through: each table load (and existence check, a load too to a
/// REST catalog) counted ([`report`]).
#[derive(Debug)]
pub struct Counted {
    inner: Arc<dyn Catalog>,
}

impl Counted {
    pub fn wrap(inner: Arc<dyn Catalog>) -> Arc<dyn Catalog> {
        Arc::new(Counted { inner })
    }
}

#[async_trait::async_trait]
impl Catalog for Counted {
    async fn list_namespaces(
        &self,
        parent: Option<&NamespaceIdent>,
    ) -> iceberg::Result<Vec<NamespaceIdent>> {
        self.inner.list_namespaces(parent).await
    }
    async fn create_namespace(
        &self,
        ns: &NamespaceIdent,
        props: HashMap<String, String>,
    ) -> iceberg::Result<iceberg::Namespace> {
        self.inner.create_namespace(ns, props).await
    }
    async fn get_namespace(&self, ns: &NamespaceIdent) -> iceberg::Result<iceberg::Namespace> {
        self.inner.get_namespace(ns).await
    }
    async fn namespace_exists(&self, ns: &NamespaceIdent) -> iceberg::Result<bool> {
        self.inner.namespace_exists(ns).await
    }
    async fn update_namespace(
        &self,
        ns: &NamespaceIdent,
        props: HashMap<String, String>,
    ) -> iceberg::Result<()> {
        self.inner.update_namespace(ns, props).await
    }
    async fn drop_namespace(&self, ns: &NamespaceIdent) -> iceberg::Result<()> {
        self.inner.drop_namespace(ns).await
    }
    async fn list_tables(&self, ns: &NamespaceIdent) -> iceberg::Result<Vec<TableIdent>> {
        self.inner.list_tables(ns).await
    }
    async fn create_table(
        &self,
        ns: &NamespaceIdent,
        creation: TableCreation,
    ) -> iceberg::Result<Table> {
        self.inner.create_table(ns, creation).await
    }
    async fn load_table(&self, table: &TableIdent) -> iceberg::Result<Table> {
        wait_turn().await;
        count(table);
        let r = self.inner.load_table(table).await;
        throttled(&r);
        r
    }
    async fn drop_table(&self, table: &TableIdent) -> iceberg::Result<()> {
        self.inner.drop_table(table).await
    }
    async fn purge_table(&self, table: &TableIdent) -> iceberg::Result<()> {
        self.inner.purge_table(table).await
    }
    async fn table_exists(&self, table: &TableIdent) -> iceberg::Result<bool> {
        wait_turn().await;
        count(table);
        let r = self.inner.table_exists(table).await;
        throttled(&r);
        r
    }
    async fn rename_table(&self, src: &TableIdent, dest: &TableIdent) -> iceberg::Result<()> {
        self.inner.rename_table(src, dest).await
    }
    async fn register_table(&self, table: &TableIdent, location: String) -> iceberg::Result<Table> {
        self.inner.register_table(table, location).await
    }
    async fn update_table(&self, commit: TableCommit) -> iceberg::Result<Table> {
        self.inner.update_table(commit).await
    }
}
