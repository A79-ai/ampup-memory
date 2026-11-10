//! Reading an org's identity provider: its discovery document and its signing keys. An org's admin names
//! the issuer, and the provider's document names the keys' URL, so both are untrusted: the server must
//! not become a way to reach hosts only it can reach (its cloud's metadata service, the private network,
//! itself).
//!
//! Every URL, and every redirect hop, is vetted the same way: https only, no userinfo, and every address
//! its host resolves to must be a public one. The connection is then pinned to the vetted address (the
//! client's resolver answers that address and nothing else, and no proxy is used), so a DNS answer that
//! changes between the check and the connection (rebinding) can't send it elsewhere.
//!
//! What goes wrong is logged here, and the caller gets [`UNREADABLE`] whatever it was: refused address,
//! no such host, connection refused, timeout, bad status or bad JSON. The answer is then the same whether
//! or not a host is up, so an org's admin can't use the server to map a network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::LOCATION;

/// All the caller of an org-provider fetch learns when it fails. The server's log says why.
pub const UNREADABLE: &str =
    "its discovery document or signing keys couldn't be read (the server's log says why)";

/// Redirects followed (each hop vetted like the first URL).
const MAX_REDIRECTS: usize = 3;
/// A discovery document or key set is a few kilobytes.
const MAX_BODY: usize = 1 << 20;
const TIMEOUT: Duration = Duration::from_secs(10);

/// How a provider's URLs are fetched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchPolicy {
    /// The operator configured it (the server's own provider, IAP): read as it is.
    Trusted,
    /// An org's admin named it: vetted, pinned, generic errors. `allow_loopback`
    /// (`AMPUP_MEMORY_ALLOW_LOCAL_ISSUERS`) also lets it be on this machine, over http too.
    Vetted { allow_loopback: bool },
}

impl FetchPolicy {
    /// Read `url` as JSON. `what` names it in the server's log.
    pub async fn get_json<T: serde::de::DeserializeOwned>(
        self,
        url: &str,
        what: &str,
    ) -> Result<T> {
        match self {
            FetchPolicy::Trusted => trusted_get(url).await.context(what.to_string()),
            FetchPolicy::Vetted { allow_loopback } => match vetted_get(url, allow_loopback).await {
                Ok(v) => Ok(v),
                Err(e) => {
                    eprintln!("org identity provider: {what} at {url}: {e:#}");
                    Err(anyhow::anyhow!(UNREADABLE))
                }
            },
        }
    }
}

/// For reading an operator-configured provider: no redirects, and a provider that hangs doesn't hang
/// requests for long.
fn provider_http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
        .expect("http client")
}

async fn trusted_get<T: serde::de::DeserializeOwned>(url: &str) -> Result<T> {
    Ok(provider_http()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn vetted_get<T: serde::de::DeserializeOwned>(url: &str, allow_loopback: bool) -> Result<T> {
    let mut url = Url::parse(url).context("not a URL")?;
    for _ in 0..=MAX_REDIRECTS {
        let addr = vet(&url, allow_loopback).await?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            // A proxy would resolve the name itself, after the check.
            .no_proxy()
            .dns_resolver(Arc::new(Pinned(addr)))
            .build()?;
        let mut r = client.get(url.clone()).send().await?;
        if r.status().is_redirection() {
            let to = r
                .headers()
                .get(LOCATION)
                .and_then(|l| l.to_str().ok())
                .context("a redirect without a Location")?;
            url = url.join(to).context("a redirect to a bad URL")?;
            continue;
        }
        let status = r.status();
        anyhow::ensure!(status.is_success(), "HTTP {status}");
        let mut body = Vec::new();
        while let Some(chunk) = r.chunk().await? {
            body.extend_from_slice(&chunk);
            anyhow::ensure!(
                body.len() <= MAX_BODY,
                "the answer is over {MAX_BODY} bytes"
            );
        }
        return Ok(serde_json::from_slice(&body)?);
    }
    anyhow::bail!("more than {MAX_REDIRECTS} redirects")
}

/// The one address `url` may be fetched from, or why it may not be fetched at all.
async fn vet(url: &Url, allow_loopback: bool) -> Result<SocketAddr> {
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "credentials in the URL"
    );
    let http = match url.scheme() {
        "https" => false,
        "http" if allow_loopback => true,
        s => anyhow::bail!("scheme {s}: only https"),
    };
    let port = url.port_or_known_default().context("no port")?;
    // The URL parser has already read decimal, octal and hex IPv4 forms (`2130706433`, `0177.0.0.1`,
    // `0x7f.1`) as the address they mean.
    let addrs: Vec<IpAddr> = match url.host().context("no host")? {
        url::Host::Ipv4(ip) => vec![IpAddr::V4(ip)],
        url::Host::Ipv6(ip) => vec![IpAddr::V6(ip)],
        url::Host::Domain(d) => tokio::time::timeout(TIMEOUT, tokio::net::lookup_host((d, port)))
            .await
            .with_context(|| format!("resolving {d} timed out"))?
            .with_context(|| format!("resolving {d}"))?
            .map(|a| a.ip())
            .collect(),
    };
    anyhow::ensure!(!addrs.is_empty(), "the host has no address");
    // Every address must pass, not just one: the connection would otherwise be free to try the others.
    for ip in &addrs {
        let local = allow_loopback && is_loopback(*ip);
        if let Some(why) = refused(*ip).filter(|_| !local) {
            anyhow::bail!("{ip} is {why}");
        }
        anyhow::ensure!(!http || local, "http to {ip}, which isn't this machine");
    }
    Ok(SocketAddr::new(addrs[0], port))
}

/// The resolver of a vetted fetch: whatever the name, the vetted address.
struct Pinned(SocketAddr);

impl Resolve for Pinned {
    fn resolve(&self, _: Name) -> Resolving {
        let addr = self.0;
        Box::pin(async move { Ok(Box::new(std::iter::once(addr)) as Addrs) })
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Why `ip` is no place for the server to fetch from, if it isn't a public address.
pub fn refused(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => refused_v4(v4),
        IpAddr::V6(v6) => refused_v6(v6),
    }
}

fn refused_v4(ip: Ipv4Addr) -> Option<&'static str> {
    Some(match ip.octets() {
        [0, ..] => "unspecified (0.0.0.0/8)",
        [10, ..] => "private (10.0.0.0/8)",
        [100, b, ..] if (64..128).contains(&b) => "shared address space (CGNAT, 100.64.0.0/10)",
        [127, ..] => "loopback (127.0.0.0/8)",
        [169, 254, ..] => "link-local (169.254.0.0/16, the cloud metadata service's)",
        [172, b, ..] if (16..32).contains(&b) => "private (172.16.0.0/12)",
        [192, 0, 0, _] => "IETF protocol assignments (192.0.0.0/24)",
        [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _] => "documentation",
        [192, 88, 99, _] => "6to4 relay anycast (192.88.99.0/24)",
        [192, 168, ..] => "private (192.168.0.0/16)",
        [198, 18 | 19, ..] => "benchmarking (198.18.0.0/15)",
        [224..=239, ..] => "multicast (224.0.0.0/4)",
        [240..=255, ..] => "reserved or broadcast (240.0.0.0/4)",
        _ => return None,
    })
}

fn refused_v6(ip: Ipv6Addr) -> Option<&'static str> {
    let s = ip.segments();
    let embedded = |hi: u16, lo: u16| Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
    match s {
        // IPv4-mapped (::ffff:a.b.c.d) and IPv4-translated (::ffff:0:a.b.c.d): the IPv4 address decides.
        [0, 0, 0, 0, 0, 0xffff, hi, lo] | [0, 0, 0, 0, 0xffff, 0, hi, lo] => {
            refused_v4(embedded(hi, lo))
        }
        // ::, ::1 and the deprecated IPv4-compatible ::a.b.c.d.
        [0, 0, 0, 0, 0, 0, _, _] => Some("unspecified, loopback or IPv4-compatible (::/96)"),
        // NAT64 (64:ff9b::/96) reaches the IPv4 address it embeds.
        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => refused_v4(embedded(hi, lo)),
        // 6to4 (2002::/16) embeds an IPv4 address too.
        [0x2002, hi, lo, ..] => refused_v4(embedded(hi, lo)),
        // Teredo (2001::/32) embeds an obfuscated one: no IdP is reached that way.
        [0x2001, 0, ..] => Some("Teredo (2001::/32)"),
        [0x2001, 0xdb8, ..] => Some("documentation (2001:db8::/32)"),
        [0x2001, 2, 0, ..] => Some("benchmarking (2001:2::/48)"),
        [x, ..] if x & 0xfe00 == 0xfc00 => Some("unique local (fc00::/7)"),
        [x, ..] if x & 0xffc0 == 0xfe80 => Some("link-local (fe80::/10)"),
        [x, ..] if x & 0xffc0 == 0xfec0 => Some("site-local (fec0::/10)"),
        [x, ..] if x & 0xff00 == 0xff00 => Some("multicast (ff00::/8)"),
        // Global unicast is 2000::/3; anything else (discard-only 100::/64, local NAT64, ...) is not.
        [x, ..] if x & 0xe000 != 0x2000 => Some("not global unicast (outside 2000::/3)"),
        _ => None,
    }
}
