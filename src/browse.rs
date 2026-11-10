//! `/browse`: a scope's memory as a file tree, for a UI to walk. Up to three trees hang off the root:
//!
//! - `sources/…`: episodes by their source split at `:` (`slack:C09:1712.3` is `sources/slack/C09/1712.3`).
//! - `entities/<type>/<key>`: facts by the entity they are about (`deal:42` is `entities/deal/42`).
//! - `skills/<slug>`: the skills compiled from the scope's documents (`crate::compile`), there only when the
//!   caller may read one on the branch read. The folder lists the routing index and each skill with when it
//!   applies; a skill reads as its markdown with its AIP step graph.
//!
//! A path is a JSON array of segments, so a segment may hold `/`. Every listing is built from what the
//! caller may read and nothing else: an episode they can't see is in no count and names no folder, and a
//! fact shows only when they can see every episode it cites (the same rules as every other read).
//!
//! Requests: `{"op": "ls", "path": [...], "limit"?, "after"?}` lists a folder (children sorted by name,
//! `after` the last name of the previous page) or, at `entities/<type>/<key>`, the entity's facts;
//! `{"op": "episode", "id": "..."}` reads one episode with the facts that cite it; `{"op": "branches"}` names
//! the scope's branches, each with how many compiled skills the caller may read there; `{"op": "schema"}` reads
//! the scope's schema on the branch read, with every version committed there.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Value, json};

use crate::search::{Col, Filter};
use crate::{Fact, Principal, Scope, Session, Store};

/// Children per page unless the request asks for fewer or more (at most [`MAX_LIMIT`]).
const LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;
/// An episode's text is returned up to this many bytes.
const BODY_BYTES: usize = 256 << 10;

/// A request the caller got wrong: shown to them.
#[derive(Debug)]
pub struct BadRequest(pub String);

impl std::fmt::Display for BadRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for BadRequest {}

/// Nothing at that path the caller may see (whether or not it exists for someone else).
#[derive(Debug)]
pub struct NotFound;

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nothing there (or not visible)")
    }
}
impl std::error::Error for NotFound {}

pub(crate) fn bad(msg: impl Into<String>) -> anyhow::Error {
    BadRequest(msg.into()).into()
}

/// One visible episode, without its text.
pub(crate) struct Ep {
    pub(crate) id: String,
    pub(crate) source: String,
    occurred_at: i64,
    about: Vec<String>,
}

impl Ep {
    fn segments(&self) -> Vec<&str> {
        self.source.split(':').collect()
    }
}

/// Answers one `/browse` request as `who`.
pub async fn run(
    store: &Store,
    session: &Session,
    who: &Principal,
    req: &Value,
    admin: bool,
) -> Result<Value> {
    match req["op"].as_str().unwrap_or("ls") {
        // The filterable lists (`crate::listing`): `/facts/list` and `/advice/list` are these ops.
        "facts" => crate::listing::facts(store, session, who, req).await,
        "advice" => crate::listing::advice(store, session, who, req, admin).await,
        "ls" => {
            let path: Vec<String> = match &req["path"] {
                Value::Null => vec![],
                Value::Array(a) => a
                    .iter()
                    .map(|s| s.as_str().map(String::from))
                    .collect::<Option<_>>()
                    .ok_or_else(|| bad("path is an array of strings"))?,
                _ => return Err(bad("path is an array of strings")),
            };
            let limit = req["limit"]
                .as_u64()
                .map_or(LIMIT, |n| n as usize)
                .clamp(1, MAX_LIMIT);
            let after = req["after"].as_str();
            ls(store, session, who, &path, limit, after).await
        }
        "episode" => {
            let id = req["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad("episode needs an id"))?;
            episode(store, session, who, id).await
        }
        "branches" => branches(store, session.scope(), who).await,
        // The schema the memory is typed with on the branch read, and every version committed there (oldest
        // first): it holds no memory, only the types and what its `extract` section names.
        "schema" => {
            let history: Vec<&crate::query::schema::Schema> =
                session.vis().schemas.iter().map(|s| &**s).collect();
            Ok(json!({"schema": session.schema().as_deref(), "history": history}))
        }
        op => Err(bad(format!(
            "unknown op {op:?}: ls, episode, branches, schema, facts or advice"
        ))),
    }
}

/// The scope's branches, main first, each with the compiled skills the caller may read on it: a memory build
/// often compiles on a branch of its own, so a reader on main learns where to look.
async fn branches(store: &Store, scope: &Scope, who: &Principal) -> Result<Value> {
    let mut names: Vec<String> = store.branch_heads(scope).await?.into_keys().collect();
    names.sort_by_key(|b| (b.as_str() != crate::MAIN, b.clone()));
    if names.is_empty() {
        names.push(crate::MAIN.to_string());
    }
    let mut out = vec![];
    for b in names {
        let session = store.pin(scope, &b).await?;
        let skills = skill_keys(&session, who).len();
        out.push(json!({"name": b, "skills": skills}));
    }
    Ok(json!({"branches": out}))
}

async fn ls(
    store: &Store,
    session: &Session,
    who: &Principal,
    path: &[String],
    limit: usize,
    after: Option<&str>,
) -> Result<Value> {
    let page = |mut children: Vec<Value>| {
        children.retain(|c| after.is_none_or(|a| c["name"].as_str().unwrap_or_default() > a));
        let more = children.len() > limit;
        children.truncate(limit);
        let next = more.then(|| children.last().map(|c| c["name"].clone()));
        json!({"path": path, "children": children, "next": next.flatten()})
    };
    match path.first().map(String::as_str) {
        None => {
            let (eps, facts) =
                tokio::try_join!(episodes(store, session, who), facts(store, session, who))?;
            let entities = by_entity(&facts).len();
            let skills = skill_keys(session, who).len();
            let mut children = vec![
                json!({"name": "entities", "kind": "folder", "path": ["entities"], "count": entities}),
                json!({"name": "skills", "kind": "folder", "path": ["skills"], "count": skills}),
                json!({"name": "sources", "kind": "folder", "path": ["sources"], "count": eps.len()}),
            ];
            children.retain(|c| c["name"] != "skills" || skills > 0);
            Ok(page(children))
        }
        Some("skills") => skills(store, session, who, path, page).await,
        Some("sources") => {
            let at: Vec<&str> = path[1..].iter().map(String::as_str).collect();
            let eps = episodes(store, session, who).await?;
            let under: Vec<&Ep> = eps
                .iter()
                .filter(|e| e.segments().starts_with(&at))
                .collect();
            if under.is_empty() && !at.is_empty() {
                return Err(NotFound.into());
            }
            // Each next segment, with the episodes under it; episodes whose source ends here are listed
            // by id.
            let mut folders: BTreeMap<&str, Vec<&Ep>> = BTreeMap::new();
            let mut here = vec![];
            for e in under {
                match e.segments().get(at.len()) {
                    Some(s) => folders.entry(s).or_default().push(e),
                    None => here.push(e),
                }
            }
            let mut children: Vec<Value> = folders
                .into_iter()
                .map(|(name, eps)| {
                    let mut p = path.to_vec();
                    p.push(name.to_string());
                    // A folder holding just one episode whose source ends at it is that episode.
                    match eps.as_slice() {
                        [e] if e.segments().len() == at.len() + 1 => {
                            json!({"name": name, "kind": "episode", "path": p, "episode_id": e.id,
                                   "occurred_at": e.occurred_at})
                        }
                        _ => json!({"name": name, "kind": "folder", "path": p, "count": eps.len(),
                                    "latest": eps.iter().map(|e| e.occurred_at).max()}),
                    }
                })
                .chain(here.into_iter().map(|e| {
                    json!({"name": e.id, "kind": "episode", "path": path, "episode_id": e.id,
                           "occurred_at": e.occurred_at})
                }))
                .collect();
            children.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            Ok(page(children))
        }
        Some("entities") if path.len() == 3 => {
            // One entity's facts: read by the entity index, not out of every fact.
            let (t, k) = (path[1].as_str(), path[2].as_str());
            let mut ids = vec![format!("{t}:{k}")];
            if t == "_" && !k.contains(':') {
                ids.push(k.to_string());
            }
            let facts = once_each(session.read_in(store, who, Col::Entity, &ids, None).await?);
            let mut fs: Vec<&Fact> = facts
                .iter()
                .filter(|f| split_entity(&f.entity) == (t, k))
                .collect();
            if fs.is_empty() {
                return Err(NotFound.into());
            }
            // The fact id last, so facts stated at the same moment keep one order however they were read.
            fs.sort_by(|a, b| {
                (&a.predicate, a.valid_from, &a.fact_id).cmp(&(
                    &b.predicate,
                    b.valid_from,
                    &b.fact_id,
                ))
            });
            Ok(json!({"path": path, "entity": fs[0].entity,
                      "facts": fs.iter().map(|f| fact_json(f)).collect::<Vec<_>>()}))
        }
        Some("entities") => {
            let facts = facts(store, session, who).await?;
            let entities = by_entity(&facts);
            match &path[1..] {
                [] => {
                    let mut types: BTreeMap<&str, usize> = BTreeMap::new();
                    for (t, _) in entities.keys() {
                        *types.entry(t).or_default() += 1;
                    }
                    Ok(page(
                        types
                            .into_iter()
                            .map(|(t, n)| {
                                json!({"name": t, "kind": "folder", "path": ["entities", t], "count": n})
                            })
                            .collect(),
                    ))
                }
                [t] => {
                    let children: Vec<Value> = entities
                        .iter()
                        .filter(|((et, _), _)| et == t)
                        .map(|((_, k), fs)| {
                            json!({"name": k, "kind": "entity", "path": ["entities", t, k],
                                   "entity": fs[0].entity, "count": fs.len()})
                        })
                        .collect();
                    if children.is_empty() {
                        return Err(NotFound.into());
                    }
                    Ok(page(children))
                }
                _ => Err(NotFound.into()),
            }
        }
        Some(_) => Err(NotFound.into()),
    }
}

async fn episode(store: &Store, session: &Session, who: &Principal, id: &str) -> Result<Value> {
    // The facts citing it: those whose sources name it (a scan of that one column), then exactly.
    let cites_it = Filter::Contains(Col::Sources, id.to_string());
    let citing = session.read_where(store, who, &cites_it, None);
    let (eps, facts) = tokio::try_join!(episodes(store, session, who), citing)?;
    let facts = once_each(facts);
    let Some(e) = eps.iter().find(|e| e.id == id) else {
        return Err(NotFound.into());
    };
    // Read with the caller's own access, again: an ACL change since the listing hides it.
    let Some(body) = session.episode_body(store, who, id).await? else {
        return Err(NotFound.into());
    };
    let truncated = body.len() > BODY_BYTES;
    let text = String::from_utf8_lossy(&body[..body.len().min(BODY_BYTES)]).into_owned();
    let mut path = vec!["sources".to_string()];
    path.extend(e.segments().into_iter().map(String::from));
    let cites: Vec<Value> = facts
        .iter()
        .filter(|f| f.source_episodes.iter().any(|s| s == id))
        .map(fact_json)
        .collect();
    Ok(
        json!({"episode_id": e.id, "source": e.source, "path": path, "occurred_at": e.occurred_at,
              "about": e.about, "body": text, "truncated": truncated, "facts": cites}),
    )
}

/// The compiled skills the caller may read: (slug, document key). The routing indexes (one per audience,
/// `skills/index` or `skills/index~<hash>`) are not skills.
fn skill_keys(session: &Session, who: &Principal) -> Vec<(String, String)> {
    session
        .documents(who, crate::compile::SKILLS, None)
        .into_iter()
        .filter(|k| !crate::compile::is_index(k))
        .filter_map(|k| {
            let slug = k.strip_prefix(crate::compile::SKILLS)?.to_string();
            (!slug.is_empty() && !slug.contains('/')).then_some((slug, k))
        })
        .collect()
}

/// The routing indexes the caller may read, by name (`index`, `index~<hash>`), as document keys.
fn index_keys(session: &Session, who: &Principal) -> Vec<(String, String)> {
    session
        .documents(who, crate::compile::SKILLS, None)
        .into_iter()
        .filter(|k| crate::compile::is_index(k))
        .filter_map(|k| Some((k.strip_prefix(crate::compile::SKILLS)?.to_string(), k)))
        .collect()
}

/// `skills/…`: the routing indexes and the compiled skills, or one of them read in full.
async fn skills(
    store: &Store,
    session: &Session,
    who: &Principal,
    path: &[String],
    page: impl Fn(Vec<Value>) -> Value,
) -> Result<Value> {
    match &path[1..] {
        [] => {
            let skills = skill_keys(session, who);
            // When each skill applies, from the index lines (`- `slug`: when (`aip key`)`) of every index
            // the caller may read.
            let mut when: BTreeMap<String, String> = BTreeMap::new();
            let mut children = vec![];
            for (name, key) in index_keys(session, who) {
                let Some((_, at, body)) = session.document(store, who, &key, None).await? else {
                    continue;
                };
                let mut lines = 0;
                for line in String::from_utf8_lossy(&body).lines() {
                    let Some((slug, rest)) =
                        line.strip_prefix("- `").and_then(|l| l.split_once("`: "))
                    else {
                        continue;
                    };
                    lines += 1;
                    let w = rest.rsplit_once(" (`").map_or(rest, |(w, _)| w).trim();
                    when.entry(slug.to_string())
                        .or_insert_with(|| w.to_string());
                }
                children.push(
                    json!({"name": name, "kind": "skill_index", "path": ["skills", name],
                                     "count": lines, "occurred_at": at}),
                );
            }
            children.extend(skills.iter().map(|(slug, _)| {
                json!({"name": slug, "kind": "skill", "path": ["skills", slug],
                       "when": when.get(slug)})
            }));
            children.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            Ok(page(children))
        }
        [name] => {
            let key = crate::compile::skill_key(name);
            let index = crate::compile::is_index(&key);
            let Some((id, at, body)) = session.document(store, who, &key, None).await? else {
                return Err(NotFound.into());
            };
            // A skill's first line is the compiler's own record of it (`<!-- memstore skill {...} -->`).
            let text = String::from_utf8_lossy(&body);
            let markdown = match text.split_once('\n') {
                Some((first, rest)) if first.starts_with("<!-- memstore skill ") => rest,
                _ => text.as_ref(),
            };
            let mut v = json!({"path": path, "name": name,
                               "kind": if index { "skill_index" } else { "skill" },
                               "markdown": markdown, "episode_id": id, "occurred_at": at});
            // The episodes it was compiled from that the caller may read, each with its name; the rest
            // are only counted.
            if let Some((meta, _)) = crate::compile::parse_skill(&body) {
                let vis = session.vis();
                let (seen, hidden): (Vec<_>, Vec<_>) = meta
                    .sources
                    .iter()
                    .enumerate()
                    .partition(|(_, e)| vis.can_see_episode(e, who));
                v["sources"] = json!(
                    seen.iter()
                        .map(|(i, e)| json!({"episode_id": e, "name": meta.names.get(*i)}))
                        .collect::<Vec<_>>()
                );
                if !hidden.is_empty() {
                    v["sources_hidden"] = json!(hidden.len());
                }
            }
            if !index {
                let graph = session
                    .document(store, who, &crate::compile::aip_key(name), None)
                    .await?;
                v["aip"] = graph.map_or(Value::Null, |(_, _, b)| {
                    serde_json::from_slice(&b)
                        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned()))
                });
            }
            Ok(v)
        }
        _ => Err(NotFound.into()),
    }
}

pub(crate) fn fact_json(f: &Fact) -> Value {
    let mut v = json!({"fact_id": f.fact_id, "entity": f.entity, "predicate": f.predicate, "value": f.value,
           "valid_from": f.valid_from, "valid_to": f.valid_to, "source_episodes": f.source_episodes,
           "quote": f.quote});
    for (k, x) in [("unit", &f.unit), ("time_basis", &f.time_basis)] {
        if let Some(x) = x {
            v[k] = json!(x);
        }
    }
    if let Some(c) = crate::cite_json(f) {
        v["cite"] = c;
    }
    v
}

/// An entity id's type and key: `deal:42` is (`deal`, `42`); an id without a type has type `_`.
fn split_entity(e: &str) -> (&str, &str) {
    e.split_once(':').unwrap_or(("_", e))
}

/// Visible facts by entity (type, key).
fn by_entity(facts: &[Fact]) -> BTreeMap<(&str, &str), Vec<&Fact>> {
    let mut out: BTreeMap<(&str, &str), Vec<&Fact>> = BTreeMap::new();
    for f in facts {
        out.entry(split_entity(&f.entity)).or_default().push(f);
    }
    out
}

/// Every fact the caller may read, over all valid time (history included), once each: a corpus loaded
/// twice into one catalog commits each fact twice, under the same id.
async fn facts(store: &Store, session: &Session, who: &Principal) -> Result<Vec<Fact>> {
    Ok(once_each(session.read(store, who, None).await?))
}

/// Each fact once, by id.
pub(crate) fn once_each(mut facts: Vec<Fact>) -> Vec<Fact> {
    let mut seen = std::collections::HashSet::new();
    facts.retain(|f| seen.insert(f.fact_id.clone()));
    facts
}

/// Every committed episode the session sees that the caller may read, once each.
pub(crate) async fn episodes(store: &Store, session: &Session, who: &Principal) -> Result<Vec<Ep>> {
    let vis = session.vis();
    let mut out: BTreeMap<String, Ep> = BTreeMap::new();
    for bs in store.episode_files(session.scope()).await?.values() {
        for b in bs.iter() {
            let (commit, id, source) = (
                crate::strings(b, "commit_id"),
                crate::strings(b, "episode_id"),
                crate::strings(b, "source"),
            );
            let at = crate::timestamps(b, "occurred_at");
            let about = crate::optional_strings(b, "about");
            for i in 0..b.num_rows() {
                if !vis.committed.contains(&commit[i]) || !vis.can_see_episode(&id[i], who) {
                    continue;
                }
                out.entry(id[i].clone()).or_insert_with(|| Ep {
                    id: id[i].clone(),
                    source: source[i].clone(),
                    occurred_at: at[i].unwrap_or_default(),
                    about: crate::split_list(about[i].as_deref().unwrap_or("")),
                });
            }
        }
    }
    Ok(out.into_values().collect())
}

/// Browsing a [`Store`]'s scopes (an inherent impl until the store moved to memstore-core).
pub(crate) trait StoreBrowse {
    async fn scopes(&self) -> Result<Vec<Scope>>;
}

impl StoreBrowse for Store {
    /// Every scope in the catalog, from its namespaces (see [`Scope::namespace`]); org and team names hold no
    /// `__`, so each namespace names one scope.
    async fn scopes(&self) -> Result<Vec<Scope>> {
        let mut out: Vec<Scope> = self
            .catalog()
            .list_namespaces(None)
            .await?
            .into_iter()
            .filter_map(|n| scope_of(&n.to_url_string()))
            .collect();
        out.sort();
        Ok(out)
    }
}

fn scope_of(ns: &str) -> Option<Scope> {
    let s = match ns.strip_prefix("memory_")? {
        "org" => Scope::Org,
        rest => match (
            rest.strip_prefix("org_"),
            rest.strip_prefix("team_"),
            rest.strip_prefix("user_"),
        ) {
            (Some(o), _, _) => match o.split_once("__team_") {
                Some((org, team)) => Scope::team(org, team).ok()?,
                None => Scope::org(o).ok()?,
            },
            (_, Some(t), _) => Scope::Team(t.into()),
            (_, _, Some(u)) => Scope::User(u.into()),
            _ => return None,
        },
    };
    (s.namespace() == ns).then_some(s)
}
