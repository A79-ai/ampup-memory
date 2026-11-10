//! How a caller proves who they are: API keys (service callers), signed JWTs (a user's identity and
//! groups, passed on by the app) and, through [`OidcConfig`], an identity provider's ID tokens.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::Digest;

use super::acl::is_acl_id;
use super::fetch::FetchPolicy;
use super::oidc::token_issuer;
use super::{OidcConfig, OrgProvider};

#[derive(Deserialize)]
struct ApiKeyRow {
    id: String,
    /// sha256 of the key, hex. The key itself is never stored.
    sha256: String,
    user: String,
    #[serde(default)]
    groups: Vec<String>,
    /// May push closure changes (`/closure`).
    #[serde(default)]
    admin: bool,
}

/// A verified API key's caller.
pub struct KeyCaller {
    pub id: String,
    pub user: String,
    pub groups: Vec<String>,
    pub admin: bool,
    /// The key's sha256, hex.
    pub sha256: String,
}

/// Where the keys that verify tokens come from.
pub enum KeySource {
    /// An HS256 shared secret (32+ characters).
    Secret(String),
    /// One RS256 public key, as PEM.
    Pem(String),
    /// An issuer's JWKS URL (Okta, Entra, Auth0, Google): RS256 keys found by the token's `kid`, fetched on
    /// first use and refetched when a token names a key we don't hold, or the keys are an hour old.
    Jwks(String),
}

/// What tokens must carry and where their claims live. Defaults fit a token minted by AmpUp's own auth;
/// Okta, Entra and Google name things differently.
pub struct JwtOptions {
    pub issuer: String,
    pub audience: String,
    /// The org claim and the org this deployment serves. `None` for an issuer with no such claim (Okta's
    /// defaults): the deployment is then bound to its issuer and audience alone.
    pub org: Option<(String, String)>,
    /// The claim holding the caller's groups: a list, or one comma- or space-separated string.
    pub groups_claim: String,
    /// Claims tried in order for the id requests run as; the first one present wins.
    pub user_claims: Vec<String>,
    /// Refuse a token whose `email_verified` is false (an absent claim passes).
    pub require_verified_email: bool,
    /// JWKS only: the least time between refetches triggered by a token naming an unknown key.
    pub jwks_min_refetch: std::time::Duration,
}

impl JwtOptions {
    pub fn new(issuer: &str, audience: &str) -> Self {
        JwtOptions {
            issuer: issuer.into(),
            audience: audience.into(),
            org: None,
            groups_claim: "groups".into(),
            user_claims: vec!["email".into(), "sub".into()],
            require_verified_email: true,
            jwks_min_refetch: JWKS_MIN_REFETCH,
        }
    }

    pub fn with_org(mut self, claim: &str, org: &str) -> Self {
        self.org = Some((claim.into(), org.into()));
        self
    }
}

/// An issuer's published keys, by `kid`.
struct Jwks {
    url: String,
    http: reqwest::Client,
    keys: std::sync::RwLock<HashMap<String, jsonwebtoken::DecodingKey>>,
    min_refetch: std::time::Duration,
    /// When we last asked the issuer, and the lock that keeps concurrent misses to one request.
    last_fetch: tokio::sync::Mutex<Option<std::time::Instant>>,
}

/// Keys this old are refetched on the next token, so a retired key stops verifying.
const JWKS_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
/// A token naming a key we don't hold triggers a refetch, but not more often than this, so a flood of bad
/// tokens can't turn into a flood of requests to the issuer.
const JWKS_MIN_REFETCH: std::time::Duration = std::time::Duration::from_secs(30);

impl Jwks {
    /// Refetch if the keys are old, or (`missing`) if a token named a key we lack and the last fetch wasn't
    /// just now. A failed fetch keeps the keys we have.
    async fn refresh(&self, missing: bool) {
        let mut last = self.last_fetch.lock().await;
        let age = last.map(|t| t.elapsed());
        let due = match age {
            None => true,
            Some(a) => a >= JWKS_TTL || (missing && a >= self.min_refetch),
        };
        if !due {
            return;
        }
        // Stamped before the request so a failing issuer is retried at the same pace, not every token.
        *last = Some(std::time::Instant::now());
        match self.fetch().await {
            Ok(keys) => *self.keys.write().unwrap() = keys,
            Err(e) => eprintln!("jwks {}: {e:#} (keeping the keys we have)", self.url),
        }
    }

    async fn fetch(&self) -> Result<HashMap<String, jsonwebtoken::DecodingKey>> {
        let set: jsonwebtoken::jwk::JwkSet = self
            .http
            .get(&self.url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut keys = HashMap::new();
        for jwk in &set.keys {
            // Signing RSA keys with an id only: anything else can't verify an RS256 token.
            let signing = jwk
                .common
                .public_key_use
                .as_ref()
                .is_none_or(|u| matches!(u, jsonwebtoken::jwk::PublicKeyUse::Signature));
            if let (Some(kid), true, jsonwebtoken::jwk::AlgorithmParameters::RSA(_)) =
                (&jwk.common.key_id, signing, &jwk.algorithm)
            {
                keys.insert(kid.clone(), jsonwebtoken::DecodingKey::from_jwk(jwk)?);
            }
        }
        anyhow::ensure!(!keys.is_empty(), "no usable RSA signing keys");
        Ok(keys)
    }
}

enum Keys {
    Fixed(jsonwebtoken::DecodingKey),
    Jwks(Jwks),
}

/// How signed tokens are checked.
pub struct JwtConfig {
    keys: Keys,
    validation: jsonwebtoken::Validation,
    opts: JwtOptions,
}

impl JwtConfig {
    /// A line saying how tokens are checked, having fetched the JWKS if that is the key source (so a wrong
    /// URL or an unreachable issuer shows up before a user does).
    pub async fn probe(&self) -> Result<String> {
        let keys = match &self.keys {
            Keys::Fixed(_) => format!("{:?} key", self.validation.algorithms[0]),
            Keys::Jwks(j) => format!("{} RS256 keys at {}", j.fetch().await?.len(), j.url),
        };
        Ok(format!(
            "{keys}; issuer {}, audience {}, user from {}, groups from `{}`{}",
            self.opts.issuer,
            self.opts.audience,
            self.opts.user_claims.join("|"),
            self.opts.groups_claim,
            match &self.opts.org {
                Some((claim, org)) => format!(", `{claim}` must be {org}"),
                None => ", no org claim".to_string(),
            }
        ))
    }

    pub fn new(keys: KeySource, opts: JwtOptions) -> Result<Self> {
        use jsonwebtoken::{Algorithm, DecodingKey, Validation};
        // The algorithm is pinned by the key source, never read from the token: a token can't pick HS256 and
        // be "verified" with a public key as the secret.
        let (keys, alg) = match keys {
            KeySource::Secret(s) => {
                anyhow::ensure!(s.len() >= 32, "MEMSTORE_JWT_SECRET needs 32+ chars");
                (
                    Keys::Fixed(DecodingKey::from_secret(s.as_bytes())),
                    Algorithm::HS256,
                )
            }
            KeySource::Pem(p) => (
                Keys::Fixed(
                    DecodingKey::from_rsa_pem(p.as_bytes()).context("MEMSTORE_JWT_PUBLIC_KEY")?,
                ),
                Algorithm::RS256,
            ),
            KeySource::Jwks(url) => {
                anyhow::ensure!(
                    url.starts_with("https://") || url.starts_with("http://"),
                    "jwks url"
                );
                (
                    Keys::Jwks(Jwks {
                        url,
                        min_refetch: opts.jwks_min_refetch,
                        http: reqwest::Client::new(),
                        keys: Default::default(),
                        last_fetch: Default::default(),
                    }),
                    Algorithm::RS256,
                )
            }
        };
        let mut validation = Validation::new(alg);
        validation.set_issuer(&[opts.issuer.as_str()]);
        validation.set_audience(&[opts.audience.as_str()]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        Ok(JwtConfig {
            keys,
            validation,
            opts,
        })
    }

    /// The caller a valid token names. Expired, mis-issued, other-org, unverified-email and unsigned tokens
    /// are refused.
    pub async fn verify(&self, token: &str) -> Result<Caller> {
        let claims = match &self.keys {
            Keys::Fixed(k) => jsonwebtoken::decode::<serde_json::Value>(token, k, &self.validation),
            Keys::Jwks(j) => {
                let kid = jsonwebtoken::decode_header(token)
                    .context("invalid token")?
                    .kid
                    .ok_or_else(|| anyhow::anyhow!("token names no key id"))?;
                j.refresh(false).await;
                let mut key = j.keys.read().unwrap().get(&kid).cloned();
                if key.is_none() {
                    j.refresh(true).await;
                    key = j.keys.read().unwrap().get(&kid).cloned();
                }
                let key =
                    key.ok_or_else(|| anyhow::anyhow!("token is signed by an unknown key"))?;
                jsonwebtoken::decode::<serde_json::Value>(token, &key, &self.validation)
            }
        }
        .context("invalid token")?
        .claims;
        self.caller(&claims)
    }

    fn caller(&self, claims: &serde_json::Value) -> Result<Caller> {
        let text = |k: &str| {
            claims[k]
                .as_str()
                .filter(|v| !v.is_empty())
                .map(String::from)
        };
        if let Some((claim, org)) = &self.opts.org {
            anyhow::ensure!(
                text(claim).as_deref() == Some(org.as_str()),
                "token is for another org"
            );
        }
        anyhow::ensure!(
            !(self.opts.require_verified_email
                && claims["email_verified"] == serde_json::Value::Bool(false)),
            "token's email is not verified"
        );
        let user = self
            .opts
            .user_claims
            .iter()
            .find_map(|c| text(c))
            .ok_or_else(|| anyhow::anyhow!("token names no user"))?;
        // Everything the token calls this person: disabling any of them locks them out.
        let mut aliases: Vec<String> = self
            .opts
            .user_claims
            .iter()
            .filter_map(|c| text(c))
            .collect();
        aliases.extend(text("sub"));
        aliases.extend(text("email"));
        aliases.sort();
        aliases.dedup();
        let groups: Vec<String> = match &claims[self.opts.groups_claim.as_str()] {
            serde_json::Value::Array(a) => a
                .iter()
                .filter_map(|g| g.as_str().map(String::from))
                .collect(),
            serde_json::Value::String(s) => s
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|g| !g.is_empty())
                .map(String::from)
                .collect(),
            // No groups claim (an Entra token over its group limit, a Google ID token): no groups, so
            // fewer rights, never more.
            _ => vec![],
        };
        // Only groups an ACL entry can name: an IdP admin's group called `role:SALES` is not the role.
        let groups = groups.into_iter().filter(|g| is_acl_id(g)).collect();
        Ok(Caller {
            user,
            groups,
            admin: false,
            aliases,
            ..Default::default()
        })
    }
}

/// Who a token or key proves the caller is.
#[derive(Default)]
pub struct Caller {
    /// The id requests run as.
    pub user: String,
    pub groups: Vec<String>,
    pub admin: bool,
    /// Every id the credential names them by (a JWT's `sub` and email, `key:<id>` for an API key).
    pub aliases: Vec<String>,
    /// Signed in through an identity provider (a person).
    pub person: bool,
    /// Signed in through these orgs' own provider: it vouches for them in these orgs only.
    pub orgs: Option<std::collections::BTreeSet<String>>,
    /// The groups their org's provider says they're in.
    pub idp_groups: Vec<String>,
    /// The provider said, in so many words, that the email is verified (some leave it out, and some
    /// let people set an email they don't own).
    pub email_verified: bool,
    /// An org's service key (`memstore apikey create`): bound to that org, and to its team if it has one.
    pub key: Option<OrgKey>,
    /// Signed in through the platform's Auth0 tenant.
    pub auth0: bool,
    /// The Auth0 Organization that token was issued for (its `org_id`), if any.
    pub auth0_org: Option<String>,
    /// Signed in through the platform's Auth0 tenant for the operators' Organization.
    pub operator: bool,
}

/// A service account's key as the server matches bearers against it, read from the directory.
#[derive(Clone, Debug, serde::Serialize)]
pub struct OrgKey {
    pub org: String,
    pub name: String,
    pub team: Option<String>,
    /// Reads its scope (`reader`), reads and writes it (`writer`), or only writes it (`ingest`).
    pub role: crate::people::directory::KeyRole,
    /// Seconds since the epoch.
    pub expires_at: Option<i64>,
    /// An ingest key writes and retracts only sources starting with one of these; none: any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    #[serde(skip)]
    pub sha256: String,
    /// A verified import of a file key: it, and only it, takes that file key's place.
    #[serde(skip)]
    pub replaces_file_key: bool,
}

impl OrgKey {
    /// Whether this key may write `source`: it is under one of its prefixes, or it has none.
    pub fn may_write_source(&self, source: &str) -> bool {
        self.sources.is_empty() || self.sources.iter().any(|p| source.starts_with(p.as_str()))
    }

    /// The id requests made with this key run as.
    pub fn user(&self) -> String {
        format!("key:{}/{}", self.org, self.name)
    }
}

/// The caller an org's service key proves: one of `keys` (compared in constant time, every one), and
/// not expired.
pub fn org_key_caller(token: &str, keys: &[OrgKey]) -> Result<Caller> {
    let digest = hex(&sha2::Sha256::digest(token.as_bytes()));
    let mut found = vec![];
    for k in keys {
        if constant_eq(k.sha256.as_bytes(), digest.as_bytes()) {
            found.push(k);
        }
    }
    // A bearer is exactly one caller: two keys with one hash (made on two servers at once) name
    // neither, rather than whichever comes last.
    anyhow::ensure!(found.len() <= 1, "api key names {} keys", found.len());
    let k = found
        .pop()
        .ok_or_else(|| anyhow::anyhow!("unknown api key"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    if k.expires_at.is_some_and(|t| now >= t) {
        crate::metrics::EXPIRED_KEY_REFUSALS.inc();
    }
    anyhow::ensure!(
        k.expires_at.is_none_or(|t| now < t),
        "api key {} expired",
        k.user()
    );
    let user = k.user();
    Ok(Caller {
        aliases: vec![user.clone()],
        user,
        orgs: Some([k.org.clone()].into()),
        key: Some(k.clone()),
        ..Default::default()
    })
}

fn says_verified(claims: &serde_json::Value) -> bool {
    claims["email_verified"] == true || claims["email_verified"] == "true"
}

/// The platform's Auth0 tenant (`MEMSTORE_AUTH0_ISSUER`): one issuer whose ID tokens sign people in to
/// many orgs, each token to the org bound to the Auth0 Organization it was issued for (its `org_id`).
pub struct Auth0 {
    /// Its keys and issuer; `audiences` are the memstore Auth0 application's client ids.
    oidc: OidcConfig,
    /// This deployment as a resource (`<public url>/mcp`): an access token the tenant issued for it, to
    /// any client registered there (an MCP client's, through dynamic registration), also signs in.
    resources: Vec<String>,
    /// The Auth0 Organization whose members are operators (`MEMSTORE_AUTH0_OPERATORS_ORG`).
    operators: Option<String>,
}

impl Auth0 {
    /// Reads `<issuer>.well-known/openid-configuration` and its keys, vetted as an org's provider is (https
    /// on a public address, pinned; this machine too with `local`).
    pub async fn discover(
        issuer: &str,
        client_ids: Vec<String>,
        resources: Vec<String>,
        operators: Option<String>,
        local: bool,
    ) -> Result<Self> {
        anyhow::ensure!(
            !client_ids.is_empty(),
            "MEMSTORE_AUTH0_ISSUER needs the memstore application's client ids (MEMSTORE_AUTH0_CLIENT_IDS)"
        );
        if let Some(o) = &operators {
            anyhow::ensure!(
                crate::people::directory::valid_auth0_org(o),
                "MEMSTORE_AUTH0_OPERATORS_ORG {o:?} is not an Auth0 organization id (org_…)"
            );
        }
        let fetch = FetchPolicy::Vetted {
            allow_loopback: local,
        };
        let oidc = OidcConfig::discover_issuer(issuer, client_ids, fetch)
            .await
            .context("MEMSTORE_AUTH0_ISSUER")?;
        oidc.fetch_keys()
            .await
            .context("MEMSTORE_AUTH0_ISSUER's keys")?;
        Ok(Auth0 {
            oidc,
            resources,
            operators,
        })
    }

    /// Its client ids and this deployment's resources: what a token of the tenant's may be issued to.
    fn audiences(&self) -> Vec<String> {
        let mut a = self.oidc.audiences.clone();
        a.extend(self.resources.iter().cloned());
        a
    }

    /// Who an access token for this deployment is: the tenant's userinfo endpoint says, as the token
    /// itself carries no email (unless the tenant's Action stamped one, [`STAMPED_EMAIL`]).
    async fn userinfo(&self, token: &str) -> Result<serde_json::Value> {
        reqwest::Client::new()
            .get(format!("{}/userinfo", self.oidc.issuer))
            .bearer_auth(token)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .context("the tenant's userinfo endpoint")?
            .error_for_status()
            .context("the tenant's userinfo endpoint")?
            .json()
            .await
            .context("the tenant's userinfo answer")
    }

    pub fn issuer(&self) -> &str {
        &self.oidc.issuer
    }

    pub fn client_ids(&self) -> &[String] {
        &self.oidc.audiences
    }

    pub fn operators(&self) -> Option<&str> {
        self.operators.as_deref()
    }

    /// Whether `token` names one of its client ids among its audiences (unverified: picks the checker).
    fn names_client(&self, token: &str) -> bool {
        let Ok(t) = jsonwebtoken::dangerous::insecure_decode::<serde_json::Value>(token) else {
            return false;
        };
        let ids = self.audiences();
        audiences_of(&t.claims).iter().any(|a| ids.contains(a))
    }

    /// The person a token proves: RS256 only, issued by the tenant to one of the client ids, unexpired,
    /// its email verified in so many words. Bound to the org `bound` (org, Auth0 Organization) says its
    /// `org_id` is, if exactly one. A token naming no Organization, issued through one of the platform's own
    /// connections (Google, the tenant's database, passwordless), signs its holder in as the directory's
    /// member wherever it lists them: people sign in first and the directory says which orgs are theirs
    /// (`memstore join`, the plugin's `/memstore:login`). One naming an Organization no org is bound to
    /// is a person bound to no org: they sign in, and reach nothing.
    async fn caller(&self, token: &str, bound: &[(String, String)]) -> Result<Caller> {
        let mut claims = self
            .oidc
            .verify_claims(token, &self.audiences(), &[jsonwebtoken::Algorithm::RS256])
            .await?;
        // An ID token names its person; an access token for this deployment (an MCP client's) doesn't,
        // and the tenant says who holds it. Only the token's signature, issuer, audience and expiry
        // vouch for that answer, so it is asked of the tenant, never read off the token's own claims.
        let for_resource = audiences_of(&claims)
            .iter()
            .any(|a| self.resources.contains(a));
        if claims["email"].as_str().is_none_or(str::is_empty) && for_resource {
            // Auth0 issues third-party (dynamically registered) clients no OIDC scopes, so their tokens
            // can't call userinfo: the tenant's post-login Action stamps the email on them instead, under
            // the tenant's own signature. Tokens without the stamp ask the tenant.
            if claims[STAMPED_EMAIL]
                .as_str()
                .is_some_and(|e| !e.is_empty())
            {
                claims["email"] = claims[STAMPED_EMAIL].clone();
                claims["email_verified"] = claims[STAMPED_EMAIL_VERIFIED].clone();
            } else {
                let info = self.userinfo(token).await?;
                for k in ["email", "email_verified"] {
                    claims[k] = info[k].clone();
                }
            }
        }
        let user = super::oidc::email_of(&claims)?;
        // The provider check lets an absent claim pass; here only an email said to be verified signs in.
        anyhow::ensure!(says_verified(&claims), "the token's email is not verified");
        let org_id = claims["org_id"]
            .as_str()
            .filter(|o| !o.is_empty())
            .map(String::from);
        let orgs: Vec<&String> = bound
            .iter()
            .filter(|(_, id)| Some(id) == org_id.as_ref())
            .map(|(org, _)| org)
            .collect();
        let orgs = match orgs.as_slice() {
            [one] => [(*one).clone()].into(),
            _ => Default::default(),
        };
        let operator = org_id.is_some() && org_id.as_deref() == self.operators.as_deref();
        // The operators' Organization signs in to no org, even one bound to it past the server's check
        // (another replica's settings, the catalog owner's CLI): operators manage orgs, they don't read them.
        let orgs = if operator { Default::default() } else { orgs };
        // The tenant vouches for the email itself only through the connections the platform runs: its own
        // database, passwordless email and the social providers. An Organization's enterprise connection
        // (SAML, OIDC, Okta, Azure AD) is the customer's, and asserts whatever email it likes: it signs its
        // people in to its org, and vouches for no one beyond it (no own memory, `?scope=mine`).
        let platform = claims["sub"].as_str().is_some_and(|s| {
            PLATFORM_CONNECTIONS
                .iter()
                .any(|c| s.split_once('|').is_some_and(|(k, _)| k == *c))
        });
        // A token naming no Organization (an MCP client's access token: the client asks the tenant for
        // none; the CLI's ID token: `memstore join` signs in first and asks the directory which orgs are
        // theirs). When the tenant itself vouches for the email, its holder is the directory's member
        // wherever it lists them, as one signed in through the server's own provider is. An enterprise
        // connection's holder stays bound to the Organization the token names (the CLI's `--sso`), like
        // any other Auth0 sign-in: such a connection asserts whatever email it likes.
        let by_directory = org_id.is_none() && platform;
        Ok(Caller {
            aliases: vec![user.clone()],
            user,
            person: true,
            email_verified: platform,
            orgs: if by_directory { None } else { Some(orgs) },
            auth0: !by_directory,
            auth0_org: org_id,
            operator,
            ..Default::default()
        })
    }
}

/// A token's audiences, one or many.
fn audiences_of(claims: &serde_json::Value) -> Vec<String> {
    match &claims["aud"] {
        serde_json::Value::String(a) => vec![a.clone()],
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => vec![],
    }
}

/// The Auth0 connections whose emails the platform's tenant vouches for, by the strategy a user id (`sub`)
/// begins with: its database (`auth0|`), passwordless email, and social providers that verify the address.
/// The claims the tenant's post-login Action stamps on an access token for this deployment: the
/// holder's email and whether the tenant verified it (third-party clients get no OIDC scopes).
const STAMPED_EMAIL: &str = "https://memstore/email";
const STAMPED_EMAIL_VERIFIED: &str = "https://memstore/email_verified";

const PLATFORM_CONNECTIONS: [&str; 6] = [
    "auth0",
    "email",
    "google-oauth2",
    "github",
    "windowslive",
    "apple",
];

/// What is known of an org's provider: its keys, or that reading them failed (and when).
enum Issuer {
    Ready(Arc<OidcConfig>),
    Failed(std::time::Instant),
}

/// A provider that couldn't be read isn't asked again for this long: tokens naming it can't make the
/// server fetch on every request.
const ISSUER_RETRY: std::time::Duration = std::time::Duration::from_secs(60);

/// API keys and signed tokens: how a caller proves who they are.
pub struct TokenAuth {
    keys: Vec<ApiKeyRow>,
    jwt: Option<JwtConfig>,
    oidc: Option<OidcConfig>,
    auth0: Option<Auth0>,
    /// Orgs' own providers, by issuer, discovered when their first token arrives.
    org_issuers: std::sync::Mutex<HashMap<String, Issuer>>,
    /// Orgs' providers may be on this machine, over http too (a provider running locally, in tests).
    /// Never on the private network, link-local (the cloud's metadata service) and the like.
    local_issuers: bool,
}

pub use memstore_client::KEY_PREFIX;

impl TokenAuth {
    pub fn new(keys_json: Option<&str>, jwt: Option<JwtConfig>) -> Result<Self> {
        let keys: Vec<ApiKeyRow> = match keys_json {
            Some(j) => serde_json::from_str(j).context("MEMSTORE_API_KEYS")?,
            None => vec![],
        };
        let mut seen = HashSet::new();
        for k in &keys {
            anyhow::ensure!(
                k.sha256.len() == 64 && k.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "api key {}: sha256 must be 64 hex chars",
                k.id
            );
            anyhow::ensure!(seen.insert(&k.id), "duplicate api key id {}", k.id);
        }
        Ok(TokenAuth {
            keys,
            jwt,
            oidc: None,
            auth0: None,
            org_issuers: Default::default(),
            local_issuers: false,
        })
    }

    /// Also accept people's ID tokens from an identity provider.
    pub fn with_oidc(mut self, oidc: OidcConfig) -> Self {
        self.oidc = Some(oidc);
        self
    }

    /// Also accept ID tokens from the platform's Auth0 tenant, each for the org its Auth0 Organization is
    /// bound to. Set after the server's provider and the JWT key. The tenant is never the server's own
    /// provider: a token from it for the server's client would be bound to no Organization, so one
    /// Organization's connection (which says what the email is) could sign people in to every org. The
    /// app JWT may share its issuer, never one of its client ids.
    pub fn with_auth0(mut self, auth0: Auth0) -> Result<Self> {
        let iss = auth0.issuer();
        let ids = auth0.client_ids();
        if let Some(o) = &self.oidc
            && o.issuer == iss
        {
            anyhow::bail!(
                "MEMSTORE_OIDC_ISSUER is the Auth0 tenant ({iss}): the tenant signs people in through MEMSTORE_AUTH0_ISSUER only, bound to their organization; give the server's own sign-in another issuer, or leave it unset"
            );
        }
        if let Some(j) = &self.jwt
            && j.opts.issuer.trim_end_matches('/') == iss
            && ids.contains(&j.opts.audience)
        {
            anyhow::bail!(
                "MEMSTORE_JWT_AUDIENCE {} of {iss} is one of MEMSTORE_AUTH0_CLIENT_IDS",
                j.opts.audience
            );
        }
        self.auth0 = Some(auth0);
        Ok(self)
    }

    /// The platform's Auth0 tenant, if its tokens are taken.
    pub fn auth0(&self) -> Option<&Auth0> {
        self.auth0.as_ref()
    }

    /// Whether `token` is the Auth0 tenant's to check: from its issuer, and issued to one of its clients
    /// (or to anyone, if no other source takes that issuer's tokens). Such a token is checked by it alone,
    /// never by the server's provider or an org's.
    pub fn is_auth0(&self, token: &str) -> bool {
        let Some(a) = &self.auth0 else {
            return false;
        };
        if token_issuer(token).as_deref() != Some(a.issuer()) {
            return false;
        }
        let shared = self
            .jwt
            .as_ref()
            .is_some_and(|j| j.opts.issuer.trim_end_matches('/') == a.issuer());
        !shared || a.names_client(token)
    }

    /// Let orgs connect providers on this machine (`MEMSTORE_ALLOW_LOCAL_ISSUERS`), over http too: for a
    /// provider run locally. Otherwise an org's provider and its keys must be https on a public address.
    pub fn allow_local_issuers(mut self) -> Self {
        self.local_issuers = true;
        self
    }

    /// An org admin names the provider, and the server fetches from it: vetted (see [`super::fetch`]).
    fn org_fetch(&self) -> FetchPolicy {
        FetchPolicy::Vetted {
            allow_loopback: self.local_issuers,
        }
    }

    /// Whether any caller could authenticate.
    pub fn has_credentials(&self) -> bool {
        !self.keys.is_empty() || self.jwt.is_some() || self.oidc.is_some() || self.auth0.is_some()
    }

    /// The identity provider people sign in with, if any.
    pub fn oidc(&self) -> Option<&OidcConfig> {
        self.oidc.as_ref()
    }

    /// How many API keys there are, and how many of them are admin keys.
    pub fn key_counts(&self) -> (usize, usize) {
        (
            self.keys.len(),
            self.keys.iter().filter(|k| k.admin).count(),
        )
    }

    /// Whether a bearer is one this source takes: an API key, or a JWT from its JWT or OIDC issuer. Used to
    /// tell its tokens from a platform's (Google's) when both are accepted.
    pub fn issues(&self, token: &str) -> bool {
        if token.starts_with(KEY_PREFIX) || self.is_auth0(token) {
            return true;
        }
        match token_issuer(token) {
            Some(iss) => {
                self.oidc.as_ref().is_some_and(|o| o.issuer == iss)
                    || self
                        .jwt
                        .as_ref()
                        .is_some_and(|j| j.opts.issuer.trim_end_matches('/') == iss)
            }
            None => false,
        }
    }

    pub fn jwt(&self) -> Option<&JwtConfig> {
        self.jwt.as_ref()
    }

    /// A key's caller, if `token` is one of ours.
    pub fn key_caller(&self, token: &str) -> Option<KeyCaller> {
        self.file_key(&hex(&sha2::Sha256::digest(token.as_bytes())))
    }

    /// The file key whose sha256 (hex) is `digest`, if this server holds one.
    pub fn file_key(&self, digest: &str) -> Option<KeyCaller> {
        let digest = digest.to_ascii_lowercase();
        // Every key is compared, in constant time, so timing doesn't say how close a guess was.
        let mut found = None;
        for k in &self.keys {
            if constant_eq(k.sha256.to_ascii_lowercase().as_bytes(), digest.as_bytes()) {
                found = Some(KeyCaller {
                    id: k.id.clone(),
                    user: k.user.clone(),
                    groups: k.groups.clone(),
                    admin: k.admin,
                    sha256: digest.clone(),
                });
            }
        }
        found
    }

    /// The caller a bearer token proves.
    pub async fn authenticate(&self, token: &str) -> Result<Caller> {
        if token.starts_with(KEY_PREFIX) {
            let k = self
                .key_caller(token)
                .ok_or_else(|| anyhow::anyhow!("unknown api key"))?;
            return Ok(Caller {
                aliases: vec![k.user.clone(), format!("key:{}", k.id)],
                user: k.user,
                groups: k.groups,
                admin: k.admin,
                ..Default::default()
            });
        }
        let jwt = self
            .jwt
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("signed tokens aren't accepted here"))?;
        jwt.verify(token).await
    }

    /// Whether checking `token` needs the directory: an Auth0 tenant's token (which org its Organization
    /// is bound to), or one that may be an org's own provider's, a JWT from an issuer neither this
    /// server's provider nor the app's tokens use.
    pub fn wants_org_providers(&self, token: &str) -> bool {
        if token.starts_with(KEY_PREFIX) {
            return false;
        }
        if self.is_auth0(token) {
            return true;
        }
        match token_issuer(token) {
            Some(iss) => {
                !self.oidc.as_ref().is_some_and(|o| o.issuer == iss)
                    && !self
                        .jwt
                        .as_ref()
                        .is_some_and(|j| j.opts.issuer.trim_end_matches('/') == iss)
            }
            None => false,
        }
    }

    /// [`Self::authenticate`], also accepting ID tokens from this server's identity provider, from orgs'
    /// own (`providers`) and from the platform's Auth0 tenant (`auth0_orgs`: each org bound to an Auth0
    /// Organization, with that Organization's id). People hold no groups of their own: what they may read
    /// comes from the scope they select. One signed in through an org's provider, or through Auth0, is
    /// bound to that org.
    pub async fn authenticate_any(
        &self,
        token: &str,
        providers: &[OrgProvider],
        auth0_orgs: &[(String, String)],
    ) -> Result<Caller> {
        if token.starts_with(KEY_PREFIX) {
            return self.authenticate(token).await;
        }
        if let Some(a) = &self.auth0
            && self.is_auth0(token)
        {
            return a.caller(token, auth0_orgs).await;
        }
        let iss = token_issuer(token).unwrap_or_default();
        // The Auth0 tenant is the platform's: an org's provider naming it (connected before the tenant
        // was configured) vouches for no one.
        let mine: Vec<&OrgProvider> = providers
            .iter()
            .filter(|p| p.issuer.trim_end_matches('/') == iss)
            .filter(|_| self.auth0.as_ref().is_none_or(|a| a.issuer() != iss))
            .collect();
        if let Some(o) = &self.oidc
            && o.issuer == iss
        {
            // The server's provider; an org may use the same issuer with its own client.
            match o.verify_for(token, &o.audiences).await {
                Ok((user, claims)) => {
                    return Ok(Caller {
                        aliases: vec![user.clone()],
                        user,
                        person: true,
                        email_verified: says_verified(&claims),
                        ..Default::default()
                    });
                }
                Err(e) if mine.is_empty() => return Err(e),
                Err(_) => {}
            }
        }
        if mine.is_empty() {
            return self.authenticate(token).await;
        }
        let config = self.org_issuer(&iss).await?;
        let audiences: Vec<String> = mine
            .iter()
            .flat_map(|p| p.client_ids.iter().cloned())
            .collect();
        let (user, claims) = config.verify_for(token, &audiences).await?;
        let aud: Vec<&str> = match &claims["aud"] {
            serde_json::Value::String(a) => vec![a.as_str()],
            serde_json::Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
            _ => vec![],
        };
        let mine: Vec<&&OrgProvider> = mine
            .iter()
            .filter(|p| p.client_ids.iter().any(|c| aud.contains(&c.as_str())))
            .collect();
        let mut idp_groups: Vec<String> = mine
            .iter()
            .flat_map(|p| {
                claims[p.groups_claim.as_str()]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|g| g.as_str().map(String::from))
            .collect();
        idp_groups.sort();
        idp_groups.dedup();
        Ok(Caller {
            aliases: vec![user.clone()],
            user,
            person: true,
            email_verified: says_verified(&claims),
            orgs: Some(mine.iter().map(|p| p.org.clone()).collect()),
            idp_groups,
            ..Default::default()
        })
    }

    /// An org provider's keys, read once per issuer.
    async fn org_issuer(&self, issuer: &str) -> Result<Arc<OidcConfig>> {
        match self.org_issuers.lock().unwrap().get(issuer) {
            Some(Issuer::Ready(c)) => return Ok(c.clone()),
            Some(Issuer::Failed(at)) if at.elapsed() < ISSUER_RETRY => {
                anyhow::bail!("{issuer} couldn't be read lately")
            }
            _ => {}
        }
        // Read without holding the lock: one slow provider doesn't hold up the others.
        let read = OidcConfig::discover_issuer(issuer, vec![], self.org_fetch())
            .await
            .map(Arc::new);
        let entry = match &read {
            Ok(c) => Issuer::Ready(c.clone()),
            Err(_) => Issuer::Failed(std::time::Instant::now()),
        };
        self.org_issuers
            .lock()
            .unwrap()
            .insert(issuer.to_string(), entry);
        read
    }

    /// Check that an org's provider can be used: its discovery document and keys are readable. What
    /// went wrong is logged; the error says only [`super::fetch::UNREADABLE`].
    pub async fn check_issuer(&self, issuer: &str) -> Result<()> {
        let c = OidcConfig::discover_issuer(issuer, vec![], self.org_fetch()).await?;
        c.read_keys().await?;
        self.org_issuers
            .lock()
            .unwrap()
            .insert(c.issuer.clone(), Issuer::Ready(Arc::new(c)));
        Ok(())
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
