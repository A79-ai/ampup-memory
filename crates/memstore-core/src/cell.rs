//! Cells: orgs split across deployments sharing one catalog, so one org's memory is only ever loaded by its own
//! cell's servers. A [`Store`](crate::Store) is opened through [`CellCatalog`], which refuses every table and
//! namespace of an org that isn't this process's cell's ([`OtherCell`]), and the search indexes refuse the same
//! ([`check_namespace`]): whatever the call path, another cell's memory is never read, written or loaded here.
//!
//! - `AMPUP_MEMORY_CELL`: this process's cell (default `default`).
//! - `AMPUP_MEMORY_CELL_MAP`: JSON, org to cell (`{"acme": "acme"}`); an org not named is the default cell's.
//!
//! Without a map (one cell, the CLI, a laptop) every org is this cell's and nothing is refused. The system
//! namespace (the directory, the ledger, the audit) and a person's own scope (`memory_user_*`) are every cell's.

use std::collections::HashMap;
use std::sync::LazyLock;

use iceberg::table::Table;
use iceberg::{Catalog, NamespaceIdent, TableCommit, TableCreation, TableIdent};

use crate::{DEFAULT_ORG, SYSTEM_NS, Scope};

pub const DEFAULT: &str = "default";

struct Cells {
    me: String,
    map: HashMap<String, String>,
}

static CELLS: LazyLock<Cells> = LazyLock::new(|| {
    let me = std::env::var("AMPUP_MEMORY_CELL")
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| DEFAULT.into());
    let map = match std::env::var("AMPUP_MEMORY_CELL_MAP") {
        Ok(v) if !v.trim().is_empty() => serde_json::from_str(&v)
            .unwrap_or_else(|e| panic!("AMPUP_MEMORY_CELL_MAP is a JSON object of strings: {e}")),
        _ => HashMap::new(),
    };
    Cells { me, map }
});

pub fn me() -> &'static str {
    &CELLS.me
}

pub fn is_default() -> bool {
    me() == DEFAULT
}

/// The orgs mapped to a cell other than the default one.
pub fn map() -> &'static HashMap<String, String> {
    &CELLS.map
}

pub fn of_org(org: &str) -> &'static str {
    CELLS.map.get(org).map_or(DEFAULT, String::as_str)
}

/// Whether `org`'s memory is this cell's.
pub fn holds_org(org: &str) -> bool {
    of_org(org) == me()
}

/// Whether this cell may open the scope's memory. A person's own scope is every cell's.
pub fn holds(scope: &Scope) -> bool {
    scope.org_name().is_none_or(holds_org)
}

/// [`OtherCell`] for another cell's org: a step about to change or read that org's memory refuses here,
/// before anything changes, rather than finding none of it.
pub fn check_org(org: &str) -> Result<(), OtherCell> {
    match holds_org(org) {
        true => Ok(()),
        false => Err(OtherCell {
            org: org.into(),
            namespace: Scope::org(org).map_or_else(|_| org.into(), |s| s.namespace()),
        }),
    }
}

/// The org a namespace's memory is: None for the system namespace and a person's own (every cell's), and for a
/// namespace that holds no org's memory.
pub fn org_of_namespace(ns: &str) -> Option<&str> {
    if ns == SYSTEM_NS || ns.starts_with("memory_user_") {
        return None;
    }
    if ns == "memory_org" || ns.starts_with("memory_team_") {
        return Some(DEFAULT_ORG);
    }
    let rest = ns.strip_prefix("memory_org_")?;
    Some(rest.split_once("__team_").map_or(rest, |(org, _)| org))
}

/// A namespace of another cell's org, refused here: its memory is that cell's servers' alone.
#[derive(Debug)]
pub struct OtherCell {
    pub org: String,
    pub namespace: String,
}

impl std::fmt::Display for OtherCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is org {}'s, cell {}'s: never opened by cell {}",
            self.namespace,
            self.org,
            of_org(&self.org),
            me()
        )
    }
}

impl std::error::Error for OtherCell {}

/// [`OtherCell`] for a namespace that isn't this cell's.
pub fn check_namespace(ns: &str) -> Result<(), OtherCell> {
    match org_of_namespace(ns) {
        Some(org) if !holds_org(org) => Err(OtherCell {
            org: org.into(),
            namespace: ns.into(),
        }),
        _ => Ok(()),
    }
}

/// [`OtherCell`] for an index location under another cell's namespace (`<base>/<namespace>/...`).
pub fn check_uri(uri: &str) -> Result<(), OtherCell> {
    uri.split('/').try_for_each(check_namespace)
}

/// Whether `e` is (or was caused by) an [`OtherCell`] refusal.
pub fn is_other_cell(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<OtherCell>().is_some()
            || c.downcast_ref::<iceberg::Error>()
                .is_some_and(|ie| ie.message().starts_with(REFUSED))
    })
}

const REFUSED: &str = "memstore cell: ";

fn refuse(ns: &NamespaceIdent) -> iceberg::Result<()> {
    check_namespace(&ns.to_url_string()).map_err(|o| {
        iceberg::Error::new(
            iceberg::ErrorKind::PreconditionFailed,
            format!("{REFUSED}{o}"),
        )
    })
}

/// The catalog every [`Store`](crate::Store) reads and writes through: another cell's namespaces and tables
/// are refused, and listed by no one here.
#[derive(Debug)]
pub struct CellCatalog {
    inner: std::sync::Arc<dyn Catalog>,
}

impl CellCatalog {
    /// `inner`, guarded where this process has cells to keep apart (else `inner` itself).
    pub fn wrap(inner: std::sync::Arc<dyn Catalog>) -> std::sync::Arc<dyn Catalog> {
        match CELLS.map.is_empty() {
            true => inner,
            false => std::sync::Arc::new(CellCatalog { inner }),
        }
    }
}

#[async_trait::async_trait]
impl Catalog for CellCatalog {
    async fn list_namespaces(
        &self,
        parent: Option<&NamespaceIdent>,
    ) -> iceberg::Result<Vec<NamespaceIdent>> {
        let all = self.inner.list_namespaces(parent).await?;
        Ok(all
            .into_iter()
            .filter(|ns| check_namespace(&ns.to_url_string()).is_ok())
            .collect())
    }
    async fn create_namespace(
        &self,
        ns: &NamespaceIdent,
        props: HashMap<String, String>,
    ) -> iceberg::Result<iceberg::Namespace> {
        refuse(ns)?;
        self.inner.create_namespace(ns, props).await
    }
    async fn get_namespace(&self, ns: &NamespaceIdent) -> iceberg::Result<iceberg::Namespace> {
        refuse(ns)?;
        self.inner.get_namespace(ns).await
    }
    async fn namespace_exists(&self, ns: &NamespaceIdent) -> iceberg::Result<bool> {
        refuse(ns)?;
        self.inner.namespace_exists(ns).await
    }
    async fn update_namespace(
        &self,
        ns: &NamespaceIdent,
        props: HashMap<String, String>,
    ) -> iceberg::Result<()> {
        refuse(ns)?;
        self.inner.update_namespace(ns, props).await
    }
    async fn drop_namespace(&self, ns: &NamespaceIdent) -> iceberg::Result<()> {
        refuse(ns)?;
        self.inner.drop_namespace(ns).await
    }
    async fn list_tables(&self, ns: &NamespaceIdent) -> iceberg::Result<Vec<TableIdent>> {
        refuse(ns)?;
        self.inner.list_tables(ns).await
    }
    async fn create_table(
        &self,
        ns: &NamespaceIdent,
        creation: TableCreation,
    ) -> iceberg::Result<Table> {
        refuse(ns)?;
        self.inner.create_table(ns, creation).await
    }
    async fn load_table(&self, table: &TableIdent) -> iceberg::Result<Table> {
        refuse(table.namespace())?;
        self.inner.load_table(table).await
    }
    async fn drop_table(&self, table: &TableIdent) -> iceberg::Result<()> {
        refuse(table.namespace())?;
        self.inner.drop_table(table).await
    }
    async fn purge_table(&self, table: &TableIdent) -> iceberg::Result<()> {
        refuse(table.namespace())?;
        self.inner.purge_table(table).await
    }
    async fn table_exists(&self, table: &TableIdent) -> iceberg::Result<bool> {
        refuse(table.namespace())?;
        self.inner.table_exists(table).await
    }
    async fn rename_table(&self, src: &TableIdent, dest: &TableIdent) -> iceberg::Result<()> {
        refuse(src.namespace())?;
        refuse(dest.namespace())?;
        self.inner.rename_table(src, dest).await
    }
    async fn register_table(&self, table: &TableIdent, location: String) -> iceberg::Result<Table> {
        refuse(table.namespace())?;
        self.inner.register_table(table, location).await
    }
    async fn update_table(&self, commit: TableCommit) -> iceberg::Result<Table> {
        refuse(commit.identifier().namespace())?;
        self.inner.update_table(commit).await
    }
}
