//! Mail the server sends: an org's admins are told when someone asks to join ([`crate::server::join`]).
//!
//! Through Resend (`RESEND_API_KEY`) or MailerSend (`MAILERSEND_API_KEY`), from `AMPUP_MEMORY_MAIL_FROM`. With
//! neither key, or no sender, nothing is sent: what would have been mailed is logged, and the request stays
//! recorded for `memstore requests`. `AMPUP_MEMORY_MAIL_URL` sends the provider's request elsewhere (a relay, or a
//! sink when checking locally).

use anyhow::{Context, Result};
use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    Resend,
    MailerSend,
}

/// A configured sender.
#[derive(Clone, Debug)]
pub struct Mailer {
    provider: Provider,
    key: String,
    from: String,
}

/// The sender the environment configures, if it configures one.
pub fn from_env() -> Option<Mailer> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let from = var("AMPUP_MEMORY_MAIL_FROM")?;
    let (provider, key) = var("RESEND_API_KEY")
        .map(|k| (Provider::Resend, k))
        .or_else(|| var("MAILERSEND_API_KEY").map(|k| (Provider::MailerSend, k)))?;
    Some(Mailer {
        provider,
        key,
        from,
    })
}

impl Mailer {
    /// The provider's name, for the log.
    pub fn provider(&self) -> &'static str {
        match self.provider {
            Provider::Resend => "resend",
            Provider::MailerSend => "mailersend",
        }
    }

    /// Send one message. The body is never logged: a link in it is the secret.
    pub async fn send(&self, to: &str, subject: &str, text: &str, html: &str) -> Result<()> {
        let (url, body) = match self.provider {
            Provider::Resend => (
                "https://api.resend.com/emails",
                json!({"from": self.from, "to": [to], "subject": subject, "text": text, "html": html}),
            ),
            Provider::MailerSend => (
                "https://api.mailersend.com/v1/email",
                json!({"from": {"email": self.from}, "to": [{"email": to}], "subject": subject,
                       "text": text, "html": html}),
            ),
        };
        let url = std::env::var("AMPUP_MEMORY_MAIL_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| url.to_string());
        let r = crate::deployment::client()
            .post(&url)
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{} didn't answer", self.provider()))?;
        let status = r.status();
        anyhow::ensure!(status.is_success(), "{} answered {status}", self.provider());
        Ok(())
    }
}

/// Text as HTML shows it.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}
