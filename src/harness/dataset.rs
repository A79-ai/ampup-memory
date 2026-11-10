//! Datasets in GCS, fetched when needed: corpora, eval inputs and extraction caches live at
//! `<root>/<name>/<version>/` with a `manifest.json` (each file's path, size and SHA-256) and
//! `<root>/<name>/LATEST` naming the newest version. `get` downloads what is missing or differs and
//! checks every hash; `put` uploads a directory as a new version. The root is `AMPUP_MEMORY_DATASETS`
//! (required, e.g. `gs://example-bucket/datasets`); access is the caller's Google credentials,
//! so a private bucket keeps internal corpora internal.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::Digest;

#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub files: Vec<Entry>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

struct Root {
    bucket: String,
    prefix: String,
}

fn root() -> Result<Root> {
    let r = std::env::var("AMPUP_MEMORY_DATASETS")
        .context("set AMPUP_MEMORY_DATASETS to the datasets root, gs://bucket/prefix")?;
    let rest = r
        .strip_prefix("gs://")
        .context("AMPUP_MEMORY_DATASETS must be gs://bucket/prefix")?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    Ok(Root {
        bucket: bucket.into(),
        prefix: prefix.trim_end_matches('/').into(),
    })
}

fn object(r: &Root, rel: &str) -> String {
    if r.prefix.is_empty() {
        rel.into()
    } else {
        format!("{}/{rel}", r.prefix)
    }
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn fetch(client: &reqwest::Client, r: &Root, rel: &str) -> Result<Option<bytes::Bytes>> {
    let url = format!(
        "https://storage.googleapis.com/storage/v1/b/{}/o/{}?alt=media",
        r.bucket,
        enc(&object(r, rel))
    );
    let resp = client
        .get(url)
        .bearer_auth(crate::platform::google_token()?)
        .send()
        .await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    ensure!(
        resp.status().is_success(),
        "GET gs://{}/{}: {}",
        r.bucket,
        object(r, rel),
        resp.status()
    );
    Ok(Some(resp.bytes().await?))
}

async fn store(client: &reqwest::Client, r: &Root, rel: &str, body: Vec<u8>) -> Result<()> {
    let url = format!(
        "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=media&name={}",
        r.bucket,
        enc(&object(r, rel))
    );
    let resp = client
        .post(url)
        .bearer_auth(crate::platform::google_token()?)
        .body(body)
        .send()
        .await?;
    ensure!(
        resp.status().is_success(),
        "upload gs://{}/{}: {}",
        r.bucket,
        object(r, rel),
        resp.status()
    );
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() {
            walk(&p, base, out)?;
        } else {
            out.push(p.strip_prefix(base)?.to_path_buf());
        }
    }
    Ok(())
}

/// Every dataset under the root, with its latest version.
pub async fn list() -> Result<Vec<(String, String)>> {
    let r = root()?;
    let client = reqwest::Client::new();
    let prefix = if r.prefix.is_empty() {
        String::new()
    } else {
        format!("{}/", r.prefix)
    };
    let url = format!(
        "https://storage.googleapis.com/storage/v1/b/{}/o?prefix={}&delimiter=%2F",
        r.bucket,
        enc(&prefix)
    );
    let v: serde_json::Value = client
        .get(url)
        .bearer_auth(crate::platform::google_token()?)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut out = vec![];
    for p in v["prefixes"].as_array().into_iter().flatten() {
        let name = p
            .as_str()
            .unwrap_or_default()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        let latest = fetch(&client, &r, &format!("{name}/LATEST")).await?;
        let latest = latest
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .unwrap_or_default();
        out.push((name.to_string(), latest));
    }
    Ok(out)
}

/// Download `name` (or `name@version`) into `dir`: files already there with the right hash are
/// kept, the rest fetched and checked. Returns (fetched, kept).
pub async fn get(spec: &str, dir: &Path) -> Result<(usize, usize)> {
    let r = root()?;
    let client = reqwest::Client::new();
    let (name, version) = match spec.split_once('@') {
        Some((n, v)) => (n.to_string(), v.to_string()),
        None => {
            let latest = fetch(&client, &r, &format!("{spec}/LATEST"))
                .await?
                .with_context(|| {
                    format!("no dataset {spec:?} (try `ampup-memory dataset list`)")
                })?;
            (
                spec.to_string(),
                String::from_utf8_lossy(&latest).trim().to_string(),
            )
        }
    };
    let m: Manifest = serde_json::from_slice(
        &fetch(&client, &r, &format!("{name}/{version}/manifest.json"))
            .await?
            .with_context(|| format!("no manifest for {name}@{version}"))?,
    )?;
    std::fs::create_dir_all(dir)?;
    let (mut fetched, mut kept) = (0, 0);
    for f in &m.files {
        // Only plain relative parts: no root, drive, `..` or `.`, so a manifest cannot write
        // outside `dir`.
        ensure!(
            !f.path.is_empty()
                && Path::new(&f.path)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "manifest path {:?} leaves the dataset",
            f.path
        );
        let dest = dir.join(&f.path);
        if std::fs::read(&dest).is_ok_and(|b| b.len() as u64 == f.size && sha256(&b) == f.sha256) {
            kept += 1;
            continue;
        }
        let body = fetch(&client, &r, &format!("{name}/{version}/files/{}", f.path))
            .await?
            .with_context(|| format!("missing file {} in {name}@{version}", f.path))?;
        if sha256(&body) != f.sha256 {
            bail!(
                "{} from {name}@{version} does not match its manifest hash",
                f.path
            );
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &body)?;
        fetched += 1;
    }
    Ok((fetched, kept))
}

/// Upload `dir` as `name@version` and point LATEST at it.
pub async fn put(name: &str, dir: &Path, version: &str, description: &str) -> Result<usize> {
    let r = root()?;
    let client = reqwest::Client::new();
    let mut paths = vec![];
    walk(dir, dir, &mut paths)?;
    paths.sort();
    let mut files = vec![];
    for p in &paths {
        let body = std::fs::read(dir.join(p))?;
        let path = p.to_string_lossy().replace('\\', "/");
        files.push(Entry {
            path: path.clone(),
            size: body.len() as u64,
            sha256: sha256(&body),
        });
        store(&client, &r, &format!("{name}/{version}/files/{path}"), body).await?;
    }
    let m = Manifest {
        name: name.into(),
        version: version.into(),
        description: description.into(),
        files,
    };
    store(
        &client,
        &r,
        &format!("{name}/{version}/manifest.json"),
        serde_json::to_vec_pretty(&m)?,
    )
    .await?;
    store(
        &client,
        &r,
        &format!("{name}/LATEST"),
        version.as_bytes().to_vec(),
    )
    .await?;
    Ok(m.files.len())
}
