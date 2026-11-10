//! Pinned memory versions for a long-running reader (an agent that reads the same memory over hours): it pins the
//! ledger state of one scope's branch as it is now, and reads it, through any read path, with the pin's id where a
//! workspace snapshot's would go (`x-memstore-at: pin_…` or `?at=pin_…`), in any org, not only a workspace. A pin
//! holds what a [`crate::Session`] pinned to heads holds: the commits reachable from them count, nothing committed
//! later, and its reads are as of when it was taken.
//!
//! - **What it doesn't hold.** Who may read what stays main's current word, as for every pinned read: a delete at
//!   the source, a narrowed ACL, an episode past retention, a forgotten person's memory are gone through a pin as
//!   everywhere (`Store::visibility_at`). So a forget, `org purge` and the nightly window's scheduled purge delete
//!   nothing a pin still reads: a pin never keeps deleted memory readable.
//! - **What cleanup keeps for it.** Every cleanup keeps what a live pin reads: a compaction keeps every row; the
//!   summaries' compaction keeps each pin's newest run of every subject, as it does a branch's and a workspace
//!   snapshot's ([`crate::Store::compact_aggregates`]); snapshot expiry touches Iceberg snapshots, which no pin
//!   reads (a pin holds ledger commits). Pins don't hold index versions: a pinned read searches the current indexes,
//!   whose rows are filtered to its commits.
//! - **A lease.** A pin lives `ttl` (`AMPUP_MEMORY_PIN_TTL`, default an hour; a request may ask for up to
//!   `AMPUP_MEMORY_PIN_MAX_TTL`, a day) and is renewed by whoever took it while they still read it, never past
//!   `AMPUP_MEMORY_PIN_MAX_AGE` (7 days) after it was taken. A pin released or past its lease is dead: reads through it
//!   are refused, and nothing is kept for it any more. So an agent that died blocks no cleanup past its lease.
//!
//! Stored per scope, in `<namespace>.pins` (one row per change: taken, renewed, released; the newest row of a pin
//! is its state), not through the ledger: a pin is no memory. Deleting the org deletes its pins with it.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arrow_array::{ArrayRef, RecordBatch, StringArray};
use iceberg::spec::PrimitiveType;
use serde::{Deserialize, Serialize};

use crate::{Scope, Store, now_micros, strings, timestamps};

/// The table of a scope's pins.
pub const PINS: &str = "pins";
/// A pin's id starts with this, so a read's `at` tells a pin from a workspace snapshot.
pub const PREFIX: &str = "pin_";

/// One pin, as its newest row says.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub id: String,
    /// The scope it pins (`org:acme`): it is read there and nowhere else.
    pub scope: String,
    pub branch: String,
    /// The branch's ledger heads when it was taken: the commits reachable from them are what it reads.
    pub heads: Vec<String>,
    /// When it was taken (µs since the epoch): its reads are as of then.
    pub at: i64,
    /// Who took it (the caller's id): they renew and release it.
    pub by: String,
    /// The end of its lease (µs): past it, the pin is dead.
    pub expires_at: i64,
    /// No renewal reaches past this (µs).
    pub until: i64,
    #[serde(default)]
    pub released: bool,
}

impl Pin {
    /// Whether it still holds: not released, and its lease not over.
    pub fn live(&self, now: i64) -> bool {
        !self.released && now < self.expires_at
    }
}

/// How long pins live: `AMPUP_MEMORY_PIN_TTL`, `AMPUP_MEMORY_PIN_MAX_TTL`, `AMPUP_MEMORY_PIN_MAX_AGE` (seconds).
#[derive(Clone, Copy, Debug)]
pub struct Lease {
    pub ttl: Duration,
    pub max_ttl: Duration,
    pub max_age: Duration,
}

impl Lease {
    pub fn from_env() -> Self {
        let secs = |k: &str, d: u64| {
            Duration::from_secs(
                std::env::var(k)
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .filter(|n| *n > 0)
                    .unwrap_or(d),
            )
        };
        let max_age = secs("AMPUP_MEMORY_PIN_MAX_AGE", 7 * 86_400);
        let max_ttl = secs("AMPUP_MEMORY_PIN_MAX_TTL", 86_400).min(max_age);
        Lease {
            ttl: secs("AMPUP_MEMORY_PIN_TTL", 3_600).min(max_ttl),
            max_ttl,
            max_age,
        }
    }

    /// The lease asked for (seconds; none: the default), within what is allowed.
    fn ttl_of(&self, asked: Option<u64>) -> Duration {
        asked
            .filter(|s| *s > 0)
            .map_or(self.ttl, Duration::from_secs)
            .min(self.max_ttl)
    }
}

/// A pin refused, and why. Its message is the caller's.
#[derive(Debug)]
pub struct Refused(pub Why, pub String);

/// Why a pin was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// No such pin (or branch) in this scope.
    Unknown,
    /// Released, or its lease is over.
    Dead,
    /// Someone else's to renew or release.
    NotYours,
    /// Asked for another branch than it holds, or to pin what isn't a branch as it is now.
    Bad,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for Refused {}

fn pins_schema() -> Result<iceberg::spec::Schema> {
    crate::schema(&[
        ("pin_id", true, PrimitiveType::String),
        ("body", true, PrimitiveType::String),
        ("recorded_at", true, PrimitiveType::Timestamptz),
    ])
}

/// Each pin as its newest row says.
fn fold(rows: &[RecordBatch]) -> Vec<Pin> {
    let mut newest: std::collections::HashMap<String, (i64, Pin)> = Default::default();
    for b in rows {
        let (id, body, at) = (
            strings(b, "pin_id"),
            strings(b, "body"),
            timestamps(b, "recorded_at"),
        );
        for i in 0..b.num_rows() {
            let Ok(pin) = serde_json::from_str::<Pin>(&body[i]) else {
                continue;
            };
            let t = at[i].unwrap_or_default();
            if newest.get(&id[i]).is_none_or(|(seen, _)| t >= *seen) {
                newest.insert(id[i].clone(), (t, pin));
            }
        }
    }
    let mut out: Vec<Pin> = newest.into_values().map(|(_, p)| p).collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

impl Store {
    /// Every pin of `scope`, live or not, as its newest row says (from the cached table: a pin taken by this
    /// server is read at once).
    pub async fn pins(&self, scope: &Scope) -> Result<Vec<Pin>> {
        Ok(fold(&self.side_rows(&scope.namespace(), PINS).await?))
    }

    /// The pins of `scope` alive at `now`.
    pub async fn live_pins(&self, scope: &Scope, now: i64) -> Result<Vec<Pin>> {
        let mut pins = self.pins(scope).await?;
        pins.retain(|p| p.live(now));
        Ok(pins)
    }

    /// Pin `branch` of `scope` as it is now, for `by`, for `ttl_secs` (none: [`Lease::ttl`]).
    pub async fn take_pin(
        &self,
        scope: &Scope,
        branch: &str,
        by: &str,
        ttl_secs: Option<u64>,
    ) -> Result<Pin> {
        let lease = Lease::from_env();
        // The heads as the catalog has them now, not as this server's cache last read them (up to
        // `AMPUP_MEMORY_MAX_STALENESS_MS` behind other writers, longer while refreshes fail): a pin is read for hours,
        // and must hold every commit made before it was asked for, by any writer.
        let heads = self
            .branch_heads(scope)
            .await?
            .remove(branch)
            .unwrap_or_default();
        if branch != crate::MAIN && heads.is_empty() {
            return Err(Refused(Why::Unknown, format!("{scope} has no branch {branch}")).into());
        }
        // And the rows of those commits: the first read through the pin reads the scope's tables again, rather than
        // a cached copy older than its heads.
        let ns = scope.namespace();
        for name in [
            crate::EPISODES,
            crate::FACTS,
            crate::QUOTES,
            crate::CLOSURES,
            crate::SCHEMAS,
            crate::RETRACTIONS,
            crate::RETENTION,
            crate::AGGREGATES,
        ] {
            self.invalidate(&ns, name);
        }
        let now = now_micros();
        let pin = Pin {
            id: format!("{PREFIX}{}", uuid::Uuid::new_v4().simple()),
            scope: scope.to_string(),
            branch: branch.to_string(),
            heads,
            at: now,
            by: by.to_string(),
            expires_at: now + lease.ttl_of(ttl_secs).as_micros() as i64,
            until: now + lease.max_age.as_micros() as i64,
            released: false,
        };
        self.write_pin(scope, &pin).await?;
        Ok(pin)
    }

    /// A live pin of `scope`; refused ([`Refused`]) when it has none of that id, or it was released or its lease
    /// is over.
    pub async fn find_pin(&self, scope: &Scope, id: &str) -> Result<Pin> {
        let pin = self
            .pins(scope)
            .await?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| Refused(Why::Unknown, format!("{scope} has no pin {id}")))?;
        let now = now_micros();
        if pin.released {
            return Err(
                Refused(Why::Dead, format!("pin {id} was released: take a new one")).into(),
            );
        }
        if !pin.live(now) {
            return Err(Refused(
                Why::Dead,
                format!(
                    "pin {id}'s lease ended at {}: take a new one",
                    crate::audit::rfc3339(pin.expires_at)
                ),
            )
            .into());
        }
        Ok(pin)
    }

    /// Extend a live pin's lease to `ttl_secs` from now (none: [`Lease::ttl`]), never past its `until`; only who
    /// took it may (`may`: whether the caller may change it).
    pub async fn renew_pin(
        &self,
        scope: &Scope,
        id: &str,
        may: &(dyn Fn(&Pin) -> bool + Sync),
        ttl_secs: Option<u64>,
    ) -> Result<Pin> {
        let mut pin = self.find_pin(scope, id).await?;
        if !may(&pin) {
            return Err(
                Refused(Why::NotYours, format!("pin {id} is {}'s to renew", pin.by)).into(),
            );
        }
        let lease = Lease::from_env();
        let now = now_micros();
        pin.expires_at = (now + lease.ttl_of(ttl_secs).as_micros() as i64).min(pin.until);
        self.write_pin(scope, &pin).await?;
        Ok(pin)
    }

    /// Release a pin: nothing is kept for it any more, and reads through it are refused.
    pub async fn release_pin(
        &self,
        scope: &Scope,
        id: &str,
        may: &(dyn Fn(&Pin) -> bool + Sync),
    ) -> Result<Pin> {
        let mut pin = self.find_pin(scope, id).await?;
        if !may(&pin) {
            return Err(Refused(
                Why::NotYours,
                format!("pin {id} is {}'s to release", pin.by),
            )
            .into());
        }
        pin.released = true;
        self.write_pin(scope, &pin).await?;
        Ok(pin)
    }

    async fn write_pin(&self, scope: &Scope, pin: &Pin) -> Result<()> {
        let ns = scope.namespace();
        self.ensure_table(&ns, PINS, pins_schema()?).await?;
        let table = self.write_target(&ns, PINS).await?;
        let s = |v: &str| -> ArrayRef { Arc::new(StringArray::from(vec![v])) };
        crate::append(
            &*self.catalog(),
            &table,
            vec![
                ("pin_id", s(&pin.id)),
                ("body", s(&serde_json::to_string(pin)?)),
                ("recorded_at", crate::ts([Some(now_micros())])),
            ],
            crate::OnConflict::Recommit,
        )
        .await
        .with_context(|| format!("record pin {}", pin.id))?;
        self.forget_missing(&ns, PINS);
        self.invalidate(&ns, PINS);
        Ok(())
    }
}
