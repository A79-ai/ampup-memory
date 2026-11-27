//! Who read a scope's Studio Home lately, so a server that just started works their Home out before it says it is
//! ready, rather than in their first request after a deploy. No server keeps who read lately, and the answers kept
//! for them are keyed by a hash of who they are for, so the readers are written down here.
//!
//! - **What a reader is.** Who they are and how they signed in, as their request was answered (a person through an
//!   identity provider, a local server's one caller): never the groups they read as (worked out again when read), a
//!   token, a key or anything that signs someone in. A key's sign-in isn't written down at all. The server that reads
//!   it back answers nobody from it: a Home worked out for a reader is only ever answered to a request whose own
//!   sign-in is the same in every respect.
//! - **Where.** One small JSON file per scope, beside the scope's facts table (`<facts table>/readers/home.json`),
//!   overwritten whole, or deleted when no one is left in it: no catalog commit, and one read per scope when a server
//!   starts. The table is loaded afresh for each read and write, and a scope with no facts table is written nothing.
//!   Deleting the org, clearing it or purging a team deletes the table's files, this one with them; forgetting a
//!   person takes them out of it ([`Store::forget_recent_reader`]).
//! - **Their reads.** Beside each reader, the Studio reads they made lately that answer from a kept answer (a
//!   listing, their advice page, the lessons), as they asked them, so a new server works those out for them too:
//!   only in a few fixed shapes, never holding a name, a key or a path into memory.
//! - **Bounded.** Only readers seen in the last [`KEPT_FOR`], the newest [`MAX_READERS`] of them, are kept, each
//!   with their newest [`MAX_READS`] reads: each write drops the rest.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{FACTS, Scope, Store, now_micros};

/// How long a reader is kept after they last read: one who didn't read Home in a day isn't worked out for.
pub const KEPT_FOR: Duration = Duration::from_secs(24 * 3600);

/// How many readers a scope keeps, the most recently seen first.
pub const MAX_READERS: usize = 20;

/// How many of a reader's Studio reads are kept, the newest first.
pub const MAX_READS: usize = 24;

/// One reader, as the server that answered them wrote them down.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RecentReader {
    /// Who they are (their user id, as their requests run as).
    pub user: String,
    /// When they last read (µs since the epoch).
    pub seen: i64,
    /// How they read: what the server needs to read as they do (never a credential).
    pub reads_as: serde_json::Value,
    /// The Studio reads they made lately, as they asked them, the newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<serde_json::Value>,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    readers: Vec<RecentReader>,
}

/// `readers`, less those not seen in [`KEPT_FOR`], the newest [`MAX_READERS`] of them.
pub fn bounded(mut readers: Vec<RecentReader>) -> Vec<RecentReader> {
    let since = now_micros() - KEPT_FOR.as_micros() as i64;
    readers.retain(|r| r.seen >= since);
    readers.sort_by_key(|r| std::cmp::Reverse(r.seen));
    readers.truncate(MAX_READERS);
    for r in &mut readers {
        r.reads.truncate(MAX_READS);
    }
    readers
}

impl Store {
    /// The scope's facts table, for where its file is, loaded now (a handle read before a delete, a clear or a purge
    /// of the scope would name files those took away): none when the scope has no facts table any more.
    async fn readers_table(&self, ns: &str) -> Result<Option<iceberg::table::Table>> {
        if !self.has_facts_table(ns).await? {
            return Ok(None);
        }
        Ok(Some(self.table(ns, FACTS).await?))
    }

    fn readers_path(table: &iceberg::table::Table) -> String {
        format!(
            "{}/readers/home.json",
            table.metadata().location().trim_end_matches('/')
        )
    }

    /// The scope's facts table as this store last loaded it (within the hard staleness), else loaded now: for a read
    /// that may find an old file or none (a warm-up's), never for a write.
    async fn readers_table_held(&self, ns: &str) -> Result<Option<iceberg::table::Table>> {
        let held = self
            .cache
            .entries
            .lock()
            .unwrap()
            .get(&format!("{ns}.{FACTS}"))
            .filter(|e| e.loaded.elapsed() < self.hard_staleness)
            .map(|e| e.snap.table.clone());
        match held {
            Some(t) => Ok(Some(t)),
            None => self.readers_table(ns).await,
        }
    }

    /// The scope's recent Home readers, as last written ([`bounded`]): none when it has none, or no facts table.
    pub async fn recent_readers(&self, scope: &Scope) -> Result<Vec<RecentReader>> {
        let table = self.readers_table(&scope.namespace()).await?;
        Self::readers_in(table).await
    }

    /// [`Store::recent_readers`] from the facts table as this store holds it, when it holds it: what a server's warm-up
    /// reads, a catalog read fewer per scope. A handle from before a delete finds no file there: the delete took it.
    pub async fn recent_readers_held(&self, scope: &Scope) -> Result<Vec<RecentReader>> {
        let table = self.readers_table_held(&scope.namespace()).await?;
        Self::readers_in(table).await
    }

    async fn readers_in(table: Option<iceberg::table::Table>) -> Result<Vec<RecentReader>> {
        let Some(table) = table else {
            return Ok(vec![]);
        };
        let path = Self::readers_path(&table);
        let input = table.file_io().new_input(&path)?;
        if !input.exists().await? {
            return Ok(vec![]);
        }
        let bytes = input.read().await.with_context(|| format!("read {path}"))?;
        // A file that doesn't read (written by a newer server, or cut short) is no readers, not a failure.
        let file: File = serde_json::from_slice(&bytes).unwrap_or_default();
        Ok(bounded(file.readers))
    }

    /// Write the scope's recent Home readers ([`bounded`]) in place of those written before; with none, the file
    /// goes. Whether the scope is still there: one deleted, cleared or purged (no facts table) is written nothing.
    pub async fn put_recent_readers(
        &self,
        scope: &Scope,
        readers: Vec<RecentReader>,
    ) -> Result<bool> {
        let Some(table) = self.readers_table(&scope.namespace()).await? else {
            return Ok(false);
        };
        let path = Self::readers_path(&table);
        let readers = bounded(readers);
        if readers.is_empty() {
            if table.file_io().new_input(&path)?.exists().await? {
                table.file_io().delete(&path).await?;
            }
            return Ok(true);
        }
        table
            .file_io()
            .new_output(&path)?
            .write(serde_json::to_vec(&File { readers })?.into())
            .await
            .with_context(|| format!("write {path}"))?;
        Ok(true)
    }

    async fn has_facts_table(&self, ns: &str) -> Result<bool> {
        let ident = iceberg::TableIdent::new(iceberg::NamespaceIdent::new(ns.into()), FACTS.into());
        self.has_table(&ident).await
    }

    /// Take `user` out of the namespace's recent Home readers (forgetting them): how many entries went. The file
    /// goes when no one is left in it.
    pub(crate) async fn forget_recent_reader(&self, ns: &str, user: &str) -> Result<usize> {
        let Some(table) = self.readers_table(ns).await? else {
            return Ok(0);
        };
        let path = Self::readers_path(&table);
        let input = table.file_io().new_input(&path)?;
        if !input.exists().await? {
            return Ok(0);
        }
        let bytes = input.read().await.with_context(|| format!("read {path}"))?;
        let Ok(mut file) = serde_json::from_slice::<File>(&bytes) else {
            // One that doesn't read can't be told apart: it goes whole.
            table.file_io().delete(&path).await?;
            return Ok(1);
        };
        let before = file.readers.len();
        file.readers
            .retain(|r| !crate::people::directory::forgets(&r.user, user));
        let gone = before - file.readers.len();
        match (gone, file.readers.is_empty()) {
            (0, _) => {}
            (_, true) => table.file_io().delete(&path).await?,
            (_, false) => table
                .file_io()
                .new_output(&path)?
                .write(serde_json::to_vec(&file)?.into())
                .await
                .with_context(|| format!("write {path}"))?,
        }
        Ok(gone)
    }
}
