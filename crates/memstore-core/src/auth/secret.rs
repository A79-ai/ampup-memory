//! Secrets the directory refers to but never holds: an org's identity-provider client secret is stored as
//! `env:AMPUP_MEMORY_IDP_SECRET_<NAME>` (that variable in the server's environment holds it; no other) or `sealed:<base64 nonce+ciphertext>`
//! (AES-256-GCM under the server's `AMPUP_MEMORY_SECRET_KEY`, bound to the org). The directory is an Iceberg
//! table anyone with read access to the bucket can read, so a literal secret never goes into it.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Context, Result};
use base64::Engine;

/// The server's key for sealing secrets: 32 bytes, base64 or hex.
pub const KEY_ENV: &str = "AMPUP_MEMORY_SECRET_KEY";
const SEALED: &str = "sealed:";
const ENV: &str = "env:";
const NONCE: usize = 12;

#[derive(Clone)]
pub struct SecretKey(Key<Aes256Gcm>);

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(..)")
    }
}

impl SecretKey {
    /// 32 bytes as base64 (standard or URL-safe, padded or not) or 64 hex digits.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        let hex = (s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| {
            (0..32)
                .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
                .collect::<Vec<u8>>()
        });
        let bytes = hex.or_else(|| {
            use base64::engine::general_purpose::{
                STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD,
            };
            [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD]
                .iter()
                .find_map(|e| e.decode(s).ok())
        });
        match bytes {
            Some(b) if b.len() == 32 => Ok(SecretKey(*Key::<Aes256Gcm>::from_slice(&b))),
            _ => anyhow::bail!(
                "{KEY_ENV} must be 32 bytes, base64 or hex (e.g. `openssl rand -base64 32`)"
            ),
        }
    }

    /// `AMPUP_MEMORY_SECRET_KEY`, if set.
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var(KEY_ENV).ok().filter(|v| !v.trim().is_empty()) {
            None => Ok(None),
            Some(v) => Self::parse(&v).map(Some),
        }
    }

    /// `sealed:<base64 nonce+ciphertext>`, bound to `context` (the org): it opens for that org only.
    pub fn seal(&self, context: &str, plain: &str) -> String {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = Aes256Gcm::new(&self.0)
            .encrypt(
                &nonce,
                Payload {
                    msg: plain.as_bytes(),
                    aad: context.as_bytes(),
                },
            )
            .expect("AES-GCM encrypts any message this size");
        let mut out = nonce.to_vec();
        out.extend(ct);
        format!(
            "{SEALED}{}",
            base64::engine::general_purpose::STANDARD.encode(out)
        )
    }

    /// What [`Self::seal`] made (with its `sealed:` prefix), opened under the same `context`: a token this
    /// server issued, as it issued it (AES-GCM refuses one changed by a bit), or an error.
    pub fn unseal(&self, context: &str, token: &str) -> Result<String> {
        let sealed = token
            .strip_prefix(SEALED)
            .ok_or_else(|| anyhow::anyhow!("not a sealed token"))?;
        self.open(context, sealed)
    }

    fn open(&self, context: &str, sealed: &str) -> Result<String> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(sealed)
            .context("not base64")?;
        anyhow::ensure!(raw.len() > NONCE, "too short");
        let (nonce, ct) = raw.split_at(NONCE);
        let plain = Aes256Gcm::new(&self.0)
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ct,
                    aad: context.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("it doesn't open with this server's {KEY_ENV}"))?;
        Ok(String::from_utf8(plain)?)
    }
}

/// How a stored secret is kept.
#[derive(Debug, PartialEq, Eq)]
pub enum Stored<'a> {
    /// In the server's environment variable.
    Env(&'a str),
    /// Sealed with the server's key.
    Sealed(&'a str),
    /// In plain text: written before secrets were kept out of the directory.
    Plain,
}

pub fn stored(s: &str) -> Stored<'_> {
    if let Some(n) = s.strip_prefix(ENV) {
        Stored::Env(n)
    } else if let Some(c) = s.strip_prefix(SEALED) {
        Stored::Sealed(c)
    } else {
        Stored::Plain
    }
}

/// The only environment variables an `env:` reference may name. An org's admin sets the reference, and
/// the secret it resolves to is served to anyone who asks how to sign in to that org: a reference to any
/// other variable (`AMPUP_MEMORY_SECRET_KEY`, cloud credentials) would hand the server's environment out.
pub const ENV_PREFIX: &str = "AMPUP_MEMORY_IDP_SECRET_";

/// A reference's variable by its current name: one written before the rename names `MEMSTORE_IDP_SECRET_…`,
/// which the server's environment holds as `AMPUP_MEMORY_IDP_SECRET_…` (memstore_client::naming), and is held to
/// the same rules as that name.
fn current_env_name(n: &str) -> std::borrow::Cow<'_, str> {
    crate::naming::env_name(n)
}

fn allowed_env_name(n: &str) -> bool {
    current_env_name(n)
        .strip_prefix(ENV_PREFIX)
        .is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// Whether `n` is one of `org`'s variables: `AMPUP_MEMORY_IDP_SECRET_<ORG>`, or that and `__<SUFFIX>` (org
/// names hold single underscores only, so no org's name runs into another's).
fn org_env_name(org: &str, n: &str) -> bool {
    let n = current_env_name(n);
    let own = format!("{ENV_PREFIX}{}", org.to_ascii_uppercase());
    n == own
        || n.strip_prefix(&own)
            .and_then(|r| r.strip_prefix("__"))
            .is_some_and(|r| !r.is_empty())
}

fn env_name_refused(n: &str) -> String {
    format!(
        "env:{n} is not allowed: a client secret's variable is named {ENV_PREFIX}<NAME> (capitals, digits \
         and _), e.g. env:{ENV_PREFIX}ACME"
    )
}

/// What `secret` (as given to `memstore auth setup`) is stored as: an `env:AMPUP_MEMORY_IDP_SECRET_…`
/// reference as is, a sealed one if it opens with `key`, a literal sealed with `key`. Without a key, a
/// literal is refused.
pub fn protect(context: &str, secret: &str, key: Option<&SecretKey>) -> Result<String, String> {
    match stored(secret) {
        Stored::Env(n) if !allowed_env_name(n) => Err(env_name_refused(n)),
        // Bound to the org, as a sealed secret is: an org's admin names their org's variable, never
        // another org's (whose secret the server would then hand out as theirs).
        Stored::Env(n) if !org_env_name(context, n) => Err(format!(
            "env:{n} is not org {context}'s: its client secret's variable is {ENV_PREFIX}{} (or {ENV_PREFIX}{}__<SUFFIX>)",
            context.to_ascii_uppercase(),
            context.to_ascii_uppercase()
        )),
        Stored::Env(_) => Ok(secret.to_string()),
        Stored::Sealed(c) => match key {
            Some(k) => k
                .open(context, c)
                .map(|_| secret.to_string())
                .map_err(|e| format!("the sealed client secret is no good: {e:#}")),
            None => Err(format!(
                "this server has no {KEY_ENV}, so it can't open a sealed client secret"
            )),
        },
        Stored::Plain => match key {
            Some(k) => Ok(k.seal(context, secret)),
            None => Err(format!(
                "this server keeps no client secret in plain text and has no {KEY_ENV} to seal it with: \
                 set {KEY_ENV} on the server (32 bytes, base64 or hex: `openssl rand -base64 32`), or pass \
                 --client-secret env:{ENV_PREFIX}<NAME> and set that variable in the server's environment"
            )),
        },
    }
}

/// The server's environment, as `env:` references read it.
pub fn process_env(n: &str) -> Option<String> {
    std::env::var(n).ok()
}

/// The secret a stored value stands for: the environment variable's value (`env` looks it up; only a
/// `AMPUP_MEMORY_IDP_SECRET_…` variable, whatever a row says), the sealed secret opened, or (written before
/// secrets were kept out of the directory) the value itself.
pub fn resolve(
    context: &str,
    stored_value: &str,
    key: Option<&SecretKey>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String> {
    match stored(stored_value) {
        Stored::Env(n) if !allowed_env_name(n) => anyhow::bail!(env_name_refused(n)),
        Stored::Env(n) => env(&current_env_name(n))
            .filter(|v| !v.is_empty())
            .with_context(|| format!("{n} is not set in the server's environment")),
        Stored::Sealed(c) => key
            .with_context(|| format!("{KEY_ENV} is not set, so the sealed secret can't be opened"))?
            .open(context, c),
        Stored::Plain => Ok(stored_value.to_string()),
    }
}

/// How a stored secret shows in views: never the secret or its sealed form.
pub fn shown(stored_value: &str) -> String {
    match stored(stored_value) {
        Stored::Env(n) => format!("{ENV}{n}"),
        Stored::Sealed(_) => "set".into(),
        Stored::Plain => "set, in plain text: run `ampup-memory auth setup` again".into(),
    }
}
