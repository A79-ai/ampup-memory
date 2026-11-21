//! A change to a CRM record. The body is JSON: the record (`object`, `id`) and its `changes` (`field`, `value`,
//! `changed_at`), or a synced record (`hubspot:deal:42`, `dynamicsCRM:account:7`) whose body is its fields as one
//! JSON object, read as a change to each field. No model reads it: each change is written by the engine as a fact on
//! `<object>:<p>-<id>`, citing the field, where `p` names the CRM the source's first segment does ([`provider`]):
//! `deal:hs-42`, `deal:sf-006X`, `account:d365-7`. Values are written as the record gives them, each field a fact of
//! its own (a stage's id, `stage_id`, too): the connector sends labels, not a CRM's codes, and nothing here maps
//! one, but for the record's `currency`, written as its ISO code. Its catalog types the fields the engine writes.
//!
//! A person's row (a contact, a lead, a rep) that carries an email is also known by the one id every connector
//! gives that person, `person:<email>` (lower case), as its `_alias`; so is a record's owner (`owner`, a `rep:`
//! id) by its `owner_email`, where the episode is about the owner too (the engine keeps only facts about what an
//! episode is about).

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{Cite, EngineFact, EpisodeType, Format, Header, Unit};
use crate::Episode;

pub struct CrmUpdate;

#[derive(Deserialize)]
struct Body {
    object: String,
    id: String,
    changes: Vec<Change>,
}

#[derive(Deserialize)]
struct Change {
    field: String,
    value: serde_json::Value,
    #[serde(default)]
    changed_at: Option<String>,
}

/// The short code a CRM's ids carry, from the source's first segment: `hs` (HubSpot), `sf` (Salesforce), `d365`
/// (Dynamics), and `crm` for a generic `crm:` source. A connector writing the same record's id elsewhere (an
/// email's `about`) uses the same code, so the two join.
fn provider(source: &str) -> &'static str {
    match source.split(':').next().unwrap_or_default() {
        "hubspot" => "hs",
        "salesforce" => "sf",
        "dynamicsCRM" => "d365",
        _ => "crm",
    }
}

/// The objects whose row is a person's: an email on one names them.
const PEOPLE: [&str; 3] = ["contact", "lead", "rep"];

/// The fields a person's email comes in: the connector's mapped name, or a CRM's own when none was mapped.
const EMAIL: [&str; 2] = ["email", "emailaddress1"];

/// The owner's email, beside a record's `owner`.
const OWNER_EMAIL: &str = "owner_email";

/// The record's currency, as its own field.
const CURRENCY: &str = "currency";

/// A currency as its ISO 4217 code: a code as given (`eur` is `EUR`), or a CRM's name for one (Dynamics sends
/// "US Dollar"). A name not listed is kept as given.
fn currency(v: &str) -> String {
    let t = v.trim();
    if t.len() == 3 && t.chars().all(|c| c.is_ascii_alphabetic()) {
        return t.to_ascii_uppercase();
    }
    let code = match t.to_lowercase().as_str() {
        "us dollar" | "u.s. dollar" | "united states dollar" => "USD",
        "euro" => "EUR",
        "british pound" | "pound sterling" | "british pound sterling" => "GBP",
        "canadian dollar" => "CAD",
        "australian dollar" => "AUD",
        "new zealand dollar" => "NZD",
        "japanese yen" => "JPY",
        "swiss franc" => "CHF",
        "indian rupee" => "INR",
        "chinese yuan" | "chinese yuan renminbi" => "CNY",
        "singapore dollar" => "SGD",
        "hong kong dollar" => "HKD",
        "swedish krona" => "SEK",
        "norwegian krone" => "NOK",
        "danish krone" => "DKK",
        "mexican peso" => "MXN",
        "brazilian real" => "BRL",
        "south african rand" => "ZAR",
        _ => return t.to_string(),
    };
    code.to_string()
}

/// `v` as one email address, lower case: `person:<it>` is the person. None for anything else (a list, a name).
fn address(v: &serde_json::Value) -> Option<String> {
    let a = v.as_str()?.trim().to_lowercase();
    let (local, domain) = a.split_once('@')?;
    (!local.is_empty()
        && domain.contains('.')
        && !domain.contains('@')
        && !a.contains(|c: char| c.is_whitespace() || c == ',' || c == ';'))
    .then_some(a)
}

impl Body {
    /// The record's id: `<object>:<p>-<id>`.
    fn subject(&self, source: &str) -> String {
        format!("{}:{}-{}", self.object, provider(source), self.id)
    }
}

impl CrmUpdate {
    /// The change: `{object, id, changes}`, or a record as a connector syncs it (`hubspot:deal:42` with the
    /// record's fields as one JSON object), read as a change to each of its fields.
    fn body(ep: &Episode) -> Result<Body> {
        let v: serde_json::Value =
            serde_json::from_slice(&ep.body).context("crm_update body: not JSON")?;
        let serde_json::Value::Object(fields) = v else {
            anyhow::bail!("crm_update body: not a record (a JSON object)");
        };
        if fields.contains_key("changes") {
            return serde_json::from_value(serde_json::Value::Object(fields))
                .context("crm_update body: not a record change");
        }
        // A synced record: its object and id are in its source, `<provider>:<object>:<id>`.
        // The last two segments: `hubspot:deal:42`, and `crm:hubspot:deal:42` too.
        let mut parts = ep.source.rsplitn(3, ':');
        let (Some(id), Some(object), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
            anyhow::bail!(
                "crm_update body: a record with no changes needs a source <provider>:<object>:<id>"
            );
        };
        anyhow::ensure!(
            !object.is_empty() && !id.is_empty(),
            "crm_update body: a record with no changes needs a source <provider>:<object>:<id>"
        );
        Ok(Body {
            object: object.to_string(),
            id: id.to_string(),
            changes: fields
                .into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(field, value)| Change {
                    field,
                    value,
                    changed_at: None,
                })
                .collect(),
        })
    }

    /// The `_alias` facts naming who a row's people are by `person:<email>`: the row's own, when it is a
    /// person's and carries an email, and its owner's (the `rep:` id its `owner` gives) by its `owner_email`.
    fn aliases(b: &Body, subject: &str) -> Vec<EngineFact> {
        let field = |c: &Change| Cite::Field {
            object: subject.to_string(),
            field: c.field.clone(),
            changed_at: c.changed_at.clone(),
        };
        let alias = |who: &str, email: String, cite: Cite| EngineFact {
            subject: who.to_string(),
            predicate: crate::query::schema::ALIAS.into(),
            value: format!("person:{email}"),
            cite,
        };
        let mut out = vec![];
        if PEOPLE.contains(&b.object.as_str())
            && let Some((c, email)) = b.changes.iter().find_map(|c| {
                EMAIL
                    .iter()
                    .any(|f| c.field.eq_ignore_ascii_case(f))
                    .then(|| address(&c.value).map(|a| (c, a)))
                    .flatten()
            })
        {
            out.push(alias(subject, email, field(c)));
        }
        let owner = b
            .changes
            .iter()
            .filter(|c| c.field == "owner")
            .find_map(|c| c.value.as_str())
            .map(str::trim)
            .filter(|o| o.strip_prefix("rep:").is_some_and(|id| !id.is_empty()));
        if let Some(owner) = owner
            && owner != subject
            && let Some((c, email)) = b
                .changes
                .iter()
                .filter(|c| c.field == OWNER_EMAIL)
                .find_map(|c| address(&c.value).map(|a| (c, a)))
        {
            out.push(alias(owner, email, field(c)));
        }
        out
    }
}

impl EpisodeType for CrmUpdate {
    fn name(&self) -> &'static str {
        "crm_update"
    }
    fn format(&self) -> Format {
        Format::Record
    }
    fn source_prefixes(&self) -> &'static [&'static str] {
        &["hubspot:", "salesforce:", "dynamicsCRM:", "crm:"]
    }
    fn catalog_json(&self) -> &'static str {
        include_str!("catalog.json")
    }
    fn engine_predicates(&self) -> &'static [&'static str] {
        &[
            "stage",
            "amount",
            "close_date",
            "owner",
            "title",
            "email",
            "industry",
            "domain",
            crate::query::schema::ALIAS,
        ]
    }
    fn uses_model(&self) -> bool {
        false
    }
    fn instructions(&self) -> &'static str {
        "A CRM record change: written field by field by the engine, never read by a model."
    }
    fn units(&self, ep: &Episode) -> Result<Vec<Unit>> {
        let b = Self::body(ep)?;
        let subject = b.subject(&ep.source);
        Ok(b.changes
            .into_iter()
            .enumerate()
            .map(|(i, c)| Unit {
                index: i,
                speaker: None,
                text: format!("{} = {}", c.field, c.value),
                cite: Cite::Field {
                    object: subject.clone(),
                    field: c.field,
                    changed_at: c.changed_at,
                },
            })
            .collect())
    }
    /// The record it changes, as its subject.
    fn header(&self, ep: &Episode) -> Header {
        Header {
            subject: Self::body(ep).ok().map(|b| b.subject(&ep.source)),
            ..Header::default()
        }
    }
    /// Each field's new value on the record, and who its people are by email ([`CrmUpdate::aliases`]).
    fn engine_facts(&self, ep: &Episode, _units: &[Unit]) -> Result<Vec<EngineFact>> {
        let b = Self::body(ep)?;
        let subject = b.subject(&ep.source);
        let aliases = Self::aliases(&b, &subject);
        let mut out: Vec<EngineFact> = b
            .changes
            .into_iter()
            .map(|c| EngineFact {
                subject: subject.clone(),
                predicate: c.field.clone(),
                value: match c.value {
                    serde_json::Value::String(s) if c.field == CURRENCY => currency(&s),
                    serde_json::Value::String(s) => s,
                    v => v.to_string(),
                },
                cite: Cite::Field {
                    object: subject.clone(),
                    field: c.field,
                    changed_at: c.changed_at,
                },
            })
            .collect();
        out.extend(aliases);
        Ok(out)
    }
}
