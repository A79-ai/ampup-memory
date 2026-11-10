//! What runs on a person's machine to reach a team's memory: who they are and which memory they want
//! ([`client`]), and the coding agents' hooks and MCP bridge ([`hook`]). memstore-core re-exports [`client`] as
//! `people::client` and memstore re-exports [`hook`], so either path reaches them.

use anyhow::Result;

pub mod client;
pub mod hook;
pub mod update;

/// API keys start with this, so a bearer is told from a JWT without trying both.
pub const KEY_PREFIX: &str = "msk_";

/// Where starter documents live: what the hooks import from a repo (`starter/<repo>/<path>`) and from a person's
/// agent memory files (`starter/<repo>/memory/<file>`, in their own memory). Extracted even where nothing else
/// no goal reads is.
pub const STARTER: &str = "starter/";

/// Org and team names become part of a namespace, which every catalog (BigQuery datasets, Snowflake
/// schemas) must accept: lower-case letters, digits and single underscores, at most 48 characters.
/// `__` is the separator between an org and its team, so no name holds one.
pub fn check_name(kind: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !name.starts_with('_')
        && !name.ends_with('_')
        && !name.contains("__");
    anyhow::ensure!(
        ok,
        "invalid {kind} name {name:?}: use lower-case letters, digits and single underscores (at most 48)"
    );
    Ok(())
}
