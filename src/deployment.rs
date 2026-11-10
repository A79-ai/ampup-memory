//! A deployment's identity with its control plane.
//!
//! Enrollment: our operator mints a one-time token for a new deployment (`memstore control-plane
//! enroll`); the customer's install runs `memstore deployment enroll <url> <token>`, which makes an
//! Ed25519 key here, sends only its public half, and gets the deployment's id back. The private key
//! never leaves the customer's account, and the control plane holds nothing that can act as the
//! deployment or reach into it: every connection is the deployment's, outbound.
//!
//! Every later request is signed: method, path, a timestamp and the body's sha256, so a captured
//! request can't be replayed outside a few minutes or with another body. `rotate` swaps the key
//! (signed with the old one); the control plane can revoke a deployment, after which its signatures
//! are refused.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::telemetry::DeploymentId;

/// The headers a signed request carries.
pub const ID_HEADER: &str = "x-memstore-deployment";
pub const TS_HEADER: &str = "x-memstore-timestamp";
pub const SIG_HEADER: &str = "x-memstore-signature";

/// How old a signed request may be, and how far ahead of the receiver's clock.
pub const MAX_AGE_SECS: u64 = 300;
pub const MAX_AHEAD_SECS: i64 = 30;

/// What a deployment keeps: where its control plane is, its id, and its private key.
#[derive(Serialize, Deserialize)]
pub struct Credential {
    pub control_plane: String,
    pub deployment: DeploymentId,
    /// The Ed25519 private key, base64.
    secret_key: String,
    /// The control plane's own public key (base64), pinned at enrollment: what its desired state and
    /// actions are signed with ([`crate::agent`]). Absent in a credential from before it was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_plane_key: Option<String>,
}

impl Credential {
    /// `AMPUP_MEMORY_DEPLOYMENT_CREDENTIAL`, else `~/.ampup-memory/deployment.json`.
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("AMPUP_MEMORY_DEPLOYMENT_CREDENTIAL") {
            return p.into();
        }
        crate::naming::current_state_dir().join("deployment.json")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("deployment credential {}", path.display()))?;
        let c: Credential = serde_json::from_str(&text)
            .with_context(|| format!("deployment credential {}", path.display()))?;
        c.key()?;
        Ok(c)
    }

    /// Written readable by its owner only, through a temporary file so a crash leaves the old one.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            use std::io::Write;
            let mut f = open_private(&tmp)?;
            f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// A secret of this deployment's own for `purpose`, derived from its key: what it salts the ids it
    /// sends with, so the control plane can't test a guess against one (`crate::telemetry::NoteId`). It
    /// changes when the key is rotated.
    pub fn salt(&self, purpose: &str) -> Result<[u8; 32]> {
        use sha2::Digest;
        let key = self.key()?;
        let mut h = sha2::Sha256::new();
        h.update(b"memstore salt\0");
        h.update(purpose.as_bytes());
        h.update(b"\0");
        h.update(key.to_bytes());
        Ok(h.finalize().into())
    }

    fn key(&self) -> Result<SigningKey> {
        let bytes: [u8; 32] = B64
            .decode(&self.secret_key)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("a deployment key is 32 bytes"))?;
        Ok(SigningKey::from_bytes(&bytes))
    }

    /// The headers that sign a request: `method` and `path` as sent, `body` as sent.
    pub fn sign(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<Vec<(&'static str, String)>> {
        let ts = chrono::Utc::now().timestamp();
        let sig = self
            .key()?
            .sign(signed_text(&self.deployment, method, path, ts, body).as_bytes());
        Ok(vec![
            (ID_HEADER, String::from(self.deployment.clone())),
            (TS_HEADER, ts.to_string()),
            (SIG_HEADER, B64.encode(sig.to_bytes())),
        ])
    }

    /// A signed request to the control plane: its status and JSON answer.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value)> {
        let bytes = serde_json::to_vec(body)?;
        let url = format!("{}{path}", checked_url(&self.control_plane)?);
        let mut req = client()
            .request(method.parse()?, &url)
            .header("content-type", "application/json")
            .body(bytes.clone());
        for (k, v) in self.sign(method, path, &bytes)? {
            req = req.header(k, v);
        }
        let resp = req.send().await.with_context(|| url.clone())?;
        let status = resp.status().as_u16();
        Ok((status, resp.json().await.unwrap_or_default()))
    }
}

/// Bounded: a control plane that accepts and then stalls can't hang the exporter.
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("an http client")
}

pub(crate) fn open_private(path: &Path) -> Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    Ok(o.open(path)?)
}

/// What a signature covers: the deployment it speaks for, so a request can't be replayed as another.
fn signed_text(id: &DeploymentId, method: &str, path: &str, ts: i64, body: &[u8]) -> String {
    format!(
        "{}\n{method}\n{path}\n{ts}\n{}",
        id.as_str(),
        hex(&Sha256::digest(body))
    )
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// What a new key signs to prove it is held: enrolling with a token, or replacing a deployment's key.
pub fn possession_text(what: &str, of: &str) -> String {
    format!("memstore-{what}\n{of}")
}

/// A signature by `secret` (base64) over `text`, base64.
pub fn sign_with(secret: &str, text: &str) -> Result<String> {
    let bytes: [u8; 32] = B64
        .decode(secret)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("a key is 32 bytes"))?;
    Ok(B64.encode(
        SigningKey::from_bytes(&bytes)
            .sign(text.as_bytes())
            .to_bytes(),
    ))
}

/// Check that `public` (base64) signed `text` (`proof`, base64).
pub fn check_possession(public: &str, text: &str, proof: &str) -> Result<()> {
    let sig: [u8; 64] = B64
        .decode(proof)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("a signature is 64 bytes"))?;
    public_key(public)?
        .verify_strict(text.as_bytes(), &ed25519_dalek::Signature::from_bytes(&sig))
        .map_err(|_| anyhow::anyhow!("the new key's proof doesn't verify"))
}

/// A control-plane URL: https, or http to this machine only.
pub(crate) fn checked_url(u: &str) -> Result<String> {
    let url = url::Url::parse(u).with_context(|| format!("control plane URL {u}"))?;
    let local = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    anyhow::ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && local),
        "the control plane must be https (http only to this machine): {u}"
    );
    Ok(u.trim_end_matches('/').into())
}

/// A fresh key pair: the private half to keep, the public half (base64) to send.
pub fn new_key() -> (String, String) {
    let k = SigningKey::generate(&mut rand::rngs::OsRng);
    (
        B64.encode(k.to_bytes()),
        B64.encode(k.verifying_key().to_bytes()),
    )
}

/// A base64 public key, checked.
pub fn public_key(b64: &str) -> Result<VerifyingKey> {
    let bytes: [u8; 32] = B64
        .decode(b64)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("a public key is 32 bytes"))?;
    let k = VerifyingKey::from_bytes(&bytes)?;
    anyhow::ensure!(!k.is_weak(), "a weak public key");
    Ok(k)
}

/// Check a signed request's signature with `key` (base64), at `now` (unix seconds).
#[allow(clippy::too_many_arguments)]
pub fn verify(
    id: &DeploymentId,
    key: &str,
    method: &str,
    path: &str,
    ts: &str,
    sig: &str,
    body: &[u8],
    now: i64,
) -> Result<()> {
    let ts: i64 = ts.parse().context("timestamp")?;
    anyhow::ensure!(
        ts <= now.saturating_add(MAX_AHEAD_SECS) && (ts >= now || now.abs_diff(ts) <= MAX_AGE_SECS),
        "timestamp more than {MAX_AGE_SECS}s old or {MAX_AHEAD_SECS}s ahead"
    );
    let sig: [u8; 64] = B64
        .decode(sig)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("a signature is 64 bytes"))?;
    public_key(key)?
        .verify_strict(
            signed_text(id, method, path, ts, body).as_bytes(),
            &ed25519_dalek::Signature::from_bytes(&sig),
        )
        .map_err(|_| anyhow::anyhow!("bad signature"))
}

/// `memstore deployment enroll`: trade a one-time token for an id, keeping the new private key in `out`.
pub async fn enroll(control_plane: &str, token: &str, out: &Path) -> Result<Credential> {
    anyhow::ensure!(
        !out.exists(),
        "{} exists: this install is enrolled already (remove it to enroll again)",
        out.display()
    );
    let control_plane = checked_url(control_plane)?;
    let (secret, public) = new_key();
    let proof = sign_with(&secret, &possession_text("enroll", token))?;
    let url = format!("{control_plane}/v1/enroll");
    let resp = client()
        .post(&url)
        .json(&serde_json::json!({"token": token, "public_key": public, "proof": proof}))
        .send()
        .await
        .with_context(|| url.clone())?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "enroll refused ({status}): {}",
        body["error"]
    );
    let id = body["deployment"]
        .as_str()
        .context("no deployment in the answer")?;
    // Pinned now, over the connection the one-time token was good on; a key that doesn't parse is refused.
    let control_plane_key = body["control_plane_key"]
        .as_str()
        .map(|k| public_key(k).map(|_| k.to_string()))
        .transpose()
        .context("the control plane's key")?;
    let c = Credential {
        control_plane,
        deployment: DeploymentId::try_from(id.to_string())?,
        secret_key: secret,
        control_plane_key,
    };
    c.save(out)?;
    Ok(c)
}

/// `memstore deployment rotate`: a new key, registered with a request the old one signs.
///
/// The new key is written beside the credential (`.rotating`) before the control plane hears of it,
/// so a crash after it accepts the key can't lose the only copy.
pub async fn rotate(path: &Path) -> Result<()> {
    let pending = path.with_extension("rotating");
    anyhow::ensure!(
        !pending.exists(),
        "an earlier rotation stopped part way: {} holds its new key. If `ampup-memory deployment whoami --credential {}` answers, move it over {}; otherwise remove it",
        pending.display(),
        pending.display(),
        path.display()
    );
    let mut c = Credential::load(path)?;
    let (secret, public) = new_key();
    let proof = sign_with(&secret, &possession_text("rotate", c.deployment.as_str()))?;
    let old = std::mem::replace(&mut c.secret_key, secret);
    c.save(&pending)?;
    c.secret_key = old;
    let (status, body) = c
        .call(
            "POST",
            "/v1/rotate",
            &serde_json::json!({"public_key": public, "proof": proof}),
        )
        .await?;
    anyhow::ensure!(
        status == 200,
        "rotate refused ({status}): {}",
        body["error"]
    );
    std::fs::rename(&pending, path)?;
    Ok(())
}
