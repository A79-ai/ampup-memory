//! Which org and user own each connector installation (an Ampersand connection), recorded by the
//! backend that created it: `memory_system.installations`, an append-only table of registrations and
//! revocations folded into the current state on read. Connectors trust a delivery's owner
//! (`groupRef`) only when it is exactly the one registered here.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::Serialize;

use crate::auth::is_acl_id;

/// One registration or revocation, as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// When it was recorded (microseconds); set by the store, increasing.
    pub at: i64,
    pub actor: String,
    /// `register` or `revoke`.
    pub op: String,
    pub installation_id: String,
    pub provider: Option<String>,
    pub group_ref: Option<String>,
    /// The person a service acted for, as it named them (`actor` is always the key that wrote it).
    pub on_behalf_of: Option<String>,
}

/// An installation as it stands now.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Installation {
    pub installation_id: String,
    pub provider: String,
    /// `org`, or `org:user` for a personal connection.
    pub group_ref: String,
    /// The key that registered it.
    pub registered_by: String,
    /// The person that key's service said it acted for, if it named one.
    pub registered_for: Option<String>,
    pub registered_at: i64,
    pub revoked_at: Option<i64>,
}

/// Every installation's current state, from its events in the order they were recorded. A revoked
/// installation stays revoked: a later registration of the same id is never written (see `plan`).
pub fn fold(events: &[Event]) -> BTreeMap<String, Installation> {
    let mut out: BTreeMap<String, Installation> = BTreeMap::new();
    for e in events {
        match e.op.as_str() {
            "register" => {
                if let (Some(provider), Some(group_ref)) = (&e.provider, &e.group_ref) {
                    out.entry(e.installation_id.clone())
                        .or_insert_with(|| Installation {
                            installation_id: e.installation_id.clone(),
                            provider: provider.clone(),
                            group_ref: group_ref.clone(),
                            registered_by: e.actor.clone(),
                            registered_for: e.on_behalf_of.clone(),
                            registered_at: e.at,
                            revoked_at: None,
                        });
                }
            }
            "revoke" => {
                if let Some(i) = out.get_mut(&e.installation_id) {
                    i.revoked_at.get_or_insert(e.at);
                }
            }
            // Ops a newer engine wrote are skipped, not misread.
            _ => {}
        }
    }
    out
}

/// A change the caller can fix, or a conflict with what is recorded.
#[derive(Debug)]
pub enum Refused {
    Bad(String),
    Conflict(String),
    NotFound(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Bad(m) | Refused::Conflict(m) | Refused::NotFound(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Refused {}

/// `org` or `org:user`, each part a plain ACL id: what connectors turn into `group:<org>` or `user:<user>`.
pub fn parse_group_ref(group_ref: &str) -> Option<(&str, Option<&str>)> {
    let (org, user) = match group_ref.split_once(':') {
        None => (group_ref, None),
        Some((o, u)) => (o, Some(u)),
    };
    let ok = |s: &str| !s.is_empty() && !s.contains(':') && is_acl_id(s);
    (ok(org) && user.is_none_or(ok)).then_some((org, user))
}

/// The events registering an installation, or none when it is already registered exactly so.
/// `known_orgs`: the directory's orgs, when it has any; the owner's org must be one of them.
pub fn plan_register(
    current: &BTreeMap<String, Installation>,
    known_orgs: Option<&[String]>,
    actor: &str,
    installation_id: &str,
    provider: &str,
    group_ref: &str,
) -> Result<Vec<Event>, Refused> {
    if installation_id.is_empty() || installation_id.len() > 200 {
        return Err(Refused::Bad(
            "installation_id is 1 to 200 characters".into(),
        ));
    }
    if provider.is_empty() || provider.len() > 100 {
        return Err(Refused::Bad("provider is 1 to 100 characters".into()));
    }
    let Some((org, _)) = parse_group_ref(group_ref) else {
        return Err(Refused::Bad(
            "group_ref is <org> or <org>:<user>, each a plain id".into(),
        ));
    };
    if let Some(orgs) = known_orgs
        && !orgs.iter().any(|o| o == org)
    {
        return Err(Refused::Bad(format!("org {org} is not in the directory")));
    }
    if let Some(i) = current.get(installation_id) {
        if i.revoked_at.is_some() {
            return Err(Refused::Conflict(format!(
                "installation {installation_id} was revoked; a revoked installation is never re-registered"
            )));
        }
        if i.group_ref != group_ref || i.provider != provider {
            // An installation's owner never changes: re-pointing it would hand its data to someone else.
            return Err(Refused::Conflict(format!(
                "installation {installation_id} is registered to {} ({})",
                i.group_ref, i.provider
            )));
        }
        return Ok(vec![]);
    }
    Ok(vec![Event {
        at: 0,
        actor: actor.into(),
        op: "register".into(),
        installation_id: installation_id.into(),
        provider: Some(provider.into()),
        group_ref: Some(group_ref.into()),
        on_behalf_of: None,
    }])
}

/// The events revoking an installation, or none when it already is.
pub fn plan_revoke(
    current: &BTreeMap<String, Installation>,
    actor: &str,
    installation_id: &str,
) -> Result<Vec<Event>, Refused> {
    match current.get(installation_id) {
        None => Err(Refused::NotFound(format!(
            "installation {installation_id} is not registered"
        ))),
        Some(i) if i.revoked_at.is_some() => Ok(vec![]),
        Some(_) => Ok(vec![Event {
            at: 0,
            actor: actor.into(),
            op: "revoke".into(),
            installation_id: installation_id.into(),
            provider: None,
            group_ref: None,
            on_behalf_of: None,
        }]),
    }
}
