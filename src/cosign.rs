//! Cosign image signatures, as the deployment agent checks them before it runs an image: the classic
//! cosign v2 format, signed with a key (ECDSA P-256 over SHA-256, a Cloud KMS `EC_SIGN_P256_SHA256` key)
//! and `--tlog-upload=false`, so no Rekor and no certificates.
//!
//! The signatures of `<repository>@sha256:<hex>` are an OCI image manifest tagged `sha256-<hex>.sig` in
//! the same repository, or in the one `COSIGN_REPOSITORY` named when they were signed (a [`Registry`]
//! is the repository the signatures are in). Each of its layers is a "simple signing" payload (JSON naming the digest it is
//! for), and carries its signature, base64, in the annotation `dev.cosignproject.cosign/signature`. An
//! image is signed when at least one layer's blob is the one its descriptor names, verifies with the
//! pinned key, and is a container image signature for exactly that digest in exactly that repository.
//! (cosign v2 writes `critical.identity.docker-reference` as the image's repository name alone, with
//! no tag or digest, even when signing by tag or with `COSIGN_REPOSITORY`; only
//! `--sign-container-identity` writes something else, and such a signature is refused.)
//!
//! The registry is read over the OCI distribution API with the agent's own small client: a bearer token
//! from the GKE metadata server (Artifact Registry, GCR), the anonymous token handshake (a public
//! registry), or nothing.

use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::pkcs8::DecodePublicKey;
use serde_json::Value;

use crate::deployment;

/// The annotation a signature layer carries its signature in.
pub const SIGNATURE_ANNOTATION: &str = "dev.cosignproject.cosign/signature";
/// What a simple signing payload's `critical.type` says.
pub const SIGNATURE_TYPE: &str = "cosign container image signature";

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";
/// A signature manifest and a payload are a few kilobytes; these bound what a registry can make us read.
const MAX_MANIFEST: usize = 1 << 20;
const MAX_BLOB: usize = 256 << 10;
const MAX_LAYERS: usize = 32;

/// The pinned public key image signatures must verify with.
#[derive(Clone, Debug)]
pub struct PublicKey(VerifyingKey);

impl PublicKey {
    /// A PEM `PUBLIC KEY` (SubjectPublicKeyInfo) of a P-256 key, as `gcloud kms keys versions
    /// get-public-key` and `cosign generate-key-pair` write it.
    pub fn from_pem(pem: &str) -> Result<Self> {
        VerifyingKey::from_public_key_pem(pem.trim())
            .map(PublicKey)
            .map_err(|_| anyhow::anyhow!("not a PEM P-256 public key"))
    }

    pub fn from_pem_file(path: &std::path::Path) -> Result<Self> {
        let pem = std::fs::read_to_string(path)
            .with_context(|| format!("the cosign key {}", path.display()))?;
        Self::from_pem(&pem).with_context(|| format!("the cosign key {}", path.display()))
    }

    /// The SHA-256 of the key's SEC1 point, hex, for an operator to compare.
    pub fn fingerprint(&self) -> String {
        use sha2::Digest;
        deployment::hex(&sha2::Sha256::digest(
            self.0.to_encoded_point(false).as_bytes(),
        ))
    }

    /// Whether `sig` (base64 of DER, or of the raw 64-byte `r || s`) is this key's over `message`.
    fn verifies(&self, message: &[u8], sig_b64: &str) -> bool {
        let Ok(bytes) = B64.decode(sig_b64.trim()) else {
            return false;
        };
        let sig = Signature::from_der(&bytes).or_else(|_| Signature::from_slice(&bytes));
        sig.is_ok_and(|s| self.0.verify(message, &s).is_ok())
    }
}

/// Why an image isn't taken as signed.
#[derive(Debug)]
pub enum Refusal {
    /// The repository has no `sha256-<hex>.sig` for the digest.
    Missing,
    /// There are signatures, but none is the pinned key's over a payload for exactly this digest.
    Invalid,
    /// The registry couldn't be read (down, refused our credentials): nothing is known either way.
    Unreachable(anyhow::Error),
}

impl Refusal {
    /// The code the agent reports.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Missing => "signature_missing",
            Refusal::Invalid => "signature_invalid",
            Refusal::Unreachable(_) => "registry_unreachable",
        }
    }
}

/// How the agent authenticates to the registry (`AMPUP_MEMORY_AGENT_REGISTRY_AUTH`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryAuth {
    /// An access token for the pod's Google service account from the GKE metadata server, as a bearer
    /// token (and as the password of the token handshake, if the registry asks for one).
    GcpMetadata,
    /// The registry's token handshake with no credentials: a public repository.
    Anonymous,
    /// No authorization at all.
    None,
}

impl RegistryAuth {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "gcp-metadata" => Ok(RegistryAuth::GcpMetadata),
            "anonymous" => Ok(RegistryAuth::Anonymous),
            "none" => Ok(RegistryAuth::None),
            _ => anyhow::bail!("registry auth is gcp-metadata, anonymous or none, not {s}"),
        }
    }

    /// The default for a registry host: the metadata server's token for Artifact Registry and GCR,
    /// the anonymous handshake for any other.
    pub fn default_for(host: &str) -> Self {
        if host.ends_with("-docker.pkg.dev") || host == "gcr.io" || host.ends_with(".gcr.io") {
            RegistryAuth::GcpMetadata
        } else {
            RegistryAuth::Anonymous
        }
    }
}

/// An image repository's host and path: `us-docker.pkg.dev/acme/memstore/engine` is the host
/// `us-docker.pkg.dev` and the path `acme/memstore/engine`. A name with no registry host is Docker Hub's.
pub fn split_repository(repository: &str) -> (String, String) {
    match repository.split_once('/') {
        Some((h, rest)) if h.contains('.') || h.contains(':') || h == "localhost" => {
            let h = if h == "docker.io" {
                "registry-1.docker.io"
            } else {
                h
            };
            let rest = if h == "registry-1.docker.io" && !rest.contains('/') {
                format!("library/{rest}")
            } else {
                rest.to_string()
            };
            (h.to_string(), rest)
        }
        _ => {
            let rest = if repository.contains('/') {
                repository.to_string()
            } else {
                format!("library/{repository}")
            };
            ("registry-1.docker.io".into(), rest)
        }
    }
}

/// One repository on one registry, read with the OCI distribution API.
pub struct Registry {
    /// `https://<host>`, or another endpoint for the same registry (https, or http to this machine).
    api: String,
    path: String,
    auth: RegistryAuth,
    /// The GCE metadata server's host (`GCE_METADATA_HOST`, else `metadata.google.internal`).
    metadata_host: String,
    /// An access token to use in the metadata server's place (`memstore preflight --installed`, from gcloud).
    token: Option<String>,
    client: reqwest::Client,
}

/// The authorization the requests of one verification carry, found once.
#[derive(Default)]
struct Session {
    /// The metadata server's access token. Never logged.
    gcp: Option<String>,
    /// The bearer token sent: the access token, or the one the handshake gave. Never logged.
    bearer: Option<String>,
    handshook: bool,
}

impl Registry {
    /// `repository`, reached at `api` when given (else `https://` its host), authenticated by `auth`
    /// (else [`RegistryAuth::default_for`] its host).
    pub fn new(
        repository: &str,
        api: Option<&str>,
        auth: Option<RegistryAuth>,
        metadata_host: Option<String>,
    ) -> Result<Self> {
        let (host, path) = split_repository(repository);
        let api = match api {
            Some(a) => deployment::checked_url(a)?,
            None => format!("https://{host}"),
        };
        Ok(Registry {
            api,
            path,
            auth: auth.unwrap_or_else(|| RegistryAuth::default_for(&host)),
            metadata_host: metadata_host
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "metadata.google.internal".into()),
            token: None,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .build()?,
        })
    }

    pub fn auth(&self) -> RegistryAuth {
        self.auth
    }

    /// Reads the registry with this access token where [`RegistryAuth::GcpMetadata`] would ask the metadata
    /// server: for a person's shell, with gcloud's token. `None` changes nothing.
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    /// Whether the image `image_repository@digest` (`sha256:<hex>`) carries a signature by `key` among
    /// this repository's signatures.
    pub async fn verify(
        &self,
        image_repository: &str,
        digest: &str,
        key: &PublicKey,
    ) -> std::result::Result<(), Refusal> {
        let hex = digest
            .strip_prefix("sha256:")
            .filter(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit()))
            .ok_or(Refusal::Invalid)?;
        let mut s = Session::default();
        if self.auth == RegistryAuth::GcpMetadata {
            let t = self.metadata_token().await.map_err(Refusal::Unreachable)?;
            s.gcp = Some(t.clone());
            s.bearer = Some(t);
        }
        let tag = format!("sha256-{hex}.sig");
        let (status, manifest) = self
            .get(
                &mut s,
                &format!("manifests/{tag}"),
                Some(MANIFEST_ACCEPT),
                MAX_MANIFEST,
            )
            .await
            .map_err(Refusal::Unreachable)?;
        match status {
            200 => {}
            404 => return Err(Refusal::Missing),
            _ => {
                return Err(Refusal::Unreachable(anyhow::anyhow!(
                    "the registry answered {status} for {tag}"
                )));
            }
        }
        let manifest: Value = serde_json::from_slice(&manifest).map_err(|_| Refusal::Invalid)?;
        let layers = manifest["layers"].as_array().ok_or(Refusal::Invalid)?;
        for layer in layers.iter().take(MAX_LAYERS) {
            let (Some(blob_digest), Some(sig)) = (
                layer["digest"].as_str(),
                layer["annotations"][SIGNATURE_ANNOTATION].as_str(),
            ) else {
                continue;
            };
            if !blob_digest
                .strip_prefix("sha256:")
                .is_some_and(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit()))
            {
                continue;
            }
            let (status, blob) = self
                .get(&mut s, &format!("blobs/{blob_digest}"), None, MAX_BLOB)
                .await
                .map_err(Refusal::Unreachable)?;
            if status != 200 {
                continue;
            }
            if check_payload(key, image_repository, digest, blob_digest, &blob, sig) {
                return Ok(());
            }
        }
        Err(Refusal::Invalid)
    }

    async fn metadata_token(&self) -> Result<String> {
        if let Some(t) = &self.token {
            return Ok(t.clone());
        }
        let url = format!(
            "http://{}/computeMetadata/v1/instance/service-accounts/default/token",
            self.metadata_host
        );
        let resp = self
            .client
            .get(&url)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .context("the GCE metadata server")?;
        let status = resp.status();
        anyhow::ensure!(
            status.is_success(),
            "the GCE metadata server answered {status} for an access token"
        );
        let v: Value = resp.json().await.context("the metadata server's token")?;
        v["access_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(String::from)
            .context("the metadata server gave no access token")
    }

    /// `GET /v2/<path>/<what>`: its status and body (at most `max` bytes). A 401 with a bearer
    /// challenge is answered with the token handshake once, then the request is made again.
    async fn get(
        &self,
        s: &mut Session,
        what: &str,
        accept: Option<&str>,
        max: usize,
    ) -> Result<(u16, Vec<u8>)> {
        let url = format!("{}/v2/{}/{what}", self.api, self.path);
        loop {
            let mut req = self.client.get(&url);
            if let Some(a) = accept {
                req = req.header("accept", a);
            }
            if let Some(t) = &s.bearer {
                req = req.bearer_auth(t);
            }
            let resp = req.send().await.with_context(|| url.clone())?;
            let status = resp.status().as_u16();
            if status == 401 && self.auth != RegistryAuth::None && !s.handshook {
                let challenge = resp
                    .headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                s.handshook = true;
                s.bearer = Some(self.handshake(s, &challenge).await?);
                continue;
            }
            return Ok((status, read_capped(resp, max).await?));
        }
    }

    /// The registry's token endpoint, as its `WWW-Authenticate: Bearer realm=…,service=…,scope=…`
    /// names it: a pull token for this repository, with the metadata server's token as the password
    /// when there is one.
    async fn handshake(&self, s: &Session, challenge: &str) -> Result<String> {
        let params = parse_challenge(challenge).context("the registry refused us (401)")?;
        let realm = params
            .iter()
            .find(|(k, _)| k == "realm")
            .map(|(_, v)| v.as_str())
            .context("the registry's challenge names no realm")?;
        let realm = deployment::checked_url(realm)?;
        let mut query: Vec<(&str, String)> = vec![];
        if let Some((_, v)) = params.iter().find(|(k, _)| k == "service") {
            query.push(("service", v.clone()));
        }
        let scope = params
            .iter()
            .find(|(k, _)| k == "scope")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| format!("repository:{}:pull", self.path));
        query.push(("scope", scope));
        let mut req = self.client.get(&realm).query(&query);
        if let Some(t) = &s.gcp {
            req = req.basic_auth("oauth2accesstoken", Some(t));
        }
        let resp = req.send().await.context("the registry's token endpoint")?;
        let status = resp.status();
        anyhow::ensure!(
            status.is_success(),
            "the registry's token endpoint answered {status}"
        );
        let v: Value = resp.json().await.context("the registry's token")?;
        v["token"]
            .as_str()
            .or_else(|| v["access_token"].as_str())
            .filter(|t| !t.is_empty())
            .map(String::from)
            .context("the registry's token endpoint gave no token")
    }
}

/// `Bearer realm="…",service="…",scope="…"` as its parameters; None for any other scheme.
fn parse_challenge(h: &str) -> Option<Vec<(String, String)>> {
    let (scheme, rest) = h.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let mut out = vec![];
    let mut rest = rest.trim();
    while !rest.is_empty() {
        let (k, after) = rest.split_once('=')?;
        let after = after.trim_start();
        let (v, next) = if let Some(q) = after.strip_prefix('"') {
            let end = q.find('"')?;
            (&q[..end], q[end + 1..].trim_start())
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after[..end].trim(), &after[end..])
        };
        out.push((k.trim().to_ascii_lowercase(), v.to_string()));
        rest = next.trim_start_matches(',').trim_start();
    }
    Some(out)
}

async fn read_capped(mut resp: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        anyhow::ensure!(
            out.len() + chunk.len() <= max,
            "the registry's answer is over {max} bytes"
        );
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// One signature layer: `blob` is the one `blob_digest` names, `sig` is `key`'s over it, and it is a
/// container image signature for exactly `repository@digest`.
fn check_payload(
    key: &PublicKey,
    repository: &str,
    digest: &str,
    blob_digest: &str,
    blob: &[u8],
    sig: &str,
) -> bool {
    use sha2::Digest;
    let actual = format!("sha256:{}", deployment::hex(&sha2::Sha256::digest(blob)));
    if actual != blob_digest || !key.verifies(blob, sig) {
        return false;
    }
    let Ok(p) = serde_json::from_slice::<Value>(blob) else {
        return false;
    };
    p["critical"]["type"] == SIGNATURE_TYPE
        && p["critical"]["image"]["docker-manifest-digest"] == digest
        && p["critical"]["identity"]["docker-reference"] == repository
}
