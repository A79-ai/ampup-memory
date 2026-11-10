//! Platform adapters (spec seams A-1..A-3). Every platform speaks the Iceberg REST catalog
//! protocol, so an adapter is only the catalog address, auth and storage props.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures::stream::BoxStream;
use iceberg::io::{
    FileMetadata, FileRead, FileWrite, InputFile, OutputFile, Storage, StorageConfig,
    StorageFactory,
};
use iceberg::memory::{MEMORY_CATALOG_WAREHOUSE, MemoryCatalogBuilder};
use iceberg::{Catalog, CatalogBuilder};
use iceberg_catalog_rest::RestCatalogBuilder;
use iceberg_storage_opendal::{OpenDalResolvingStorageFactory, OpenDalStorageFactory};
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, SemaphorePermit};

/// Build the catalog named by `MEMSTORE_PLATFORM` (gcp | snowflake | databricks | rest | local).
const SPCS_TOKEN: &str = "/snowflake/session/token";

/// `SNOWFLAKE_PAT`, or the file `SNOWFLAKE_PAT_FILE` names (SPCS mounts secret references as files).
pub fn snowflake_pat() -> Option<String> {
    secret("SNOWFLAKE_PAT")
}

/// `<VAR>`, or the contents of the file `<VAR>_FILE` names (mounted secrets), trimmed; None if unset or empty.
fn secret(var: &str) -> Option<String> {
    let v = match std::env::var(format!("{var}_FILE")) {
        Ok(path) => std::fs::read_to_string(path).ok()?,
        Err(_) => std::env::var(var).ok()?,
    };
    Some(v.trim().to_string()).filter(|v| !v.is_empty())
}

pub async fn catalog_from_env() -> Result<Arc<dyn Catalog>> {
    if env("MEMSTORE_PLATFORM")? == "local" {
        return Ok(open_from_env().await?.0);
    }
    let (c, lasts) = open_from_env().await?;
    Ok(Arc::new(Fresh {
        held: tokio::sync::Mutex::new((c, reopen_at(lasts))),
    }))
}

/// How long an opened catalog is used before it is opened again: its credentials (the REST catalog's OAuth
/// token, vended storage credentials, a gcloud token) last about an hour.
const FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(45 * 60);

/// How long before its token runs out a catalog is opened again, when the token says how long it lasts: past the
/// minute a cached table handle may still be used after it (`MEMSTORE_HARD_STALENESS_MS`).
const BEFORE_EXPIRY: std::time::Duration = std::time::Duration::from_secs(120);

/// When a catalog opened now is opened again: after [`fresh_for`], or two minutes before its token runs out when the
/// token says how long it has left. A metadata server hands out its cached token with whatever life it has left
/// (minutes, at times), so an hour can't be assumed from the moment of opening.
fn reopen_at(lasts: Option<std::time::Duration>) -> std::time::Instant {
    std::time::Instant::now() + reopen_after(fresh_for(), lasts)
}

fn reopen_after(
    fresh: std::time::Duration,
    lasts: Option<std::time::Duration>,
) -> std::time::Duration {
    match lasts {
        Some(t) => fresh.min(t.saturating_sub(BEFORE_EXPIRY)),
        None => fresh,
    }
}

/// The environment's catalog, opened again before its token runs out ([`reopen_at`]), so a command that runs
/// longer than its tokens last still commits (a run on OpenRouter's Batch API waits for its batch, often over an
/// hour). `serve` replaces its whole store on a timer as well.
#[derive(Debug)]
struct Fresh {
    /// The catalog, and when it is opened again.
    held: tokio::sync::Mutex<(Arc<dyn Catalog>, std::time::Instant)>,
}

impl Fresh {
    async fn get(&self) -> iceberg::Result<Arc<dyn Catalog>> {
        let mut held = self.held.lock().await;
        if std::time::Instant::now() >= held.1 {
            let (c, lasts) = open_from_env().await.map_err(|e| {
                iceberg::Error::new(
                    iceberg::ErrorKind::Unexpected,
                    format!("opening the catalog again for fresh credentials: {e:#}"),
                )
            })?;
            eprintln!(
                "memstore: catalog opened again for fresh credentials (its token has {}s left)",
                lasts.map_or("unknown".into(), |t| t.as_secs().to_string())
            );
            *held = (c, reopen_at(lasts));
        }
        Ok(held.0.clone())
    }
}

/// [`FRESH_FOR`], or `MEMSTORE_CATALOG_FRESH_SECS`.
fn fresh_for() -> std::time::Duration {
    std::env::var("MEMSTORE_CATALOG_FRESH_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(FRESH_FOR, std::time::Duration::from_secs)
}

#[async_trait::async_trait]
impl Catalog for Fresh {
    async fn list_namespaces(
        &self,
        parent: Option<&iceberg::NamespaceIdent>,
    ) -> iceberg::Result<Vec<iceberg::NamespaceIdent>> {
        self.get().await?.list_namespaces(parent).await
    }
    async fn create_namespace(
        &self,
        ns: &iceberg::NamespaceIdent,
        props: HashMap<String, String>,
    ) -> iceberg::Result<iceberg::Namespace> {
        self.get().await?.create_namespace(ns, props).await
    }
    async fn get_namespace(
        &self,
        ns: &iceberg::NamespaceIdent,
    ) -> iceberg::Result<iceberg::Namespace> {
        self.get().await?.get_namespace(ns).await
    }
    async fn namespace_exists(&self, ns: &iceberg::NamespaceIdent) -> iceberg::Result<bool> {
        self.get().await?.namespace_exists(ns).await
    }
    async fn update_namespace(
        &self,
        ns: &iceberg::NamespaceIdent,
        props: HashMap<String, String>,
    ) -> iceberg::Result<()> {
        self.get().await?.update_namespace(ns, props).await
    }
    async fn drop_namespace(&self, ns: &iceberg::NamespaceIdent) -> iceberg::Result<()> {
        self.get().await?.drop_namespace(ns).await
    }
    async fn list_tables(
        &self,
        ns: &iceberg::NamespaceIdent,
    ) -> iceberg::Result<Vec<iceberg::TableIdent>> {
        self.get().await?.list_tables(ns).await
    }
    async fn create_table(
        &self,
        ns: &iceberg::NamespaceIdent,
        creation: iceberg::TableCreation,
    ) -> iceberg::Result<iceberg::table::Table> {
        self.get().await?.create_table(ns, creation).await
    }
    async fn load_table(
        &self,
        ident: &iceberg::TableIdent,
    ) -> iceberg::Result<iceberg::table::Table> {
        self.get().await?.load_table(ident).await
    }
    async fn drop_table(&self, ident: &iceberg::TableIdent) -> iceberg::Result<()> {
        self.get().await?.drop_table(ident).await
    }
    async fn purge_table(&self, ident: &iceberg::TableIdent) -> iceberg::Result<()> {
        self.get().await?.purge_table(ident).await
    }
    async fn table_exists(&self, ident: &iceberg::TableIdent) -> iceberg::Result<bool> {
        self.get().await?.table_exists(ident).await
    }
    async fn rename_table(
        &self,
        src: &iceberg::TableIdent,
        dest: &iceberg::TableIdent,
    ) -> iceberg::Result<()> {
        self.get().await?.rename_table(src, dest).await
    }
    async fn register_table(
        &self,
        ident: &iceberg::TableIdent,
        metadata_location: String,
    ) -> iceberg::Result<iceberg::table::Table> {
        self.get()
            .await?
            .register_table(ident, metadata_location)
            .await
    }
    async fn update_table(
        &self,
        commit: iceberg::TableCommit,
    ) -> iceberg::Result<iceberg::table::Table> {
        self.get().await?.update_table(commit).await
    }
}

/// The catalog `MEMSTORE_PLATFORM` names, opened now, and how long its token lasts when the token says.
async fn open_from_env() -> Result<(Arc<dyn Catalog>, Option<std::time::Duration>)> {
    let platform = env("MEMSTORE_PLATFORM")?;
    if platform == "local" {
        // The local catalog lives in this process's memory: a new one would know none of the tables
        // made so far (a server refreshing its catalog would lose them all). One per warehouse.
        static LOCAL: tokio::sync::Mutex<Option<HashMap<String, Arc<dyn Catalog>>>> =
            tokio::sync::Mutex::const_new(None);
        let warehouse = env("MEMSTORE_WAREHOUSE")?;
        let mut held = LOCAL.lock().await;
        let held = held.get_or_insert_with(HashMap::new);
        if let Some(c) = held.get(&warehouse) {
            return Ok((c.clone(), None));
        }
        let c = local(&warehouse).await?;
        held.insert(warehouse, c.clone());
        return Ok((c, None));
    }
    let mut lasts = None;
    let props = match platform.as_str() {
        "gcp" => {
            let (token, ttl) = gcp_token_lasting()?;
            lasts = ttl;
            gcp_props_with(&env("GCP_PROJECT")?, &env("GCS_BUCKET")?, token, ttl)
        }
        // A service user's PAT wins (Native Apps: Horizon REST doesn't accept the app's own identity).
        "snowflake" if snowflake_pat().is_some() => {
            let mut props = snowflake_props(
                &std::env::var("SNOWFLAKE_ACCOUNT").unwrap_or_default(),
                &env("SNOWFLAKE_DATABASE")?,
                &env("SNOWFLAKE_ROLE")?,
                &snowflake_pat().unwrap_or_default(),
            );
            // SPCS's internal host only accepts the container's session token: a PAT has to come in
            // through the account's public URL (egress allowed by the app's network rule).
            if std::env::var("SNOWFLAKE_ACCOUNT")
                .unwrap_or_default()
                .is_empty()
                && let Ok(host) = std::env::var("SNOWFLAKE_HOST")
            {
                props.insert("uri".into(), format!("https://{host}/polaris/api/catalog"));
            }
            props
        }
        // Inside Snowpark Container Services: the service's own session token, as a bearer.
        // ponytail: read once; the file rotates, so long-lived services must rebuild the catalog.
        "snowflake" if std::path::Path::new(SPCS_TOKEN).exists() => HashMap::from([
            (
                "uri".into(),
                format!("https://{}/polaris/api/catalog", env("SNOWFLAKE_HOST")?),
            ),
            ("warehouse".into(), env("SNOWFLAKE_DATABASE")?),
            (
                "token".into(),
                std::fs::read_to_string(SPCS_TOKEN)?.trim().into(),
            ),
            (
                "header.X-Iceberg-Access-Delegation".into(),
                "vended-credentials".into(),
            ),
        ]),
        "snowflake" => snowflake_props(
            &env("SNOWFLAKE_ACCOUNT")?,
            &env("SNOWFLAKE_DATABASE")?,
            &env("SNOWFLAKE_ROLE")?,
            &env("SNOWFLAKE_PAT")?,
        ),
        "databricks" => databricks_props(
            &env("DATABRICKS_HOST")?,
            &env("DATABRICKS_CATALOG")?,
            secret("DATABRICKS_TOKEN"),
            secret("DATABRICKS_CLIENT_ID").zip(secret("DATABRICKS_CLIENT_SECRET")),
        )?,
        "rest" => {
            let mut props = HashMap::from([
                ("uri".into(), env("MEMSTORE_REST_URI")?),
                ("warehouse".into(), env("MEMSTORE_WAREHOUSE")?),
            ]);
            // Extra catalog/storage props as `key=value;key=value` (e.g. credential, s3.endpoint).
            for kv in std::env::var("MEMSTORE_REST_PROPS")
                .unwrap_or_default()
                .split(';')
            {
                if let Some((k, v)) = kv.split_once('=') {
                    props.insert(k.trim().into(), v.trim().into());
                }
            }
            props
        }
        other => bail!("unknown MEMSTORE_PLATFORM {other}"),
    };
    Ok((rest(&platform, props).await?, lasts))
}

/// An Iceberg REST catalog from its props (`uri`, `warehouse`, auth, and storage props such as
/// `s3.endpoint`), named `name`. Storage is resolved per table location (gs://, s3://, abfss://, file://),
/// with a cap on the requests in flight ([`STORAGE_REQUESTS`]).
pub async fn rest(name: &str, props: HashMap<String, String>) -> Result<Arc<dyn Catalog>> {
    let catalog = RestCatalogBuilder::default()
        .with_storage_factory(Arc::new(BoundedStorageFactory(
            OpenDalResolvingStorageFactory::new(),
        )))
        .load(name, props)
        .await?;
    Ok(Arc::new(catalog))
}

/// Object-store requests in flight at once in this process, every catalog and table together:
/// `MEMSTORE_STORAGE_REQUESTS` (default 64). opendal sends every request through one HTTP client whose pool
/// keeps every connection it opens for 90 s, and opens a new one whenever a request finds none idle, so
/// uncapped the connections grow with the most requests ever in flight. A memory build scanning many tables and
/// reading episode bodies held about 1,900 per process, and took down the port forwarder in front of its object
/// store (Docker Desktop's). The cap bounds them only roughly: a request that finds its connection still on its
/// way back to the pool opens another. On a 2,000-episode load and a full scan of it, 64 held 65–80
/// connections and 256 held 256–402, against 362–400 uncapped, in the same time.
/// A request waits its turn, first come first served. A permit covers one request, never a reader's or
/// writer's life, so a scan that is paused between reads holds none.
static STORAGE_REQUESTS: LazyLock<Semaphore> = LazyLock::new(|| {
    let n = std::env::var("MEMSTORE_STORAGE_REQUESTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n > 0)
        .unwrap_or(64);
    Semaphore::new(n)
});

async fn turn() -> SemaphorePermit<'static> {
    // Never closed, so acquire can't fail.
    STORAGE_REQUESTS.acquire().await.unwrap()
}

/// Builds the inner factory's storage behind [`BoundedStorage`].
#[derive(Debug, Serialize, Deserialize)]
struct BoundedStorageFactory(OpenDalResolvingStorageFactory);

#[typetag::serde(name = "memstore::BoundedStorageFactory")]
impl StorageFactory for BoundedStorageFactory {
    fn build(&self, config: &StorageConfig) -> iceberg::Result<Arc<dyn Storage>> {
        let inner: Arc<dyn Storage> = if config.props().contains_key(GCS_OWN_TOKEN) {
            Arc::new(OwnTokenStorage {
                props: config.props().clone(),
                built: Default::default(),
            })
        } else {
            self.0.build(config)?
        };
        Ok(Arc::new(BoundedStorage(inner)))
    }
}

/// A storage reached with the workload's own token: at each request, the token this process holds now
/// ([`own_token`]), not the one its file IO was built with. A file IO lives as long as the table handle that
/// carries it (a cached table, the table a commit is pinned to, a long job's), and with the token fixed, one used
/// after its token ran out was refused (GCS 401 "Invalid Credentials", about once an hour on a pod serving many
/// orgs, on reads of the ledger's manifests). Vended credentials (another platform's) are never replaced.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct OwnTokenStorage {
    /// The file IO's properties, the token it was built with among them.
    props: HashMap<String, String>,
    /// The storage built with the own token of a generation ([`OwnToken::generation`]; 0: the token in `props`),
    /// shared by the input and output files handed out.
    #[serde(skip, default)]
    built: Arc<std::sync::RwLock<Option<Built>>>,
}

/// A storage, and the generation of the own token it was built with.
type Built = (u64, Arc<dyn Storage>);

impl OwnTokenStorage {
    /// The storage for the own token held now, built again when a new one has been taken.
    async fn current(&self) -> iceberg::Result<Arc<dyn Storage>> {
        let own = match own_token_held() {
            Some(t) => Some(t),
            // The metadata server is asked off the async threads.
            None => tokio::task::spawn_blocking(own_token)
                .await
                .ok()
                .and_then(|r| {
                    r.map_err(|e| eprintln!("memstore: no storage token taken: {e:#}"))
                        .ok()
                }),
        };
        let generation = own.as_ref().map_or(0, |t| t.generation);
        if let Some((g, s)) = self.built.read().unwrap().as_ref()
            && *g == generation
        {
            return Ok(s.clone());
        }
        let mut props = self.props.clone();
        if let Some(t) = own {
            props.insert(iceberg::io::GCS_TOKEN.into(), t.token);
            props.insert(
                GCS_TOKEN_EXPIRES_AT.into(),
                t.expires.timestamp_millis().to_string(),
            );
        }
        let s = OpenDalResolvingStorageFactory::new().build(&StorageConfig::from_props(props))?;
        *self.built.write().unwrap() = Some((generation, s.clone()));
        Ok(s)
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "memstore::OwnTokenStorage")]
impl Storage for OwnTokenStorage {
    async fn exists(&self, path: &str) -> iceberg::Result<bool> {
        self.current().await?.exists(path).await
    }

    async fn metadata(&self, path: &str) -> iceberg::Result<FileMetadata> {
        self.current().await?.metadata(path).await
    }

    async fn read(&self, path: &str) -> iceberg::Result<Bytes> {
        self.current().await?.read(path).await
    }

    async fn reader(&self, path: &str) -> iceberg::Result<Box<dyn FileRead>> {
        self.current().await?.reader(path).await
    }

    async fn write(&self, path: &str, bs: Bytes) -> iceberg::Result<()> {
        self.current().await?.write(path, bs).await
    }

    async fn writer(&self, path: &str) -> iceberg::Result<Box<dyn FileWrite>> {
        self.current().await?.writer(path).await
    }

    async fn delete(&self, path: &str) -> iceberg::Result<()> {
        self.current().await?.delete(path).await
    }

    async fn delete_prefix(&self, path: &str) -> iceberg::Result<()> {
        self.current().await?.delete_prefix(path).await
    }

    async fn delete_stream(&self, paths: BoxStream<'static, String>) -> iceberg::Result<()> {
        self.current().await?.delete_stream(paths).await
    }

    fn new_input(&self, path: &str) -> iceberg::Result<InputFile> {
        Ok(InputFile::new(Arc::new(self.clone()), path.to_string()))
    }

    fn new_output(&self, path: &str) -> iceberg::Result<OutputFile> {
        Ok(OutputFile::new(Arc::new(self.clone()), path.to_string()))
    }
}

/// A storage whose every request waits for a [`STORAGE_REQUESTS`] permit. Readers, writers and the input and
/// output files it hands out go through it too.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct BoundedStorage(Arc<dyn Storage>);

#[async_trait::async_trait]
#[typetag::serde(name = "memstore::BoundedStorage")]
impl Storage for BoundedStorage {
    async fn exists(&self, path: &str) -> iceberg::Result<bool> {
        let _turn = turn().await;
        self.0.exists(path).await
    }

    async fn metadata(&self, path: &str) -> iceberg::Result<FileMetadata> {
        let _turn = turn().await;
        self.0.metadata(path).await
    }

    async fn read(&self, path: &str) -> iceberg::Result<Bytes> {
        let _turn = turn().await;
        self.0.read(path).await
    }

    async fn reader(&self, path: &str) -> iceberg::Result<Box<dyn FileRead>> {
        let _turn = turn().await;
        Ok(Box::new(BoundedRead(self.0.reader(path).await?)))
    }

    async fn write(&self, path: &str, bs: Bytes) -> iceberg::Result<()> {
        let _turn = turn().await;
        self.0.write(path, bs).await
    }

    async fn writer(&self, path: &str) -> iceberg::Result<Box<dyn FileWrite>> {
        let _turn = turn().await;
        Ok(Box::new(BoundedWrite(self.0.writer(path).await?)))
    }

    async fn delete(&self, path: &str) -> iceberg::Result<()> {
        let _turn = turn().await;
        self.0.delete(path).await
    }

    async fn delete_prefix(&self, path: &str) -> iceberg::Result<()> {
        let _turn = turn().await;
        self.0.delete_prefix(path).await
    }

    async fn delete_stream(&self, paths: BoxStream<'static, String>) -> iceberg::Result<()> {
        let _turn = turn().await;
        self.0.delete_stream(paths).await
    }

    fn new_input(&self, path: &str) -> iceberg::Result<InputFile> {
        Ok(InputFile::new(Arc::new(self.clone()), path.to_string()))
    }

    fn new_output(&self, path: &str) -> iceberg::Result<OutputFile> {
        Ok(OutputFile::new(Arc::new(self.clone()), path.to_string()))
    }
}

/// One permit per range read (a Parquet scan reads a file in several).
struct BoundedRead(Box<dyn FileRead>);

#[async_trait::async_trait]
impl FileRead for BoundedRead {
    async fn read(&self, range: Range<u64>) -> iceberg::Result<Bytes> {
        let _turn = turn().await;
        self.0.read(range).await
    }
}

/// One permit per write (a part upload, once the buffer fills) and for the close that finishes the upload.
struct BoundedWrite(Box<dyn FileWrite>);

#[async_trait::async_trait]
impl FileWrite for BoundedWrite {
    async fn write(&mut self, bs: Bytes) -> iceberg::Result<()> {
        let _turn = turn().await;
        self.0.write(bs).await
    }

    async fn close(&mut self) -> iceberg::Result<()> {
        let _turn = turn().await;
        self.0.close().await
    }
}

/// GCP: BigLake metastore Iceberg REST catalog over a Cloud Storage bucket (end-user credentials).
pub fn gcp_props(project: &str, bucket: &str) -> Result<HashMap<String, String>> {
    let (token, lasts) = gcp_token_lasting()?;
    Ok(gcp_props_with(project, bucket, token, lasts))
}

/// When a storage token in a catalog's file IO properties runs out, in ms since the epoch: the name the Iceberg
/// REST spec gives a vended GCS token's expiry. Set beside our own token where its source says how long it lasts
/// (the metadata server), so what takes the token from a table's file IO (the writer lease, the reports) takes
/// it again before then ([`token_expires_at`]).
pub const GCS_TOKEN_EXPIRES_AT: &str = "gcs.oauth2.token-expires-at";

/// Set in a catalog's file IO properties when their GCS token is the workload's own ([`own_token`], from the
/// metadata server): a file IO built with them takes the token this process holds at each request
/// ([`OwnTokenStorage`]), never only the one it was built with. Never set for a token vended for a table.
const GCS_OWN_TOKEN: &str = "memstore.gcs.own-token";

fn gcp_props_with(
    project: &str,
    bucket: &str,
    token: String,
    lasts: Option<std::time::Duration>,
) -> HashMap<String, String> {
    let mut props = HashMap::from([
        (
            "uri".into(),
            "https://biglake.googleapis.com/iceberg/v1/restcatalog".into(),
        ),
        ("warehouse".into(), format!("gs://{bucket}")),
        ("token".into(), token.clone()),
        ("header.x-goog-user-project".into(), project.into()),
        (iceberg::io::GCS_TOKEN.into(), token),
    ]);
    // Only the metadata server says how long its token lasts: that token is the workload's own.
    if let Some(t) = lasts {
        let at = chrono::Utc::now() + t;
        props.insert(
            GCS_TOKEN_EXPIRES_AT.into(),
            at.timestamp_millis().to_string(),
        );
        props.insert(GCS_OWN_TOKEN.into(), "true".into());
    }
    props
}

/// When the earliest storage token in file IO properties `props` runs out, where they say: a GCS token's
/// ([`GCS_TOKEN_EXPIRES_AT`]), an S3 session token's or an ADLS SAS token's, by the Iceberg REST spec's names.
pub fn token_expires_at(props: &HashMap<String, String>) -> Option<chrono::DateTime<chrono::Utc>> {
    props
        .iter()
        .filter(|(k, _)| {
            *k == GCS_TOKEN_EXPIRES_AT
                || *k == "s3.session-token-expires-at-ms"
                || k.starts_with("adls.sas-token-expires-at-ms.")
        })
        .filter_map(|(_, v)| v.trim().parse::<i64>().ok())
        .min()
        .and_then(chrono::DateTime::from_timestamp_millis)
}

/// Snowflake: Horizon Catalog Iceberg REST endpoint. The PAT is exchanged for a session token
/// scoped to one role, and storage credentials are vended per table.
pub fn snowflake_props(
    account: &str,
    database: &str,
    role: &str,
    pat: &str,
) -> HashMap<String, String> {
    HashMap::from([
        (
            "uri".into(),
            format!("https://{account}.snowflakecomputing.com/polaris/api/catalog"),
        ),
        ("warehouse".into(), database.into()),
        ("credential".into(), pat.into()),
        ("scope".into(), format!("session:role:{role}")),
        (
            "header.X-Iceberg-Access-Delegation".into(),
            "vended-credentials".into(),
        ),
    ])
}

/// Databricks: Unity Catalog's Iceberg REST endpoint; `warehouse` is the catalog. A service principal's OAuth
/// client credentials (exchanged at the workspace's token endpoint, scope all-apis) or a PAT. Storage credentials
/// are vended per table, which needs a catalog on customer storage (not default storage), External data access
/// on the metastore and EXTERNAL USE SCHEMA for the principal.
pub fn databricks_props(
    host: &str,
    catalog: &str,
    token: Option<String>,
    client: Option<(String, String)>,
) -> Result<HashMap<String, String>> {
    let host = host.trim_end_matches('/');
    let host = if host.starts_with("https://") {
        host.to_string()
    } else {
        format!("https://{host}")
    };
    let mut props = HashMap::from([
        (
            "uri".into(),
            format!("{host}/api/2.1/unity-catalog/iceberg-rest"),
        ),
        ("warehouse".into(), catalog.into()),
        (
            "header.X-Iceberg-Access-Delegation".into(),
            "vended-credentials".into(),
        ),
    ]);
    match (client, token) {
        // ponytail: the OAuth token is fetched when the catalog is built (1 h); `serve` rebuilds it every 5 min.
        (Some((id, secret)), _) => {
            props.insert("credential".into(), format!("{id}:{secret}"));
            props.insert("oauth2-server-uri".into(), format!("{host}/oidc/v1/token"));
            props.insert("scope".into(), "all-apis".into());
        }
        (None, Some(t)) => {
            props.insert("token".into(), t);
        }
        (None, None) => bail!(
            "set DATABRICKS_CLIENT_ID + DATABRICKS_CLIENT_SECRET, or DATABRICKS_TOKEN (or their _FILE)"
        ),
    }
    Ok(props)
}

/// Local filesystem catalog for tests and development.
pub async fn local(warehouse: &str) -> Result<Arc<dyn Catalog>> {
    let catalog = MemoryCatalogBuilder::default()
        .with_storage_factory(Arc::new(OpenDalStorageFactory::Fs))
        .load(
            "local",
            HashMap::from([(MEMORY_CATALOG_WAREHOUSE.into(), warehouse.into())]),
        )
        .await?;
    Ok(Arc::new(catalog))
}

/// A catalog for one machine that survives restarts (`memstore serve --local`). Tables live under
/// `<dir>/warehouse` as for [`local`]; which tables exist and where each one's current metadata file is
/// are kept in `<dir>/catalog.json`, rewritten after every change and read back on open. The in-memory
/// catalog underneath still does the work, including the compare-and-set on commit, so one process
/// owns a directory at a time: it holds a lock on `<dir>/.lock` while it runs, and a second process
/// is refused (each would rewrite catalog.json from its own view and undo the other's commits).
pub async fn local_dir(dir: &std::path::Path) -> Result<Arc<dyn Catalog>> {
    std::fs::create_dir_all(dir.join("warehouse"))
        .with_context(|| format!("create {}", dir.display()))?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("resolve {}", dir.display()))?;
    let lock = own(&dir)?;
    let inner = local(&format!("file://{}", dir.join("warehouse").display())).await?;
    let file = dir.join("catalog.json");
    let saved: SavedCatalog = match std::fs::read(&file) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("read {}", file.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SavedCatalog::default(),
        Err(e) => return Err(e).with_context(|| format!("read {}", file.display())),
    };
    for (ns, props) in &saved.namespaces {
        inner
            .create_namespace(&iceberg::NamespaceIdent::from_strs(ns)?, props.clone())
            .await?;
    }
    for t in &saved.tables {
        let ident = iceberg::TableIdent::new(
            iceberg::NamespaceIdent::from_strs(&t.namespace)?,
            t.name.clone(),
        );
        inner
            .register_table(&ident, t.metadata_location.clone())
            .await
            .with_context(|| format!("reopen table {}", t.name))?;
    }
    Ok(Arc::new(LocalDirCatalog {
        inner,
        file,
        saved: std::sync::Mutex::new(saved),
        _lock: lock,
    }))
}

/// Lock `<dir>/.lock` for this process (released when it exits, however it exits), or say which process
/// has it.
fn own(dir: &std::path::Path) -> Result<std::fs::File> {
    use std::io::{Read, Seek, Write};
    let path = dir.join(".lock");
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    match f.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut pid = String::new();
            let _ = f.read_to_string(&mut pid);
            let pid = pid.trim();
            anyhow::bail!(
                "{} is in use by another memstore process{}: stop it first, or run this through it \
                 (one process owns a local store at a time)",
                dir.display(),
                if pid.is_empty() {
                    String::new()
                } else {
                    format!(" (pid {pid})")
                }
            );
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("lock {}", path.display()));
        }
    }
    f.set_len(0)?;
    f.rewind()?;
    write!(f, "{}", std::process::id())?;
    Ok(f)
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct SavedCatalog {
    namespaces: Vec<(Vec<String>, HashMap<String, String>)>,
    tables: Vec<SavedTable>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedTable {
    namespace: Vec<String>,
    name: String,
    metadata_location: String,
}

#[derive(Debug)]
struct LocalDirCatalog {
    inner: Arc<dyn Catalog>,
    file: std::path::PathBuf,
    saved: std::sync::Mutex<SavedCatalog>,
    /// Held for the process's life: see [`own`].
    _lock: std::fs::File,
}

impl std::fmt::Debug for SavedCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} tables", self.tables.len())
    }
}

impl LocalDirCatalog {
    /// Apply a change to what's saved and write it out (to a temporary file, then renamed, so a crash
    /// leaves the old file or the new one).
    fn save(&self, change: impl FnOnce(&mut SavedCatalog)) -> iceberg::Result<()> {
        let mut saved = self.saved.lock().unwrap();
        change(&mut saved);
        let tmp = self.file.with_extension("json.tmp");
        let io = |e: std::io::Error| {
            iceberg::Error::new(iceberg::ErrorKind::Unexpected, "write catalog.json").with_source(e)
        };
        std::fs::write(&tmp, serde_json::to_vec_pretty(&*saved).unwrap()).map_err(io)?;
        std::fs::rename(&tmp, &self.file).map_err(io)
    }

    fn remember_table(&self, table: &iceberg::table::Table) -> iceberg::Result<()> {
        let ident = table.identifier();
        let namespace = ident.namespace().clone().inner();
        let name = ident.name().to_string();
        let metadata_location = table.metadata_location_result()?.to_string();
        self.save(|s| {
            s.tables
                .retain(|t| !(t.namespace == namespace && t.name == name));
            s.tables.push(SavedTable {
                namespace,
                name,
                metadata_location,
            });
        })
    }

    fn forget_table(&self, ident: &iceberg::TableIdent) -> iceberg::Result<()> {
        let namespace = ident.namespace().clone().inner();
        self.save(|s| {
            s.tables
                .retain(|t| !(t.namespace == namespace && t.name == ident.name()))
        })
    }
}

#[async_trait::async_trait]
impl Catalog for LocalDirCatalog {
    async fn list_namespaces(
        &self,
        parent: Option<&iceberg::NamespaceIdent>,
    ) -> iceberg::Result<Vec<iceberg::NamespaceIdent>> {
        self.inner.list_namespaces(parent).await
    }
    async fn create_namespace(
        &self,
        ns: &iceberg::NamespaceIdent,
        props: HashMap<String, String>,
    ) -> iceberg::Result<iceberg::Namespace> {
        let created = self.inner.create_namespace(ns, props.clone()).await?;
        let key = ns.clone().inner();
        self.save(|s| {
            s.namespaces.retain(|(n, _)| *n != key);
            s.namespaces.push((key, props));
        })?;
        Ok(created)
    }
    async fn get_namespace(
        &self,
        ns: &iceberg::NamespaceIdent,
    ) -> iceberg::Result<iceberg::Namespace> {
        self.inner.get_namespace(ns).await
    }
    async fn namespace_exists(&self, ns: &iceberg::NamespaceIdent) -> iceberg::Result<bool> {
        self.inner.namespace_exists(ns).await
    }
    async fn update_namespace(
        &self,
        ns: &iceberg::NamespaceIdent,
        props: HashMap<String, String>,
    ) -> iceberg::Result<()> {
        self.inner.update_namespace(ns, props.clone()).await?;
        let key = ns.clone().inner();
        self.save(|s| {
            s.namespaces.retain(|(n, _)| *n != key);
            s.namespaces.push((key, props));
        })
    }
    async fn drop_namespace(&self, ns: &iceberg::NamespaceIdent) -> iceberg::Result<()> {
        self.inner.drop_namespace(ns).await?;
        let key = ns.clone().inner();
        self.save(|s| s.namespaces.retain(|(n, _)| *n != key))
    }
    async fn list_tables(
        &self,
        ns: &iceberg::NamespaceIdent,
    ) -> iceberg::Result<Vec<iceberg::TableIdent>> {
        self.inner.list_tables(ns).await
    }
    async fn create_table(
        &self,
        ns: &iceberg::NamespaceIdent,
        creation: iceberg::TableCreation,
    ) -> iceberg::Result<iceberg::table::Table> {
        let t = self.inner.create_table(ns, creation).await?;
        self.remember_table(&t)?;
        Ok(t)
    }
    async fn load_table(
        &self,
        ident: &iceberg::TableIdent,
    ) -> iceberg::Result<iceberg::table::Table> {
        self.inner.load_table(ident).await
    }
    async fn drop_table(&self, ident: &iceberg::TableIdent) -> iceberg::Result<()> {
        self.inner.drop_table(ident).await?;
        self.forget_table(ident)
    }
    async fn purge_table(&self, ident: &iceberg::TableIdent) -> iceberg::Result<()> {
        let purged = self.inner.purge_table(ident).await;
        // The in-memory catalog drops the table before deleting its files: a purge that fails partway has still
        // dropped it, and it must not come back when this directory is opened again.
        if purged.is_ok() || !self.inner.table_exists(ident).await? {
            self.forget_table(ident)?;
        }
        purged
    }
    async fn table_exists(&self, ident: &iceberg::TableIdent) -> iceberg::Result<bool> {
        self.inner.table_exists(ident).await
    }
    async fn rename_table(
        &self,
        src: &iceberg::TableIdent,
        dest: &iceberg::TableIdent,
    ) -> iceberg::Result<()> {
        self.inner.rename_table(src, dest).await?;
        self.forget_table(src)?;
        let t = self.inner.load_table(dest).await?;
        self.remember_table(&t)
    }
    async fn register_table(
        &self,
        ident: &iceberg::TableIdent,
        metadata_location: String,
    ) -> iceberg::Result<iceberg::table::Table> {
        let t = self.inner.register_table(ident, metadata_location).await?;
        self.remember_table(&t)?;
        Ok(t)
    }
    async fn update_table(
        &self,
        commit: iceberg::TableCommit,
    ) -> iceberg::Result<iceberg::table::Table> {
        let t = self.inner.update_table(commit).await?;
        self.remember_table(&t)?;
        Ok(t)
    }
}

/// A Google access token for the caller (gcloud's application default credentials, or the
/// metadata server on Cloud Run and GKE).
pub fn google_token() -> Result<String> {
    gcp_token()
}

fn gcp_token() -> Result<String> {
    Ok(gcp_token_lasting()?.0)
}

/// A Google access token, and how long it has left when its source says (the metadata server does; gcloud and
/// `GCP_ACCESS_TOKEN` don't, and the caller assumes an hour from now).
fn gcp_token_lasting() -> Result<(String, Option<std::time::Duration>)> {
    if let Ok(t) = std::env::var("GCP_ACCESS_TOKEN") {
        return Ok((t, None));
    }
    // Cloud Run services and jobs, and GKE pods (workload identity), have no gcloud: the service account's token
    // from the metadata server, as this process holds it ([`own_token`]).
    if ["K_SERVICE", "CLOUD_RUN_JOB", "KUBERNETES_SERVICE_HOST"]
        .iter()
        .any(|k| std::env::var(k).is_ok())
    {
        let t = own_token()?;
        let left = (t.expires - chrono::Utc::now())
            .to_std()
            .unwrap_or_default();
        return Ok((t.token, Some(left)));
    }
    let out = std::process::Command::new("gcloud")
        .args(["auth", "application-default", "print-access-token"])
        .output()
        .context("run gcloud for an access token")?;
    anyhow::ensure!(
        out.status.success(),
        "gcloud: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok((String::from_utf8(out.stdout)?.trim().to_string(), None))
}

fn metadata(path: &str) -> Result<String> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect("metadata.google.internal:80")?;
    write!(
        s,
        "GET /computeMetadata/v1/{path} HTTP/1.0\r\nHost: metadata.google.internal\r\nMetadata-Flavor: Google\r\n\r\n"
    )?;
    let mut resp = String::new();
    s.read_to_string(&mut resp)?;
    Ok(resp
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_string())
}

fn metadata_token() -> Result<String> {
    Ok(metadata_token_lasting()?.0)
}

/// The service account's token from the metadata server, and the life it has left (`expires_in`): the server hands
/// out its cached token, so that can be minutes.
fn metadata_token_lasting() -> Result<(String, Option<std::time::Duration>)> {
    let v: serde_json::Value =
        serde_json::from_str(&metadata("instance/service-accounts/default/token")?)
            .context("metadata token response")?;
    let token = v["access_token"]
        .as_str()
        .map(String::from)
        .context("no access_token from the metadata server")?;
    Ok((
        token,
        v["expires_in"].as_u64().map(std::time::Duration::from_secs),
    ))
}

/// The workload's own storage token, as this process holds it: one for the catalogs it opens and for every file IO
/// that reads or writes with it ([`OwnTokenStorage`]), taken again from the metadata server shortly before it runs
/// out ([`RENEW_BEFORE`]).
#[derive(Clone, Debug)]
struct OwnToken {
    token: String,
    expires: chrono::DateTime<chrono::Utc>,
    /// Bumped each time a new token is taken: a storage built with an older one builds itself again.
    generation: u64,
}

/// The own token, and when the metadata server was last asked for one.
static OWN_TOKEN: std::sync::Mutex<Option<(OwnToken, std::time::Instant)>> =
    std::sync::Mutex::new(None);

/// How long before the own token runs out it is taken again. The metadata server hands out its cached token until
/// it takes the next one, about five minutes before the old one runs out (5m11s and 5m43s on GKE, Nov 2026), so
/// asked four minutes before, it has the next one.
const RENEW_BEFORE: std::time::Duration = std::time::Duration::from_secs(240);

/// While the metadata server still hands out a token that runs out within [`RENEW_BEFORE`], it is asked again no
/// sooner than this.
const ASK_AGAIN: std::time::Duration = std::time::Duration::from_secs(10);

/// The own token held now, if it needs no new one ([`RENEW_BEFORE`], [`ASK_AGAIN`]): no call to the metadata server.
fn own_token_held() -> Option<OwnToken> {
    fresh(&OWN_TOKEN.lock().unwrap())
}

fn fresh(held: &Option<(OwnToken, std::time::Instant)>) -> Option<OwnToken> {
    let (t, asked) = held.as_ref()?;
    let lasts = (t.expires - chrono::Utc::now())
        .to_std()
        .unwrap_or_default();
    (lasts > RENEW_BEFORE || asked.elapsed() < ASK_AGAIN).then(|| t.clone())
}

/// The own token: the one held while it lasts, else a new one from the metadata server.
fn own_token() -> Result<OwnToken> {
    own_token_from(metadata_token_lasting)
}

/// [`own_token`], with `fetch` for the metadata server.
fn own_token_from(
    fetch: impl FnOnce() -> Result<(String, Option<std::time::Duration>)>,
) -> Result<OwnToken> {
    {
        let mut held = OWN_TOKEN.lock().unwrap();
        if let Some(t) = fresh(&held) {
            return Ok(t);
        }
        // This caller asks: the others take the token held meanwhile (for ASK_AGAIN), not ask too.
        if let Some((_, asked)) = held.as_mut() {
            *asked = std::time::Instant::now();
        }
    }
    // Not under the lock: the call can take a moment.
    let fetched = fetch();
    let mut held = OWN_TOKEN.lock().unwrap();
    let (token, lasts) = match fetched {
        Ok(t) => t,
        Err(e) => {
            // The token held, while it lasts, rather than no storage at all; asked again after ASK_AGAIN.
            if let Some((t, _)) = held.as_ref()
                && t.expires > chrono::Utc::now()
            {
                eprintln!(
                    "memstore: taking a new storage token failed (keeping the one held): {e:#}"
                );
                return Ok(t.clone());
            }
            return Err(e);
        }
    };
    // The server's own life for it; an hour where it doesn't say.
    let expires = chrono::Utc::now() + lasts.unwrap_or(std::time::Duration::from_secs(3600));
    let generation = match held.as_ref() {
        Some((t, _)) if t.token == token => t.generation,
        Some((t, _)) => t.generation + 1,
        None => 1,
    };
    let t = OwnToken {
        token,
        expires,
        generation,
    };
    *held = Some((t.clone(), std::time::Instant::now()));
    Ok(t)
}

/// Whether this Cloud Run service still makes Cloud Run IAM verify every caller's token (no `allUsers` invoker,
/// the invoker check not disabled; only then may a signature-stripped token be believed), and the service's
/// URLs, the audiences its callers' tokens are minted for.
/// Needs `run.services.get` and `run.services.getIamPolicy` on the service (roles/run.viewer).
pub async fn cloud_run_verifies_callers() -> Result<(bool, Vec<String>)> {
    let service = env("K_SERVICE")?;
    // "projects/<number>/regions/<region>"
    let region = metadata("instance/region")?
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let url = format!(
        "https://run.googleapis.com/v2/projects/{}/locations/{region}/services/{service}",
        env("GCP_PROJECT")?
    );
    let token = metadata_token()?;
    let client = reqwest::Client::new();
    let get = |u: String| client.get(u).bearer_auth(&token).send();
    let svc: serde_json::Value = get(url.clone()).await?.error_for_status()?.json().await?;
    let policy: serde_json::Value = get(format!("{url}:getIamPolicy"))
        .await?
        .error_for_status()?
        .json()
        .await?;
    let public = policy["bindings"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|b| {
            b["role"] == "roles/run.invoker"
                && b["members"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|m| m == "allUsers")
        });
    let urls = svc["urls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u.as_str())
        .map(|u| u.trim_end_matches('/').to_string())
        .collect();
    Ok((svc["invokerIamDisabled"] != true && !public, urls))
}

/// Google: who an ID token belongs to: its verified email if it has one (BigQuery's service agent sends tokens
/// without), and its numeric subject id. Two shapes arrive through Cloud Run IAM:
/// - Signature replaced with `SIGNATURE_REMOVED_BY_GOOGLE` (user tokens): Cloud Run verified it, so the claims are
///   read as they are. That trusts Cloud Run IAM, so it's allowed only while `iam_verified` (the caller passes
///   what `cloud_run_verifies_callers` last said); otherwise anyone could hand-make one.
/// - Signature intact (service accounts, BigQuery): Google's tokeninfo checks it, and it must be minted for this
///   service (`audiences`, from `MEMSTORE_ID_AUDIENCE`), so a token issued for some other app can't be replayed.
///
/// User tokens carry their own client's audience (gcloud's, Claude's), which is why the stripped shape has no
/// audience check: Cloud Run IAM has already decided they may call.
pub async fn google_caller(
    id_token: &str,
    iam_verified: bool,
    audiences: &[String],
) -> Result<(Option<String>, String)> {
    let claims: serde_json::Value = match id_token.split('.').collect::<Vec<_>>()[..] {
        [_, payload, "SIGNATURE_REMOVED_BY_GOOGLE"] => {
            anyhow::ensure!(
                iam_verified,
                "Cloud Run IAM isn't confirmed to verify callers"
            );
            use base64::Engine;
            let claims: serde_json::Value = serde_json::from_slice(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(payload.trim_end_matches('='))?,
            )?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            anyhow::ensure!(
                claims["exp"].as_u64().is_some_and(|e| e > now),
                "expired token"
            );
            anyhow::ensure!(
                matches!(
                    claims["iss"].as_str(),
                    Some("https://accounts.google.com" | "accounts.google.com")
                ),
                "token not issued by Google"
            );
            claims
        }
        _ => {
            let resp = reqwest::Client::new()
                .post(google_api("oauth2.googleapis.com", "/tokeninfo"))
                .form(&[("id_token", id_token)])
                .send()
                .await?;
            let status = resp.status();
            anyhow::ensure!(status.is_success(), "tokeninfo: HTTP {status}");
            let claims: serde_json::Value = resp.json().await?;
            let aud = claims["aud"].as_str().unwrap_or_default();
            // BigQuery mints for the endpoint URL, path included.
            anyhow::ensure!(
                audiences.iter().any(|a| !a.is_empty()
                    && (aud == a
                        || aud
                            .strip_prefix(a.as_str())
                            .is_some_and(|rest| rest.starts_with('/')))),
                "token minted for another audience ({aud})"
            );
            claims
        }
    };
    let verified = claims["email_verified"] == "true" || claims["email_verified"] == true;
    let email = claims["email"]
        .as_str()
        .filter(|e| verified && !e.is_empty());
    let sub = claims["sub"].as_str().filter(|s| !s.is_empty());
    Ok((
        email.map(String::from),
        sub.ok_or_else(|| anyhow::anyhow!("token has no subject"))?
            .into(),
    ))
}

/// GCP: run one parameterized GoogleSQL statement as the engine's service account (`params`: name, type,
/// value; type `ARRAY<STRING>` takes a JSON array of strings). A DML insert, not streaming: rows in the
/// streaming buffer couldn't be deleted by ingest afterwards.
pub async fn bigquery_dml(sql: &str, params: &[(&str, &str, serde_json::Value)]) -> Result<()> {
    let project = env("GCP_PROJECT")?;
    let params: Vec<serde_json::Value> = params
        .iter()
        .map(
            |(name, ty, v)| match ty.strip_prefix("ARRAY<").and_then(|t| t.strip_suffix('>')) {
                Some(inner) => serde_json::json!({"name": name,
                "parameterType": {"type": "ARRAY", "arrayType": {"type": inner}},
                "parameterValue": {"arrayValues": v.as_array().into_iter().flatten()
                    .map(|x| serde_json::json!({"value": x})).collect::<Vec<_>>()}}),
                None => serde_json::json!({"name": name, "parameterType": {"type": ty},
                "parameterValue": {"value": v}}),
            },
        )
        .collect();
    let mut body = serde_json::json!({"query": sql, "useLegacySql": false, "parameterMode": "NAMED",
        "queryParameters": params, "timeoutMs": 30000});
    if let Ok(region) = std::env::var("GCP_REGION") {
        body["location"] = region.into();
    }
    let resp = reqwest::Client::new()
        .post(format!(
            "https://bigquery.googleapis.com/bigquery/v2/projects/{project}/queries"
        ))
        .bearer_auth(metadata_token()?)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let out: serde_json::Value = resp.json().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "BigQuery: HTTP {status} {}",
        out["error"]["message"]
    );
    anyhow::ensure!(out["errors"].is_null(), "BigQuery: {}", out["errors"]);
    Ok(())
}

/// GCP: one Gemini completion through Vertex AI, as the engine's service account (roles/aiplatform.user).
pub async fn gemini(model: &str, prompt: &str) -> Result<String> {
    let project = env("GCP_PROJECT")?;
    let resp = reqwest::Client::new()
        .post(format!(
            "https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/publishers/google/models/{model}:generateContent"
        ))
        .bearer_auth(metadata_token()?)
        .json(&serde_json::json!({"contents": [{"role": "user", "parts": [{"text": prompt}]}]}))
        .send()
        .await?;
    let status = resp.status();
    let out: serde_json::Value = resp.json().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "Vertex AI: HTTP {status} {}",
        out["error"]["message"]
    );
    Ok(out["candidates"][0]["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["text"].as_str())
        .collect())
}

/// `https://<host><path>`, or a stand-in's in tests: `MEMSTORE_GOOGLE_API` is its base URL (tokeninfo and
/// Cloud Identity both), and [`google_api_token`] its bearer.
fn google_api(host: &str, path: &str) -> String {
    match std::env::var("MEMSTORE_GOOGLE_API") {
        Ok(base) => format!("{}{path}", base.trim_end_matches('/')),
        Err(_) => format!("https://{host}{path}"),
    }
}

/// The service account's token for Google's APIs (`GCP_ACCESS_TOKEN` against a stand-in).
fn google_api_token() -> Result<String> {
    match std::env::var("MEMSTORE_GOOGLE_API") {
        Ok(_) => gcp_token(),
        Err(_) => metadata_token(),
    }
}

/// Google: the groups (direct and nested) a user belongs to, by group email (matched by `group:<email>` ACL entries). Needs the
/// Cloud Identity API and, in the Workspace admin console, the Groups Reader role for the engine's service
/// account; without it the lookup fails and the caller gets no groups (fewer rights, never more).
/// ponytail: first page only (500 groups); follow nextPageToken if a user ever has more.
pub async fn google_groups(email: &str) -> Result<Vec<String>> {
    anyhow::ensure!(
        !email.contains('\'') && !email.contains('\\'),
        "unexpected character in email"
    );
    let resp = reqwest::Client::new()
        .get(google_api(
            "cloudidentity.googleapis.com",
            "/v1/groups/-/memberships:searchTransitiveGroups",
        ))
        .query(&[
            (
                "query",
                format!(
                    "member_key_id == '{email}' && 'cloudidentity.googleapis.com/groups.discussion_forum' in labels"
                ),
            ),
            ("pageSize", "500".into()),
        ])
        .bearer_auth(google_api_token()?)
        .send()
        .await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "Cloud Identity: HTTP {status} {}",
        body["error"]["message"]
    );
    Ok(body["memberships"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["groupKey"]["id"].as_str())
        .map(|g| g.to_ascii_lowercase())
        .collect())
}

/// Inside Snowpark Container Services with `executeAsCaller`: who the caller is, as Snowflake says when the
/// service connects with `<service token>.<Sf-Context-Current-User-Token>` (SQL API, internal host). Snowflake
/// validates that token, so unlike the plain `Sf-Context-Current-User` header it can't be forged.
/// Returns the caller's user name and which of `roles` (upper-case names) their session holds, directly or
/// through inherited roles.
pub async fn snowflake_caller(user_token: &str, roles: &[String]) -> Result<(String, Vec<String>)> {
    // Role names go into the statement as literals: only plain identifiers are checked.
    let roles: Vec<&String> = roles
        .iter()
        .filter(|r| {
            !r.is_empty()
                && r.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        })
        .collect();
    let checks: String = roles
        .iter()
        .map(|r| format!(", IS_ROLE_IN_SESSION('{r}')"))
        .collect();
    // A stand-in SQL API (tests): `MEMSTORE_SNOWFLAKE_API` is its base URL, `MEMSTORE_SPCS_TOKEN_FILE` its token.
    let token_file =
        std::env::var("MEMSTORE_SPCS_TOKEN_FILE").unwrap_or_else(|_| SPCS_TOKEN.into());
    let service_token = std::fs::read_to_string(&token_file).context("SPCS session token")?;
    let api = match std::env::var("MEMSTORE_SNOWFLAKE_API") {
        Ok(base) => base,
        Err(_) => format!("https://{}", env("SNOWFLAKE_HOST")?),
    };
    let resp = reqwest::Client::new()
        .post(format!("{api}/api/v2/statements"))
        .bearer_auth(format!("{}.{}", service_token.trim(), user_token.trim()))
        .header("X-Snowflake-Authorization-Token-Type", "OAUTH")
        .header("User-Agent", concat!("memstore/", env!("CARGO_PKG_VERSION"))) // the SQL API requires one
        .header("Accept", "application/json")
        .json(&serde_json::json!({"statement": format!("SELECT CURRENT_USER(){checks}"), "timeout": 30}))
        .send()
        .await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "caller lookup: HTTP {status}: {}",
        body["message"]
    );
    let row = &body["data"][0];
    let user = row[0].as_str().unwrap_or_default().to_string();
    anyhow::ensure!(!user.is_empty(), "caller lookup returned no user");
    // The SQL API returns every value as a string.
    let held = roles
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            row[i + 1]
                .as_str()
                .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        })
        .map(|(_, r)| (*r).clone())
        .collect();
    Ok((user, held))
}

fn env(k: &str) -> Result<String> {
    std::env::var(k).with_context(|| format!("{k} is not set"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_catalog_is_opened_again_before_its_token_runs_out() {
        use std::time::Duration;
        let fresh = Duration::from_secs(45 * 60);
        // Unknown lifetime: the fixed window.
        assert_eq!(super::reopen_after(fresh, None), fresh);
        // A fresh hour-long token: the fixed window still comes first.
        assert_eq!(
            super::reopen_after(fresh, Some(Duration::from_secs(3600))),
            fresh
        );
        // A cached token with 11 minutes left: two minutes before it runs out.
        assert_eq!(
            super::reopen_after(fresh, Some(Duration::from_secs(11 * 60))),
            Duration::from_secs(9 * 60)
        );
        // Under two minutes left: opened again on the next call.
        assert_eq!(
            super::reopen_after(fresh, Some(Duration::from_secs(90))),
            Duration::ZERO
        );
    }

    use super::google_caller;

    fn stripped(claims: serde_json::Value) -> String {
        use base64::Engine;
        let b = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.SIGNATURE_REMOVED_BY_GOOGLE",
            b(b"{}"),
            b(claims.to_string().as_bytes())
        )
    }

    #[tokio::test]
    async fn cloud_run_verified_tokens_name_their_email_and_fail_closed() {
        let ok = |exp| {
            serde_json::json!({"iss": "https://accounts.google.com", "exp": exp, "sub": "42",
                                          "email": "ana@x.com", "email_verified": true})
        };
        let who = google_caller(&stripped(ok(u64::MAX / 2)), true, &[])
            .await
            .unwrap();
        assert_eq!(who, (Some("ana@x.com".into()), "42".into()));
        assert!(
            google_caller(&stripped(ok(1)), true, &[]).await.is_err(),
            "expired"
        );
        assert!(
            google_caller(&stripped(ok(u64::MAX / 2)), false, &[])
                .await
                .is_err(),
            "IAM not confirmed"
        );
        let mut other = ok(u64::MAX / 2);
        other["iss"] = "https://evil.example".into();
        assert!(
            google_caller(&stripped(other), true, &[]).await.is_err(),
            "foreign issuer"
        );
        let mut unverified = ok(u64::MAX / 2);
        unverified["email_verified"] = false.into();
        let who = google_caller(&stripped(unverified), true, &[])
            .await
            .unwrap();
        assert_eq!(who, (None, "42".into()), "an unverified email names no one");
    }
}
