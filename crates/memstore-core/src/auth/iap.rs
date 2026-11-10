//! Google's Identity-Aware Proxy in front of Cloud Run: people sign in with Google Workspace at the proxy,
//! and each request reaches the service with `x-goog-iap-jwt-assertion`, a short-lived ES256 JWT IAP
//! signs naming the user. The service checks it against IAP's published keys and the audience IAP mints
//! for this service, so a request that didn't come through the proxy (or came for another service) is
//! refused.

use anyhow::{Context, Result};

use super::oidc::OidcConfig;

pub const HEADER: &str = "x-goog-iap-jwt-assertion";
pub const ISSUER: &str = "https://cloud.google.com/iap";
pub const KEYS_URL: &str = "https://www.gstatic.com/iap/verify/public_key-jwk";
/// IAP's own allowance for clock skew on `iat` and `exp`.
const SKEW: u64 = 30;

pub struct Iap {
    keys: OidcConfig,
}

impl Iap {
    /// `audience` is what IAP mints for this service: `/projects/<number>/locations/<region>/services/<name>`
    /// on Cloud Run. `keys_url` is IAP's JWK set (`KEYS_URL`; a stand-in in tests).
    pub fn new(audience: &str, keys_url: &str) -> Result<Self> {
        anyhow::ensure!(
            audience.starts_with("/projects/"),
            "MEMSTORE_IAP_AUDIENCE is the audience IAP mints, e.g. /projects/123/locations/us-central1/services/memstore"
        );
        Ok(Iap {
            keys: OidcConfig::with_keys(ISSUER, keys_url, vec![audience.to_string()]),
        })
    }

    /// IAP from the environment: `MEMSTORE_IAP_AUDIENCE` (unset: no IAP), `MEMSTORE_IAP_JWKS_URL` and
    /// `MEMSTORE_IAP_KEYS_TTL_SECS` (how long its keys are trusted before they are re-read; an hour).
    pub fn from_env() -> Result<Option<Self>> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let Some(aud) = var("MEMSTORE_IAP_AUDIENCE") else {
            return Ok(None);
        };
        let mut iap = Iap::new(
            &aud,
            &var("MEMSTORE_IAP_JWKS_URL").unwrap_or_else(|| KEYS_URL.into()),
        )?;
        if let Some(t) = var("MEMSTORE_IAP_KEYS_TTL_SECS") {
            let secs: u64 = t
                .parse()
                .context("MEMSTORE_IAP_KEYS_TTL_SECS is a number of seconds")?;
            iap.keys = iap
                .keys
                .with_ttl(std::time::Duration::from_secs(secs.max(1)));
        }
        Ok(Some(iap))
    }

    pub fn audience(&self) -> &str {
        &self.keys.audiences[0]
    }

    /// The user's email, once the assertion verifies: IAP's ES256 signature by a published key, IAP as
    /// issuer, this service as audience, not expired, and issued in the past.
    pub async fn verify(&self, assertion: &str) -> Result<String> {
        let (email, claims) = self
            .keys
            .verify_with(
                assertion,
                &self.keys.audiences,
                &[jsonwebtoken::Algorithm::ES256],
            )
            .await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let iat = claims["iat"].as_u64().context("assertion has no iat")?;
        anyhow::ensure!(iat <= now + SKEW, "assertion issued in the future");
        Ok(email)
    }

    /// A line for `identity check`, having fetched the keys.
    pub async fn probe(&self) -> Result<String> {
        let n = self.keys.fetch_keys().await?;
        Ok(format!(
            "{n} IAP signing keys; audience {}",
            self.audience()
        ))
    }
}
