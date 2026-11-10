//! People's sign-in: an identity provider's ID tokens (OpenID Connect).

use anyhow::{Context, Result};

use super::fetch::FetchPolicy;

/// An identity provider's ID tokens (OpenID Connect): what people sign in with, through
/// `memstore auth login` or an MCP client's OAuth flow. The provider's signing keys come from its
/// discovery document and are re-read when a token names a key not seen yet (keys rotate).
pub struct OidcConfig {
    pub issuer: String,
    /// The client ids tokens may be issued to (the CLI's, Claude's, ...).
    pub audiences: Vec<String>,
    pub jwks_uri: String,
    keys: tokio::sync::RwLock<(Option<std::time::Instant>, jsonwebtoken::jwk::JwkSet)>,
    /// Keys this old are re-read on the next token, so a key the issuer retired stops verifying.
    ttl: std::time::Duration,
    /// How its keys are read: as they are (the operator's provider), or vetted (an org admin's).
    fetch: FetchPolicy,
}

/// The keys of an issuer are re-read at most this often, so unknown-key tokens can't hammer it.
const JWKS_MIN_REFRESH: std::time::Duration = std::time::Duration::from_secs(60);
/// How long an issuer's keys are trusted before they are re-read.
const JWKS_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

impl OidcConfig {
    /// Reads `<issuer>/.well-known/openid-configuration`.
    pub async fn discover(issuer: &str, audiences: Vec<String>) -> Result<Self> {
        anyhow::ensure!(
            !audiences.is_empty(),
            "an OIDC issuer needs its client ids (MEMSTORE_OIDC_AUDIENCE)"
        );
        Self::discover_issuer(issuer, audiences, FetchPolicy::Trusted).await
    }

    /// An org's provider: its client ids come with each check ([`Self::verify_for`]). With
    /// [`FetchPolicy::Vetted`], the discovery document and the keys (whose URL the document names, maybe
    /// on another host) are each vetted, and a failure says only [`super::fetch::UNREADABLE`].
    pub async fn discover_issuer(
        issuer: &str,
        audiences: Vec<String>,
        fetch: FetchPolicy,
    ) -> Result<Self> {
        let issuer = issuer.trim_end_matches('/').to_string();
        let doc: serde_json::Value = fetch
            .get_json(
                &format!("{issuer}/.well-known/openid-configuration"),
                "OIDC discovery document",
            )
            .await?;
        let jwks_uri = match doc["jwks_uri"].as_str() {
            Some(u) => u.to_string(),
            None if fetch == FetchPolicy::Trusted => {
                anyhow::bail!("discovery document has no jwks_uri")
            }
            None => {
                eprintln!("org identity provider {issuer}: the discovery document has no jwks_uri");
                anyhow::bail!(super::fetch::UNREADABLE)
            }
        };
        Ok(OidcConfig {
            issuer,
            audiences,
            jwks_uri,
            keys: tokio::sync::RwLock::new((None, jsonwebtoken::jwk::JwkSet { keys: vec![] })),
            ttl: JWKS_TTL,
            fetch,
        })
    }

    /// Keys at a known URL, no discovery (Google's IAP publishes a JWK set and no OpenID configuration).
    pub fn with_keys(issuer: &str, jwks_uri: &str, audiences: Vec<String>) -> Self {
        OidcConfig {
            issuer: issuer.trim_end_matches('/').to_string(),
            audiences,
            jwks_uri: jwks_uri.to_string(),
            keys: tokio::sync::RwLock::new((None, jsonwebtoken::jwk::JwkSet { keys: vec![] })),
            ttl: JWKS_TTL,
            fetch: FetchPolicy::Trusted,
        }
    }

    /// Re-read the keys once they are `ttl` old (an hour by default). An unknown key re-reads them at most
    /// once a minute, or once per `ttl` if that is shorter.
    pub fn with_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Re-read the key set now; how many keys it holds.
    pub async fn fetch_keys(&self) -> Result<usize> {
        let set = self.read_keys().await?;
        let n = set.keys.len();
        *self.keys.write().await = (Some(std::time::Instant::now()), set);
        Ok(n)
    }

    /// Whether `token` claims to come from this issuer (unverified: picks the checker, nothing more).
    pub fn claims_issuer(&self, token: &str) -> bool {
        jsonwebtoken::dangerous::insecure_decode::<serde_json::Value>(token)
            .ok()
            .and_then(|t| {
                t.claims["iss"]
                    .as_str()
                    .map(|i| i.trim_end_matches('/') == self.issuer)
            })
            .unwrap_or(false)
    }

    /// The key `kid` names. The set is re-read when it is `ttl` old, or when it lacks `kid` and wasn't read
    /// in the last minute. The time of a read is kept even when it fails (and the keys held are kept), so a
    /// failing issuer is retried at that pace, not on every token.
    async fn key(&self, kid: &str) -> Result<jsonwebtoken::DecodingKey> {
        let min_refresh = JWKS_MIN_REFRESH.min(self.ttl);
        {
            let k = self.keys.read().await;
            let age = k.0.map(|at| at.elapsed());
            let fresh = age.is_some_and(|a| a < self.ttl);
            match k.1.find(kid) {
                Some(j) if fresh => return Ok(jsonwebtoken::DecodingKey::from_jwk(j)?),
                None if fresh && age.is_some_and(|a| a < min_refresh) => {
                    anyhow::bail!("unknown signing key {kid}")
                }
                _ => {}
            }
        }
        let mut k = self.keys.write().await;
        // Another request may have re-read them while this one waited.
        if k.0.is_none_or(|at| at.elapsed() >= min_refresh) {
            k.0 = Some(std::time::Instant::now());
            match self.read_keys().await {
                Ok(set) => k.1 = set,
                Err(e) => eprintln!("{}: {e:#} (keeping the keys we have)", self.jwks_uri),
            }
        }
        let j =
            k.1.find(kid)
                .with_context(|| format!("unknown signing key {kid}"))?;
        Ok(jsonwebtoken::DecodingKey::from_jwk(j)?)
    }

    pub async fn read_keys(&self) -> Result<jsonwebtoken::jwk::JwkSet> {
        self.fetch
            .get_json(&self.jwks_uri, "issuer's signing keys")
            .await
    }

    /// The user an ID token proves: its verified email.
    /// Signature, issuer, audience and expiry are checked; only asymmetric algorithms are accepted.
    pub async fn verify(&self, token: &str) -> Result<String> {
        Ok(self.verify_for(token, &self.audiences).await?.0)
    }

    /// [`Self::verify`] for tokens issued to one of `audiences`; also returns the token's claims.
    pub async fn verify_for(
        &self,
        token: &str,
        audiences: &[String],
    ) -> Result<(String, serde_json::Value)> {
        use jsonwebtoken::Algorithm::*;
        self.verify_with(
            token,
            audiences,
            &[RS256, RS384, RS512, ES256, ES384, PS256, EdDSA],
        )
        .await
    }

    /// [`Self::verify_for`] accepting only `algs` (an issuer that signs one way, like IAP's ES256, pins it).
    pub async fn verify_with(
        &self,
        token: &str,
        audiences: &[String],
        algs: &[jsonwebtoken::Algorithm],
    ) -> Result<(String, serde_json::Value)> {
        let c = self.verify_claims(token, audiences, algs).await?;
        Ok((email_of(&c)?, c))
    }

    /// The claims of a token this issuer signed for one of `audiences` with one of `algs`, unexpired. Who
    /// it names is the caller's to read ([`email_of`]): an access token for a resource carries no email.
    pub async fn verify_claims(
        &self,
        token: &str,
        audiences: &[String],
        algs: &[jsonwebtoken::Algorithm],
    ) -> Result<serde_json::Value> {
        let header = jsonwebtoken::decode_header(token).context("invalid token")?;
        anyhow::ensure!(
            algs.contains(&header.alg),
            "token signed with {:?}, not an identity provider's key",
            header.alg
        );
        let key = self.key(header.kid.as_deref().unwrap_or_default()).await?;
        let mut v = jsonwebtoken::Validation::new(header.alg);
        v.set_issuer(&[&self.issuer, &format!("{}/", self.issuer)]);
        v.set_audience(audiences);
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        Ok(jsonwebtoken::decode::<serde_json::Value>(token, &key, &v)
            .context("invalid token")?
            .claims)
    }
}

/// The verified email a token's (or a userinfo answer's) claims name, lower-cased. People are known by
/// their verified email, which is what the directory names. A subject id is never used instead: some
/// providers let it look like an email, which would let it pose as one.
pub fn email_of(c: &serde_json::Value) -> Result<String> {
    let email = c["email"]
        .as_str()
        .filter(|e| !e.is_empty())
        .context("the token carries no email (ask the provider for the email scope)")?;
    anyhow::ensure!(
        c["email_verified"] != false && c["email_verified"] != "false",
        "the token's email is not verified"
    );
    Ok(email.to_lowercase())
}

impl OidcConfig {}

/// The issuer a JWT names, unverified (only to pick who checks it).
pub fn token_issuer(token: &str) -> Option<String> {
    jsonwebtoken::dangerous::insecure_decode::<serde_json::Value>(token)
        .ok()
        .and_then(|t| {
            t.claims["iss"]
                .as_str()
                .map(|i| i.trim_end_matches('/').to_string())
        })
}

/// An org's own identity provider, as the directory holds it.
#[derive(Clone, Debug)]
pub struct OrgProvider {
    pub org: String,
    pub issuer: String,
    pub client_ids: Vec<String>,
    pub groups_claim: String,
}
