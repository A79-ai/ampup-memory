//! The CLI as a client of a `memstore serve`: who you are (`memstore auth login`) and which memory you
//! want, kept the way kubectl keeps clusters and namespaces. `config.json` holds named **contexts**
//! (a server, an org, a team) and the current one; `credentials.json` holds one sign-in per server, plus
//! one per org that signs people in through its own identity provider.
//! Both live in `AMPUP_MEMORY_CONFIG_DIR` (default `~/.ampup-memory`; once `~/.memstore`, moved there on first run), readable by you only.
//!
//! Signing in is the MCP authorization flow (the one Claude and other MCP clients run when they connect):
//! read the server's protected-resource metadata (RFC 9728) for its identity provider, read the provider's
//! OpenID configuration, then an authorization-code grant with PKCE (S256) through the browser, redirected
//! to a one-shot listener on 127.0.0.1. The ID token it returns is the bearer; its refresh token renews it.
//! Service callers (CI, a platform admin) sign in with an API key instead.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context as _, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;

/// The settings of a context.
pub const KEYS: [&str; 4] = ["server", "org", "team", "user"];

pub fn config_dir() -> PathBuf {
    match std::env::var("AMPUP_MEMORY_CONFIG_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => crate::naming::current_state_dir(),
    }
}

static CONTEXT_OVERRIDE: OnceLock<String> = OnceLock::new();

/// `--context` (or `AMPUP_MEMORY_CONTEXT`): use this context for one command instead of the current one.
pub fn override_context(name: &str) {
    let _ = CONTEXT_OVERRIDE.set(name.to_string());
}

/// Where a command goes and whose memory it reads: a server, an org and (optionally) a team, plus
/// `user`, who you read as when the CLI reaches the catalog directly.
#[derive(Clone, Default, Serialize, Deserialize, Debug, PartialEq)]
pub struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl Context {
    pub fn get(&self, key: &str) -> Option<&str> {
        match key {
            "server" => self.server.as_deref(),
            "org" => self.org.as_deref(),
            "team" => self.team.as_deref(),
            "user" => self.user.as_deref(),
            _ => None,
        }
        .filter(|v| !v.is_empty())
    }

    pub fn set(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        if let Some(v) = value {
            match key {
                "org" => crate::check_name("org", v)?,
                "team" => crate::check_name("team", v)?,
                "server" => anyhow::ensure!(
                    v.starts_with("http://") || v.starts_with("https://"),
                    "server is a URL (http:// or https://)"
                ),
                "user" => {}
                _ => anyhow::bail!("unknown setting {key} (one of {})", KEYS.join(", ")),
            }
        }
        let v = value.map(|v| v.trim_end_matches('/').to_string());
        match key {
            "server" => self.server = v,
            // A team belongs to an org: choosing another org forgets the team.
            "org" => {
                if self.org != v {
                    self.team = None;
                }
                self.org = v
            }
            "team" => self.team = v,
            "user" => self.user = v,
            _ => anyhow::bail!("unknown setting {key} (one of {})", KEYS.join(", ")),
        }
        Ok(())
    }

    /// `acme` or `acme/sales`: what `memstore use` and `create-org` name a context.
    pub fn default_name(&self) -> String {
        match (&self.org, &self.team) {
            (Some(o), Some(t)) => format!("{o}/{t}"),
            (Some(o), None) => o.clone(),
            _ => "default".into(),
        }
    }
}

/// `config.json`.
#[derive(Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(
        rename = "current-context",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub current: Option<String>,
    #[serde(default)]
    pub contexts: BTreeMap<String, Context>,
}

impl Config {
    pub fn load() -> Result<Self> {
        match std::fs::read_to_string(config_dir().join("config.json")) {
            Ok(t) => Ok(serde_json::from_str(&t).context("config.json")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> Result<()> {
        write_private("config.json", &serde_json::to_string_pretty(self)?)
    }

    /// The context in use: `--context`, else `AMPUP_MEMORY_CONTEXT`, else the current one.
    pub fn current_name(&self) -> Option<String> {
        CONTEXT_OVERRIDE
            .get()
            .cloned()
            .or_else(|| {
                std::env::var("AMPUP_MEMORY_CONTEXT")
                    .ok()
                    .filter(|c| !c.is_empty())
            })
            .or_else(|| self.current.clone())
    }

    /// The context in use (empty when there is none).
    pub fn context(&self) -> Context {
        self.current_name()
            .and_then(|n| self.contexts.get(&n).cloned())
            .unwrap_or_default()
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.context().get(key).map(String::from)
    }

    /// Change a setting of the context in use, creating a `default` context if there's none yet.
    pub fn set(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        let name = self.current_name().unwrap_or_else(|| "default".into());
        if self.current.is_none() {
            self.current = Some(name.clone());
        }
        self.contexts.entry(name).or_default().set(key, value)
    }

    /// Switch to `name`.
    pub fn use_context(&mut self, name: &str) -> Result<()> {
        anyhow::ensure!(
            self.contexts.contains_key(name),
            "no context {name} (ampup-memory config get-contexts)"
        );
        self.current = Some(name.into());
        Ok(())
    }

    /// Add or replace context `name` and switch to it.
    pub fn put_context(&mut self, name: &str, ctx: Context) {
        self.contexts.insert(name.into(), ctx);
        self.current = Some(name.into());
    }
}

/// Written readable by the owner only: credentials sit beside the config. Written whole to a file of its own and
/// renamed into place, so a reader (a hook starting while another renews) never sees half a file, and the file
/// ends up owner-only even if it was looser before.
fn write_private(name: &str, text: &str) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let written = opts
        .open(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(written?)
}

/// Held while a sign-in is renewed: the hooks and `memstore mcp` are separate processes that renew the same
/// sign-in, and a refresh token used twice (where the provider rotates them) can sign the person out everywhere.
fn renewal_lock() -> Result<std::fs::File> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let f = opts.open(dir.join("credentials.lock"))?;
    f.lock()?;
    Ok(f)
}

/// One server's sign-in, in `credentials.json`: a person's tokens from the identity provider, or an
/// API key.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Credentials {
    pub user: String,
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub token_endpoint: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Seconds since epoch.
    #[serde(default)]
    pub expires_at: u64,
    /// A service caller's key (`msk_...`): sent as it is, never renewed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Which server these are for: the key in `credentials.json`, `<server>` or, for a sign-in through
    /// an org's own provider (good in that org only), `<server>#<org>`.
    #[serde(skip)]
    server: String,
}

fn org_key(server: &str, org: &str) -> String {
    format!("{server}#{org}")
}

type CredentialFile = BTreeMap<String, Credentials>;

fn credential_file() -> Result<CredentialFile> {
    match std::fs::read_to_string(config_dir().join("credentials.json")) {
        Ok(t) => Ok(serde_json::from_str(&t).context("credentials.json")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

impl Credentials {
    /// The sign-in for the context's server and org.
    pub fn load(server: Option<&str>) -> Result<Option<Self>> {
        let ctx = Config::load()?.context();
        let server = match server {
            Some(s) => s.trim_end_matches('/').to_string(),
            None => match ctx.get("server") {
                Some(s) => s.to_string(),
                None => return Ok(None),
            },
        };
        Self::load_for(&server, ctx.get("org"))
    }

    /// The sign-in to use on `server` for `org`: the org's own, if it has one, else the server's.
    pub fn load_for(server: &str, org: Option<&str>) -> Result<Option<Self>> {
        let server = server.trim_end_matches('/');
        let mut all = credential_file()?;
        let key = org
            .map(|o| org_key(server, o))
            .filter(|k| all.contains_key(k))
            .unwrap_or_else(|| server.to_string());
        Ok(all.remove(&key).map(|mut c| {
            c.server = key;
            c
        }))
    }

    fn save(&self) -> Result<()> {
        let mut all = credential_file()?;
        all.insert(self.server.clone(), self.clone());
        write_private("credentials.json", &serde_json::to_string_pretty(&all)?)
    }

    /// Forget every sign-in to `server` (its own and its orgs'); whether there was one.
    pub fn remove(server: &str) -> Result<bool> {
        let server = server.trim_end_matches('/');
        let mut all = credential_file()?;
        let before = all.len();
        all.retain(|k, _| k != server && !k.starts_with(&format!("{server}#")));
        let had = all.len() < before;
        write_private("credentials.json", &serde_json::to_string_pretty(&all)?)?;
        Ok(had)
    }

    /// Sign in to `server` with an API key; the server tells who it belongs to.
    pub async fn api_key(server: &str, key: &str) -> Result<Self> {
        anyhow::ensure!(
            key.starts_with(crate::KEY_PREFIX),
            "API keys start with {}",
            crate::KEY_PREFIX
        );
        let server = server.trim_end_matches('/').to_string();
        let r = http()
            .get(format!("{server}/me"))
            .bearer_auth(key)
            .send()
            .await?;
        anyhow::ensure!(
            r.status().is_success(),
            "the server refused the key (HTTP {})",
            r.status()
        );
        let me: Value = r.json().await?;
        let c = Credentials {
            user: me["user"].as_str().unwrap_or_default().to_string(),
            api_key: Some(key.to_string()),
            server,
            ..Default::default()
        };
        c.save()?;
        Ok(c)
    }
}

/// How to sign in, when the server doesn't say (or to override it).
#[derive(Default)]
pub struct LoginOptions {
    pub issuer: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Print the sign-in URL instead of opening a browser.
    pub no_browser: bool,
    /// Sign in for this org: through its own identity provider, if it has one.
    pub org: Option<String>,
    /// Sign in to the org's Auth0 Organization (`organization=`: its enterprise connection, its groups)
    /// rather than as yourself, for the directory to say which orgs are yours.
    pub sso: bool,
    /// The loopback port the provider redirects to; else `AMPUP_MEMORY_LOGIN_PORT`, the port the server names,
    /// or any free one.
    pub port: Option<u16>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn http() -> reqwest::Client {
    http_for(Duration::from_secs(30))
}

fn http_for(wait: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(wait)
        .build()
        .expect("http client")
}

async fn get_json(url: &str) -> Result<Value> {
    let r = http()
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = r.status();
    anyhow::ensure!(status.is_success(), "GET {url}: HTTP {status}");
    Ok(r.json().await?)
}

/// The claims of a JWT, unverified: only for showing who signed in and when the token expires. The
/// server verifies every token it is given.
fn claims(jwt: &str) -> Value {
    jwt.split('.')
        .nth(1)
        .and_then(|p| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(p.trim_end_matches('='))
                .ok()
        })
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

/// Sign in to `server`'s identity provider. Returns who signed in.
pub async fn login(server: Option<&str>, opts: &LoginOptions) -> Result<Credentials> {
    let (mut issuer, mut client_id, mut client_secret) = (
        opts.issuer.clone(),
        opts.client_id.clone(),
        opts.client_secret.clone(),
    );
    // Set when the server sends us to an org's own provider.
    let mut org_provider: Option<String> = None;
    // The Auth0 Organization (`org_…`) the server says the org's sign-ins go into.
    let mut organization: Option<String> = None;
    let mut port = opts.port.or_else(|| {
        std::env::var("AMPUP_MEMORY_LOGIN_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
    });
    if issuer.is_none() || client_id.is_none() {
        let server = server.context(
            "no server to ask how to sign in: pass --server (or `ampup-memory config set server <url>`), or --issuer and --client-id",
        )?;
        let query = opts
            .org
            .as_deref()
            .map(|o| format!("?org={o}"))
            .unwrap_or_default();
        let meta = get_json(&format!(
            "{server}/.well-known/oauth-protected-resource{query}"
        ))
        .await
        .context("the server didn't say how to sign in; pass --issuer and --client-id")?;
        org_provider = meta["memstore_org"].as_str().map(String::from);
        // Only for `--sso`: a sign-in bound to the Organization reaches only people it holds, and an
        // Organization's enterprise connection holds whom the customer says. Everyone else signs in as
        // themselves (Google, a password) and the directory says which orgs are theirs.
        organization = meta["memstore_auth0_organization"]
            .as_str()
            .filter(|_| opts.sso)
            .map(String::from);
        port = port.or_else(|| {
            meta["memstore_cli_callback_port"]
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
        });
        issuer = issuer.or_else(|| meta["authorization_servers"][0].as_str().map(String::from));
        client_id = client_id.or_else(|| meta["memstore_cli_client_id"].as_str().map(String::from));
        client_secret = client_secret.or_else(|| {
            meta["memstore_cli_client_secret"]
                .as_str()
                .map(String::from)
        });
    }
    let issuer = issuer
        .context("no identity provider (--issuer)")?
        .trim_end_matches('/')
        .to_string();
    let client_id = client_id.context("no client id (--client-id)")?;
    let oidc = get_json(&format!("{issuer}/.well-known/openid-configuration")).await?;
    let auth_endpoint = oidc["authorization_endpoint"]
        .as_str()
        .context("provider has no authorization_endpoint")?;
    let token_endpoint = oidc["token_endpoint"]
        .as_str()
        .context("provider has no token_endpoint")?
        .to_string();

    // A one-shot listener for the provider's redirect. Loopback redirects take any port (RFC 8252), but
    // providers that match registered URIs exactly need the one registered.
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", port.unwrap_or(0))).with_context(|| {
            format!(
                "can't listen on 127.0.0.1:{} for the sign-in redirect",
                port.unwrap_or(0)
            )
        })?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
    let verifier = random();
    let challenge = b64(&sha2::Sha256::digest(verifier.as_bytes()));
    let state = random();
    let google = issuer == "https://accounts.google.com";
    let mut url = reqwest::Url::parse(auth_endpoint)?;
    // The provider's document names this URL, and it is handed to the system's browser opener: only
    // https (or http to this machine, for a provider running locally).
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    anyhow::ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && local),
        "the identity provider's sign-in URL must be https, got {auth_endpoint}"
    );
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        if let Some(o) = &organization {
            q.append_pair("organization", o);
        }
        if google {
            // Google asks for a refresh token this way, not with the offline_access scope.
            q.append_pair("scope", "openid email")
                .append_pair("access_type", "offline")
                .append_pair("prompt", "consent");
        } else {
            q.append_pair("scope", "openid email offline_access");
            if let Some(s) = server {
                q.append_pair("resource", &format!("{s}/mcp"));
            }
        }
    }
    eprintln!("Sign in at:\n{url}\n");
    if !opts.no_browser {
        open_browser(url.as_str());
    }
    let code = tokio::task::spawn_blocking(move || wait_for_code(listener, &state)).await??;

    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", redirect),
        ("client_id", client_id.clone()),
        ("code_verifier", verifier),
    ];
    if let Some(s) = &client_secret {
        form.push(("client_secret", s.clone()));
    }
    let tokens = token_request(&token_endpoint, &form).await?;
    let mut creds = Credentials {
        issuer: issuer.clone(),
        token_endpoint,
        client_id,
        client_secret,
        // Sign-ins are kept per server (and per org, through an org's own provider); one made without a
        // server is kept under its provider.
        server: match (server, &org_provider) {
            (Some(s), Some(o)) => org_key(s, o),
            (Some(s), None) => s.to_string(),
            (None, _) => issuer,
        },
        ..Default::default()
    };
    creds.take(&tokens)?;
    creds.save()?;
    Ok(creds)
}

impl Credentials {
    /// The org whose own provider this sign-in is through, if it is.
    pub fn org(&self) -> Option<&str> {
        self.server.split_once('#').map(|(_, o)| o)
    }

    /// The ID token's claims (unverified: for showing what the provider says).
    pub fn claims(&self) -> Value {
        self.id_token.as_deref().map(claims).unwrap_or(Value::Null)
    }

    /// Fill in a token response.
    fn take(&mut self, t: &Value) -> Result<()> {
        self.access_token = t["access_token"]
            .as_str()
            .context("no access_token in the provider's answer")?
            .to_string();
        if let Some(id) = t["id_token"].as_str() {
            self.id_token = Some(id.to_string());
        }
        if let Some(r) = t["refresh_token"].as_str() {
            self.refresh_token = Some(r.to_string());
        }
        let c = self.id_token.as_deref().map(claims).unwrap_or(Value::Null);
        self.expires_at = c["exp"]
            .as_u64()
            .unwrap_or_else(|| now() + t["expires_in"].as_u64().unwrap_or(3600));
        if let Some(u) = c["email"].as_str().or(c["sub"].as_str()) {
            self.user = u.to_string();
        }
        Ok(())
    }

    /// The bearer to send: the API key, or the ID token (what servers verify), renewed first if it
    /// expires within a minute.
    pub async fn bearer(&mut self) -> Result<String> {
        if let Some(k) = &self.api_key {
            return Ok(k.clone());
        }
        if self.expires_at > now() + 60 {
            return Ok(self.current());
        }
        let _held = renewal_lock()?;
        // Another process may have renewed while this one waited, or since this one read the file (a
        // long-lived `memstore mcp`): take its sign-in, renewed or not, so its refresh token is the newest.
        if let Some(mut disk) = credential_file()?.remove(&self.server) {
            disk.server = std::mem::take(&mut self.server);
            *self = disk;
        }
        if self.expires_at <= now() + 60 {
            let refresh = self.refresh_token.clone().ok_or_else(|| {
                SignedOut(format!("signed-in token expired: run {}", sign_in_hint()))
            })?;
            let mut form = vec![
                ("grant_type", "refresh_token".to_string()),
                ("refresh_token", refresh),
                ("client_id", self.client_id.clone()),
            ];
            if let Some(s) = &self.client_secret {
                form.push(("client_secret", s.clone()));
            }
            // Offline, signing in again would not help: the renewal works once the provider answers. Refused, only
            // signing in again does.
            let t = token_request(&self.token_endpoint, &form)
                .await
                .map_err(|e| match e.is::<Unreached>() {
                    true => e.context("the sign-in couldn't be renewed now"),
                    false => SignedOut(format!(
                        "renewing the sign-in failed: run {}: {e:#}",
                        sign_in_hint()
                    ))
                    .into(),
                })?;
            self.take(&t)?;
            self.save()?;
        }
        Ok(self.current())
    }

    /// The ID token (what servers verify), else the access token.
    fn current(&self) -> String {
        self.id_token
            .clone()
            .unwrap_or_else(|| self.access_token.clone())
    }
}

/// A failure worth sending again: the memory unreachable, slow, down, busy or the sign-in stale, or the sign-in's
/// provider unreachable, down or busy while renewing it. A refusal (bad arguments, a session that isn't the
/// caller's) never is, and a renewal the provider refused is [`SignedOut`]. With the seconds it said to wait first,
/// when it said.
#[derive(Debug)]
pub(crate) struct Unreached(pub(crate) String, pub(crate) Option<u64>);

impl std::fmt::Display for Unreached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreached {}

/// The sign-in can't be renewed: its provider refused the renewal (a refresh token revoked or past its life), or
/// there is nothing to renew it with. Nothing gets through until the person signs in again, and then it does: a
/// hook's session end waits in the outbox for that rather than being lost.
#[derive(Debug)]
pub(crate) struct SignedOut(pub(crate) String);

impl std::fmt::Display for SignedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SignedOut {}

/// Why a request never got its answer, with what reqwest keeps under its own message ("error sending request"):
/// a timeout and how long it waited, or the connection's, TLS's or DNS's own error.
pub(crate) fn sent_error(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    if e.is_timeout() {
        out.push_str(": timed out");
    }
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        out.push_str(&format!(": {s}"));
        source = s.source();
    }
    out
}

/// A token request's answer. Unanswered, or the provider down or busy, it is [`Unreached`]: offline, a hook's
/// write (a session's end) waits in the outbox rather than being lost to a renewal that would work later.
async fn token_request(endpoint: &str, form: &[(&str, String)]) -> Result<Value> {
    let r = http().post(endpoint).form(form).send().await.map_err(|e| {
        Unreached(
            format!("no answer from {endpoint}: {}", sent_error(&e)),
            None,
        )
    })?;
    let status = r.status();
    if status.is_server_error() || [408, 429].contains(&status.as_u16()) {
        return Err(Unreached(format!("token request to {endpoint}: HTTP {status}"), None).into());
    }
    let body: Value = r.json().await.unwrap_or(Value::Null);
    anyhow::ensure!(
        status.is_success(),
        "token request refused (HTTP {status}): {}",
        body["error_description"]
            .as_str()
            .or(body["error"].as_str())
            .unwrap_or("no reason given")
    );
    Ok(body)
}

fn open_browser(url: &str) {
    let r = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(windows) {
        // Not through cmd.exe, which would read the URL's `&`s as command separators.
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    if !r.is_ok_and(|s| s.success()) {
        eprintln!("(couldn't open a browser: open the URL above yourself)");
    }
}

/// Wait (up to five minutes) for the provider to redirect the browser back with a code for `state`.
fn wait_for_code(listener: std::net::TcpListener, state: &str) -> Result<String> {
    listener.set_nonblocking(false)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "no sign-in within five minutes"
        );
        let (mut conn, _) = listener.accept()?;
        conn.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut line = String::new();
        BufReader::new(&conn).read_line(&mut line)?;
        // "GET /callback?code=...&state=... HTTP/1.1"
        let target = line.split_whitespace().nth(1).unwrap_or_default();
        let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else {
            continue;
        };
        if url.path() != "/callback" {
            let _ = conn.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            continue;
        }
        let q: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
        let (ok, msg) = match (q.get("code"), q.get("state"), q.get("error")) {
            (_, _, Some(e)) => (
                Err(anyhow::anyhow!("sign-in refused: {e}")),
                "Sign-in was refused.",
            ),
            (Some(c), Some(s), None) if s == state => (
                Ok(c.clone()),
                "Signed in to AmpUp Memory. You can close this tab.",
            ),
            _ => (
                Err(anyhow::anyhow!("sign-in answer doesn't match this request")),
                "That sign-in answer wasn't for this request.",
            ),
        };
        let page = format!("<html><body><p>{msg}</p></body></html>");
        let _ = write!(
            conn,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        return ok;
    }
}

/// A request to the context's server as the signed-in user, selecting the context's org and team.
pub async fn call(method: &str, path: &str, body: Option<Value>) -> Result<Value> {
    let ctx = Config::load()?.context();
    let server = ctx.get("server").context(
        "no server: `ampup-memory join <url>`, or `ampup-memory config set server <url>`",
    )?;
    call_on(server, ctx.get("org"), ctx.get("team"), method, path, body).await
}

/// `call`, waiting up to `wait` for the answer: for what takes minutes on the server (an org's clear).
pub async fn call_waiting(
    method: &str,
    path: &str,
    body: Option<Value>,
    wait: Duration,
) -> Result<Value> {
    let ctx = Config::load()?.context();
    let server = ctx.get("server").context(
        "no server: `ampup-memory join <url>`, or `ampup-memory config set server <url>`",
    )?;
    let org = ctx.get("org");
    call_as(server, org, org, ctx.get("team"), method, path, body, wait).await
}

/// A request to `server` as the user signed in to it, selecting `org` and `team`.
pub async fn call_on(
    server: &str,
    org: Option<&str>,
    team: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    call_as(
        server,
        org,
        org,
        team,
        method,
        path,
        body,
        Duration::from_secs(30),
    )
    .await
}

/// A request to `server` with the sign-in kept for `signed_in` (an org's own, else the server's), selecting
/// `org` and `team`: `join` asks who it signed in as before any org is selected, with the sign-in it just made.
#[allow(clippy::too_many_arguments)]
async fn call_as(
    server: &str,
    signed_in: Option<&str>,
    org: Option<&str>,
    team: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
    wait: Duration,
) -> Result<Value> {
    let mut creds = Credentials::load_for(server, signed_in)?
        .with_context(|| format!("not signed in to {server}: run {}", sign_in_hint()))?;
    let bearer = creds.bearer().await?;
    let url = format!("{server}{path}");
    let mut req = match method {
        "GET" => http_for(wait).get(&url),
        _ => http_for(wait).post(&url).json(&body.unwrap_or(json!({}))),
    }
    .bearer_auth(bearer);
    if let Some(o) = org {
        req = req.header("x-memstore-org", o);
    }
    if let Some(t) = team {
        req = req.header("x-memstore-team", t);
    }
    let r = req
        .send()
        .await
        .with_context(|| format!("{method} {url}"))?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    anyhow::ensure!(
        status.is_success(),
        "{path}: HTTP {status}: {}",
        v["error"].as_str().unwrap_or("no reason given")
    );
    Ok(v)
}

/// `memstore join` (and `memstore-client join`): sign in to `server` as yourself, then add a context for `org`
/// and `team` there and switch to it. With no org named, the one org the server says you're in; several, and
/// a terminal asks which; none, and where the server names a sign-up service, it offers to make your own org
/// (`create_org` makes it without asking), else asks which org to ask to join. Not a member of `org`: asks to
/// join it (the org's admins are emailed to approve), and says so; run again once approved. `sso` signs in to
/// the org's Auth0 Organization instead (an enterprise connection's people).
pub async fn join(
    server: &str,
    org: Option<&str>,
    team: Option<&str>,
    no_browser: bool,
    sso: bool,
    create_org: bool,
) -> Result<()> {
    let server = server.trim_end_matches('/');
    let opts = LoginOptions {
        no_browser,
        org: org.map(String::from),
        sso,
        ..Default::default()
    };
    let creds = login(Some(server), &opts).await?;
    println!("Signed in as {}", creds.user);
    // Through an org's own provider the sign-in is kept under that org (`<server>#<org>`), not the server.
    let signed_in = creds.org();
    let me = match call_as(
        server,
        signed_in,
        None,
        None,
        "GET",
        "/me",
        None,
        Duration::from_secs(30),
    )
    .await
    {
        Ok(me) => me,
        // The provider says it hasn't verified the email: the server takes such a sign-in nowhere but an org
        // whose admins approved it. Say so, rather than the server's bare refusal.
        Err(e) if creds.claims()["email_verified"] == Value::Bool(false) => anyhow::bail!(
            "{server} doesn't take this sign-in: the identity provider hasn't verified {}'s email ({e:#}). \
            Sign in with Google or GitHub (an account whose email the provider verified), or verify the email, \
            and run this again; making an org of your own takes a Google or GitHub sign-in",
            creds.user
        ),
        Err(e) => return Err(e),
    };
    // An alias of one of theirs is that org, kept under its own name.
    let own = org.and_then(|o| member_org(&me, o));
    let org = match (org, own) {
        (_, Some(own)) => own,
        (Some(o), None) => return request_join(server, signed_in, o, &creds.user, &me).await,
        (None, None) => {
            let orgs: Vec<String> = me["memberships"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            match orgs.as_slice() {
                [one] => one.clone(),
                [] => {
                    // Nobody's yet. Where the server names a sign-up service, the person can make an org of
                    // their own: asked at a terminal, or straight away with --create-org.
                    let signup = signup_url(server).await;
                    if let Some(url) = &signup {
                        let make = create_org
                            || confirm(&format!(
                                "{} isn't a member of any org on {server} yet. Make your own memory org now? [Y/n] ",
                                creds.user
                            ))?
                            .unwrap_or(false);
                        if make {
                            let org = make_own_org(server, url, &creds, no_browser).await?;
                            return use_context(server, &org, team);
                        }
                    } else if create_org {
                        anyhow::bail!(
                            "{server} offers no sign-up: ask an admin of your team's org to invite you, or name the org to ask to join (--org <org>)"
                        );
                    }
                    // A terminal asks which org to ask to join; a hook or command says how.
                    let Some(o) = ask(&format!(
                        "{} isn't a member of any org on {server} yet. Which org do you want to join? (its name; an admin will be emailed to approve) ",
                        creds.user
                    ))?
                    else {
                        let own = if signup.is_some() {
                            " Or make your own memory org: --create-org (the plugin: /ampup-memory:login --create-org)."
                        } else {
                            ""
                        };
                        anyhow::bail!(
                            "{} isn't a member of any org on {server} yet: name the org to ask to join (--org <org>; the plugin: /ampup-memory:login --org <org>), and its admins are emailed to approve.{own}",
                            creds.user
                        )
                    };
                    return request_join(server, signed_in, &o, &creds.user, &me).await;
                }
                many => {
                    let list = many.join(", ");
                    match choose(
                        &format!("{} is in several orgs: which one here?", creds.user),
                        many,
                    )? {
                        Some(o) => o,
                        None => anyhow::bail!(
                            "{} is in several orgs ({list}): name one (--org <org>; the plugin: /ampup-memory:login --org <org>)",
                            creds.user
                        ),
                    }
                }
            }
        }
    };
    if create_org {
        println!(
            "{} is in {org} on {server} already: no new org made",
            creds.user
        );
    }
    use_context(server, &org, team)
}

/// `ampup-memory-client join <server> --org <org> --api-key <key>`: signs in to `server` with a personal API key,
/// with no browser (a cloud agent's machine, CI), as `ampup-memory auth login --api-key` does, then adds a context
/// for `org` (and `team`) there and switches to it. With no org named, the one the server says the key's person is
/// in. The key is kept in `credentials.json` (owner-only), never in a config an agent reads.
pub async fn join_with_key(
    server: &str,
    org: Option<&str>,
    team: Option<&str>,
    key: &str,
) -> Result<()> {
    let server = server.trim_end_matches('/');
    let c = Credentials::api_key(server, key).await?;
    println!("Signed in to {server} as {} (API key)", c.user);
    let org = match org {
        Some(o) => o.to_string(),
        None => {
            let me = call_as(
                server,
                None,
                None,
                None,
                "GET",
                "/me",
                None,
                Duration::from_secs(30),
            )
            .await?;
            let orgs: Vec<String> = me["memberships"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            match orgs.as_slice() {
                [one] => one.clone(),
                [] => anyhow::bail!("{} is in no org on {server}: name one with --org", c.user),
                many => anyhow::bail!(
                    "{} is in several orgs ({}): name one with --org",
                    c.user,
                    many.join(", ")
                ),
            }
        }
    };
    use_context(server, &org, team)
}

/// The org of the caller's (`/me`) that `named` selects: one of that name, else the one that has it as an alias.
pub fn member_org(me: &Value, named: &str) -> Option<String> {
    if me["memberships"].get(named).is_some() {
        return Some(named.to_string());
    }
    me["names"].as_object()?.iter().find_map(|(org, n)| {
        n["aliases"]
            .as_array()?
            .iter()
            .any(|a| a == named)
            .then(|| org.clone())
    })
}

/// Add a context for `org` (and `team`) on `server` and switch to it.
fn use_context(server: &str, org: &str, team: Option<&str>) -> Result<()> {
    let mut ctx = Context::default();
    ctx.set("server", Some(server))?;
    ctx.set("org", Some(org))?;
    ctx.set("team", team)?;
    let name = ctx.default_name();
    let mut config = Config::load()?;
    config.put_context(&name, ctx);
    config.save()?;
    println!("Switched to context {name}");
    Ok(())
}

/// The sign-up service `server` names (`memstore_signup_url` in its protected-resource metadata): where a
/// person it signed in, in no org yet, makes their own. Only an https URL, or http on this machine: it is sent
/// the sign-in's ID token.
async fn signup_url(server: &str) -> Option<String> {
    let meta = get_json(&format!("{server}/.well-known/oauth-protected-resource"))
        .await
        .ok()?;
    let url = reqwest::Url::parse(meta["memstore_signup_url"].as_str()?).ok()?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    (url.scheme() == "https" || (url.scheme() == "http" && local)).then(|| url.to_string())
}

/// Make the signed-in person's own org at the sign-up service `url` (`POST` with their ID token), and leave
/// them signed in to it. Returns its name.
///
/// The org is made with them as its admin, so the sign-in they have, which names no Auth0 Organization,
/// already counts there: the directory lists them. Where the server doesn't say so within a few seconds,
/// they sign in again for the org's own Organization, which counts there in any case.
async fn make_own_org(
    server: &str,
    url: &str,
    creds: &Credentials,
    no_browser: bool,
) -> Result<String> {
    let mut held = creds.clone();
    held.bearer().await?;
    let token = held
        .id_token
        .clone()
        .context("the sign-in gave no ID token, which making an org needs")?;
    println!("Making your own memory org...");
    let mut tries = 0;
    let made = loop {
        tries += 1;
        let r = http()
            .post(url)
            .bearer_auth(&token)
            .json(&json!({}))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = r.status();
        let wait = r
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let body: Value = r.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            break body;
        }
        let why = body["error"].as_str().unwrap_or("no reason given");
        // Still being made by another request, or a step that failed part way: asking again finishes it.
        if (status.as_u16() == 503 || status.as_u16() == 502) && tries < 4 {
            tokio::time::sleep(Duration::from_secs(wait.unwrap_or(3).min(10))).await;
            continue;
        }
        anyhow::bail!("{} couldn't make an org of their own: {why}", creds.user);
    };
    let org = made["org"]
        .as_str()
        .context("the sign-up service answered no org")?
        .to_string();
    match made["expiresAt"].as_str() {
        Some(end) => println!(
            "Made {org} for {}: the {} plan, a trial until {end}",
            creds.user,
            made["plan"].as_str().unwrap_or("starter")
        ),
        None => println!("{org} is {}'s already", creds.user),
    }
    if let Some(at) = made["memstoreUrl"].as_str()
        && at.trim_end_matches('/') != server
    {
        eprintln!("(the sign-up service says {org} is served at {at}, not {server})");
    }

    // The directory lists them in it now, or within a few seconds on another replica.
    for _ in 0..10 {
        if let Ok(me) = call_as(
            server,
            None,
            None,
            None,
            "GET",
            "/me",
            None,
            Duration::from_secs(30),
        )
        .await
            && me["memberships"].get(&org).is_some()
        {
            return Ok(org);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    println!("Signing in to {org} itself...");
    let opts = LoginOptions {
        no_browser,
        org: Some(org.clone()),
        sso: true,
        ..Default::default()
    };
    let again = login(Some(server), &opts).await?;
    let me = call_as(
        server,
        again.org(),
        Some(&org),
        None,
        "GET",
        "/me",
        None,
        Duration::from_secs(30),
    )
    .await?;
    anyhow::ensure!(
        me["memberships"].get(&org).is_some(),
        "{org} was made, but {server} doesn't count {} in it yet: run this again in a minute",
        again.user
    );
    Ok(org)
}

/// A yes or no from the person at the terminal (an empty answer is yes), or None where there is no terminal.
fn confirm(prompt: &str) -> Result<Option<bool>> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(None);
    }
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(Some(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "" | "y" | "yes"
    )))
}

/// A line from the person at the terminal, or None where there is no terminal (a hook, the plugin's command)
/// or they gave none.
fn ask(prompt: &str) -> Result<Option<String>> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(None);
    }
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim();
    Ok((!line.is_empty()).then(|| line.to_string()))
}

/// One of `options`, chosen at the terminal by number or name; None where there is no terminal.
fn choose(prompt: &str, options: &[String]) -> Result<Option<String>> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(None);
    }
    println!("{prompt}");
    for (i, o) in options.iter().enumerate() {
        println!("  {}. {o}", i + 1);
    }
    let Some(a) = ask("Number or name: ")? else {
        return Ok(None);
    };
    let picked = a
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| options.get(i))
        .or_else(|| options.iter().find(|o| **o == a));
    match picked {
        Some(o) => Ok(Some(o.clone())),
        None => anyhow::bail!("{a} isn't one of them ({})", options.join(", ")),
    }
}

/// Ask to join `org` on `server` as the signed-in person (the directory doesn't hold them there): the org's
/// admins are emailed to approve. Asked already: says so. Once approved, the next `join` gets in.
async fn request_join(
    server: &str,
    signed_in: Option<&str>,
    org: &str,
    user: &str,
    me: &Value,
) -> Result<()> {
    let waiting = me["pending"]
        .as_array()
        .is_some_and(|p| p.iter().any(|o| o == org));
    if waiting {
        println!(
            "{user} asked to join {org} on {server} already, and an admin hasn't approved it yet. Once they do, run this again."
        );
        return Ok(());
    }
    let body = json!({"op": "request_join", "org": org});
    match call_as(
        server,
        signed_in,
        None,
        None,
        "POST",
        "/admin",
        Some(body),
        Duration::from_secs(30),
    )
    .await
    {
        Ok(_) => println!(
            "{user} isn't a member of {org} on {server} yet: asked to join it. The org's admins have been emailed to approve; once they do, run this again."
        ),
        Err(e) => anyhow::bail!(
            "{user} isn't a member of {org} on {server}, and couldn't ask to join: {e:#}"
        ),
    }
    Ok(())
}

/// How to sign in, with the command this machine has: memstore-client installed alone has no `auth login`.
pub fn sign_in_hint() -> &'static str {
    let me = std::env::current_exe().ok();
    match me
        .as_deref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
    {
        Some(n)
            if n.starts_with(crate::naming::CLIENT_BIN)
                || n.starts_with(crate::naming::OLD_CLIENT_BIN) =>
        {
            "`ampup-memory-client join <server> --org <org>`"
        }
        _ => "`ampup-memory auth login`",
    }
}
