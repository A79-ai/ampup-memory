//! A change to a CRM record. The body is JSON: the record (`object`, `id`) and its `changes` (`field`, `value`,
//! `changed_at`), or a synced record (`hubspot:deal:42`, `dynamicsCRM:account:7`) whose body is its fields as one
//! JSON object, read as a change to each field. No model reads it: each change is written by the engine as a fact on `<object>:hs-<id>`, citing
//! the field. Its catalog types the fields the engine writes.

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
        Ok(b.changes
            .into_iter()
            .enumerate()
            .map(|(i, c)| Unit {
                index: i,
                speaker: None,
                text: format!("{} = {}", c.field, c.value),
                cite: Cite::Field {
                    object: format!("{}:hs-{}", b.object, b.id),
                    field: c.field,
                    changed_at: c.changed_at,
                },
            })
            .collect())
    }
    /// The record it changes, as its subject.
    fn header(&self, ep: &Episode) -> Header {
        Header {
            subject: Self::body(ep)
                .ok()
                .map(|b| format!("{}:hs-{}", b.object, b.id)),
            ..Header::default()
        }
    }
    fn engine_facts(&self, ep: &Episode, _units: &[Unit]) -> Result<Vec<EngineFact>> {
        let b = Self::body(ep)?;
        let subject = format!("{}:hs-{}", b.object, b.id);
        Ok(b.changes
            .into_iter()
            .map(|c| EngineFact {
                subject: subject.clone(),
                predicate: c.field.clone(),
                value: match c.value {
                    serde_json::Value::String(s) => s,
                    v => v.to_string(),
                },
                cite: Cite::Field {
                    object: subject.clone(),
                    field: c.field,
                    changed_at: c.changed_at,
                },
            })
            .collect())
    }
}
