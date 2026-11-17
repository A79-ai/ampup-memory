//! What an entity is called where it is shown, rather than its id: `meeting:7d3f…` as the call's title.
//!
//! Only an id whose key is opaque (a uuid, a record number: [`opaque`]) is named; one whose key is a slug
//! (`account:globex`) already reads as its name. An entity's name is its open `_name` fact. One the engine has not named (a meeting recorded before its episode
//! type wrote that fact) is called by the title of a record it is the subject of: a call's recorder title, read from
//! the newest episode about it whose type says the record is of it ([`crate::arrivals::titled`]). Both are read
//! with the reader's own access: a fact or an episode they may not read names nothing.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;

use crate::browse::Ep;
use crate::query::schema::NAME;
use crate::search::Col;
use crate::{Principal, Session, Store};

/// Ids named at most per call: a page's worth.
const MAX_IDS: usize = 1000;

/// The name of each of `ids` that has one, by id. `eps` are the episodes the reader may read in the session
/// ([`crate::browse::episodes`]); empty, only `_name` facts name anything.
pub(crate) async fn of(
    store: &Store,
    session: &Session,
    who: &Principal,
    ids: &[String],
    eps: &[Ep],
) -> Result<BTreeMap<String, String>> {
    let ids: Vec<String> = ids
        .iter()
        .filter(|i| opaque(i))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .take(MAX_IDS)
        .collect();
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let mut newest: HashMap<String, i64> = HashMap::new();
    for f in session.read_in(store, who, Col::Entity, &ids, None).await? {
        let name = f.value.trim();
        if f.predicate != NAME || f.valid_to.is_some() || name.is_empty() {
            continue;
        }
        if newest.get(&f.entity).is_none_or(|at| f.valid_from > *at) {
            newest.insert(f.entity.clone(), f.valid_from);
            out.insert(f.entity.clone(), name.to_string());
        }
    }
    let missing: HashSet<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|i| !out.contains_key(*i))
        .collect();
    if missing.is_empty() {
        return Ok(out);
    }
    // The newest episode about each unnamed id.
    let mut about: HashMap<&str, &Ep> = HashMap::new();
    for e in eps {
        for a in e.about.iter().filter(|a| missing.contains(a.as_str())) {
            let held = about.entry(a.as_str()).or_insert(e);
            if e.occurred_at > held.occurred_at {
                *held = e;
            }
        }
    }
    let ask: Vec<(String, String)> = about
        .values()
        .map(|e| (e.id.clone(), e.source.clone()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let titled = crate::arrivals::titled(store, session, who, &ask).await?;
    for (id, e) in about {
        let Some(t) = titled.get(&e.id) else { continue };
        if let Some(name) = t
            .named
            .as_ref()
            .filter(|_| t.subject.as_deref() == Some(id))
        {
            out.insert(id.to_string(), name.clone());
        }
    }
    Ok(out)
}

/// Whether `id` is an entity id whose key says nothing to a person: digits and hex (a uuid, a CRM record's number).
pub(crate) fn opaque(id: &str) -> bool {
    let Some((_, key)) = id.split_once(':') else {
        return false;
    };
    crate::entity_page::entity_like(id)
        && key.bytes().filter(u8::is_ascii_digit).count() >= 4
        && key
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'-' || b == b'_')
}

/// Every opaque entity id a response holds, to name: under `entity`, `value_ref`, `value`, `id` and `about`,
/// wherever they are.
pub(crate) fn ids_in(v: &serde_json::Value, out: &mut Vec<String>) {
    use serde_json::Value;
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match (k.as_str(), x) {
                    ("entity" | "value_ref" | "value" | "id", Value::String(s)) if opaque(s) => {
                        out.push(s.clone())
                    }
                    ("about", Value::Array(a)) => out.extend(
                        a.iter()
                            .filter_map(Value::as_str)
                            .filter(|s| opaque(s))
                            .map(String::from),
                    ),
                    _ => ids_in(x, out),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| ids_in(x, out)),
        _ => {}
    }
}

/// `v` with `names`: the name of every opaque entity id it holds that has one ([`of`]). An object only.
pub(crate) async fn attach(
    store: &Store,
    session: &Session,
    who: &Principal,
    mut v: serde_json::Value,
    eps: &[Ep],
) -> Result<serde_json::Value> {
    let mut ids = vec![];
    ids_in(&v, &mut ids);
    let names = of(store, session, who, &ids, eps).await?;
    if let (false, Some(m)) = (names.is_empty(), v.as_object_mut()) {
        m.insert("names".into(), serde_json::json!(names));
    }
    Ok(v)
}
