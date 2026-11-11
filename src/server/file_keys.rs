//! Non-admin keys in the keys file (`AMPUP_MEMORY_API_KEYS`). A keys file is for admin keys and for a server that
//! serves its own scope: there a non-admin file key reads that scope, never an org's, which takes one of the
//! org's keys (`ampup-memory apikey`). A server that reads every request through the directory
//! (`AMPUP_MEMORY_DIRECTORY=required`) has no scope of its own to serve, so it takes no non-admin file key. Before
//! ignoring one, the server holding the writer lease moves it into the directory if a directory key reads
//! exactly what it did (its user in one org and in none of its teams, no groups): the same key, now revocable in
//! the directory. Each server says what becomes of every one (a log line when that changes, `ALERT` for one it
//! ignores; the `memstore_file_keys` gauge; `ampup-memory identity check`), by id and user, never the key.
//!
//! A key whose hash was ever a directory key (revoked since, say) is never imported: the directory said
//! what becomes of it. The import is the operator's `apikey import` of the one row, planned inside the
//! directory's commit like any other change.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use crate::auth::KeyCaller;
use crate::people::directory::{
    Actor, Change, Directory, FileKey, KeyRole, SignIn, apply, key_name_of, user_id,
};

/// What becomes of one non-admin file key.
#[derive(Clone, Debug, PartialEq)]
pub enum Fate {
    /// The directory's key `org/name` has its hash: a bearer of it is that key, on every server.
    Imported { org: String, name: String },
    /// It reads this server's own scope: the server doesn't require the directory.
    Scope,
    /// The server requires the directory, and a writer key `org/name` reads just what the key would, so the
    /// server holding the writer lease imports it.
    Importable { org: String, name: String },
    /// The server requires the directory, and the key isn't imported: it is ignored. Why, and what to do.
    Ignored(String),
}

impl Fate {
    /// The `state` label of `memstore_file_keys`.
    fn state(&self) -> &'static str {
        match self {
            Fate::Imported { .. } => "imported",
            Fate::Scope => "scope",
            Fate::Importable { .. } | Fate::Ignored(_) => "ignored",
        }
    }

    /// What a server says of the key `k` (never the key, nor its hash).
    pub fn line(&self, k: &KeyCaller) -> String {
        let who = format!("file key {} ({})", k.id, k.user);
        match self {
            Fate::Imported { org, name } => format!(
                "{who} is the directory's key {org}/{name}: revoke it there (ampup-memory apikey revoke {name} --org {org}), and take it out of the keys file"
            ),
            Fate::Scope => format!("{who} reads this server's own scope, and no org's"),
            Fate::Importable { org, name } => format!(
                "ALERT {who} is ignored here (this server reads every request through the directory): the server holding the writer lease imports it as {org}/{name}, the same key"
            ),
            Fate::Ignored(why) => format!(
                "ALERT {who} is ignored here (this server reads every request through the directory, and takes no keys-file key but an admin's): {why}"
            ),
        }
    }
}

/// What becomes of the non-admin file key `k` against the directory `dir`, decided as a request with it is
/// (`request_principal`): the directory's key if one has its hash; else, on a server that doesn't require the
/// directory (`through_directory` false), this server's scope; else imported if a directory key reads just
/// what it would, and ignored if not. `more_groups`: the groups this server gives the key's user on top of the
/// key's own (the server's groups, closures), which a directory key holds none of. `was_directory_key`: its
/// hash was a directory key once (revoked since): it is not imported again.
pub fn fate(
    dir: &Directory,
    k: &KeyCaller,
    through_directory: bool,
    more_groups: &[String],
    was_directory_key: bool,
) -> Fate {
    let sha = k.sha256.to_ascii_lowercase();
    let twin = dir.orgs.iter().find_map(|(o, org)| {
        org.keys
            .iter()
            .find(|(_, d)| d.sha256 == sha)
            .map(|(n, _)| (o.clone(), n.clone()))
    });
    if let Some((org, name)) = twin {
        return Fate::Imported { org, name };
    }
    if !through_directory {
        return Fate::Scope;
    }
    if was_directory_key {
        return Fate::Ignored(
            "it was a directory key, revoked since, so it isn't imported again: take it out of the keys file".into(),
        );
    }
    if !k.groups.is_empty() {
        return Fate::Ignored(format!(
            "it holds groups ({}), and a directory key holds none: give the service a key of the team those groups stand for (ampup-memory apikey create --team <team>)",
            k.groups.join(", ")
        ));
    }
    if !more_groups.is_empty() {
        return Fate::Ignored(format!(
            "this server gives its user groups ({}: the server's groups, or pushed closures), and a directory key holds none: give the service a key of its own (ampup-memory apikey create)",
            more_groups.join(", ")
        ));
    }
    let user = user_id(&k.user);
    let held = dir.access(&user, &SignIn::default());
    let org = match held.iter().collect::<Vec<_>>().as_slice() {
        [] => {
            return Fate::Ignored(format!(
                "its user {user} is a member of no org; add them to the org the key serves and it is imported"
            ));
        }
        [(org, a)] if a.teams.is_empty() => org.to_string(),
        [(org, a)] => {
            let teams: Vec<&str> = a.teams.iter().map(String::as_str).collect();
            return Fate::Ignored(format!(
                "its user {user} is also in teams of {org} ({}), which a key of {org} doesn't read: import it for the scope it serves (ampup-memory apikey import --org {org} [--team <team>] --from <keys file> --only {})",
                teams.join(", "),
                k.id
            ));
        }
        many => {
            let orgs: Vec<&str> = many.iter().map(|(o, _)| o.as_str()).collect();
            return Fate::Ignored(format!(
                "its user {user} is a member of several orgs ({}), and a directory key reads one: import it into the one it serves (ampup-memory apikey import --org <org> --from <keys file> --only {}) and give each other one a key of its own",
                orgs.join(", "),
                k.id
            ));
        }
    };
    let Some(name) = key_name_of(&k.id) else {
        return Fate::Ignored(format!(
            "its id {:?} makes no key name (lower-case letters, digits, single underscores): give it another id in the keys file",
            k.id
        ));
    };
    if dir
        .orgs
        .get(&org)
        .is_some_and(|o| o.keys.contains_key(&name))
    {
        return Fate::Ignored(format!(
            "org {org} has a key named {name} already: revoke that one, or give this row another id in the keys file"
        ));
    }
    Fate::Importable { org, name }
}

/// The import that makes `k` the directory's key of `org`: the operator's `apikey import` of the one row, as a
/// writer key of the org (what a file key read and wrote), from this server's own keys file.
pub fn import(k: &KeyCaller, org: &str, file_keys: BTreeSet<String>) -> (Actor, Change) {
    let actor = Actor {
        user: "server:keys-file".into(),
        superuser: true,
        via: SignIn::default(),
        owner: false,
        file_keys,
    };
    let change = Change::ImportKeys {
        org: org.to_string(),
        team: None,
        role: KeyRole::Writer,
        keys: vec![FileKey {
            id: k.id.clone(),
            sha256: k.sha256.to_ascii_lowercase(),
            user: k.user.clone(),
            groups: k.groups.clone(),
            admin: false,
        }],
    };
    (actor, change)
}

/// A server's report of its non-admin file keys: the gauge, and what it last said of each (so it says it again
/// only when that changes).
pub struct Report {
    /// `memstore_file_keys{id, user, state}`: 1 for each non-admin file key, `state` `scope` (reads this
    /// server's own scope), `imported` (a directory key now) or `ignored`.
    pub gauge: prometheus::IntGaugeVec,
    said: Mutex<BTreeMap<String, String>>,
}

impl Default for Report {
    fn default() -> Self {
        Report {
            gauge: prometheus::IntGaugeVec::new(
                prometheus::Opts::new(
                    "memstore_file_keys",
                    "Non-admin keys in the keys file, by id and user (never the key): state scope (reads this server's own scope), imported (a directory key now) or ignored (the server reads every request through the directory)",
                ),
                &["id", "user", "state"],
            )
            .unwrap(),
            said: Default::default(),
        }
    }
}

impl Report {
    /// Records each key's fate: the gauge as it is now, and a line for each key whose fate changed (all of
    /// them, the first time), plus a summary when any did.
    pub fn record(&self, fates: &[(KeyCaller, Fate)]) {
        self.gauge.reset();
        let mut said = self.said.lock().unwrap();
        let mut now = BTreeMap::new();
        let mut changed = false;
        for (k, f) in fates {
            self.gauge
                .with_label_values(&[k.id.as_str(), k.user.as_str(), f.state()])
                .set(1);
            let line = f.line(k);
            if said.get(&k.id) != Some(&line) {
                eprintln!("{line}");
                changed = true;
            }
            now.insert(k.id.clone(), line);
        }
        changed |= said.keys().any(|id| !now.contains_key(id));
        if changed {
            let count = |s: &str| fates.iter().filter(|(_, f)| f.state() == s).count();
            eprintln!(
                "keys file: {} non-admin keys: {} read this server's own scope, {} are directory keys, {} ignored",
                fates.len(),
                count("scope"),
                count("imported"),
                count("ignored")
            );
        }
        *said = now;
    }
}

/// Every non-admin key in `keys`, with its fate against `dir`. `was_directory_key`: whether a hash was ever a
/// directory key.
pub fn fates(
    dir: &Directory,
    keys: Vec<KeyCaller>,
    through_directory: bool,
    more_groups: impl Fn(&str) -> Vec<String>,
    was_directory_key: impl Fn(&str) -> bool,
) -> Vec<(KeyCaller, Fate)> {
    keys.into_iter()
        .filter(|k| !k.admin)
        .map(|k| {
            let f = fate(
                dir,
                &k,
                through_directory,
                &more_groups(&k.user),
                was_directory_key(&k.sha256),
            );
            (k, f)
        })
        .collect()
}

/// The sha256 of every key the directory's log ever made (`key.create`), revoked since or not.
pub async fn ever_directory_keys(store: &crate::Store) -> anyhow::Result<BTreeSet<String>> {
    Ok(store
        .directory_events()
        .await?
        .iter()
        .filter(|e| e.op == "key.create")
        .filter_map(|e| {
            serde_json::from_str::<crate::people::directory::KeyDetail>(e.detail.as_deref()?).ok()
        })
        .map(|k| k.sha256.to_ascii_lowercase())
        .collect())
}

/// What a server keeps between passes: hashes found to have been directory keys once (that never changes back),
/// and the last import failure said of each key.
#[derive(Default)]
pub(super) struct Passes {
    were_keys: BTreeSet<String>,
    failed: BTreeMap<String, String>,
    /// When the log was last read for them, on a server that doesn't import (at most once a minute).
    read_at: Option<std::time::Instant>,
}

/// One pass on a server: report its non-admin file keys, and if it requires the directory and holds the writer
/// lease, import each one a directory key stands for exactly. Before importing, the directory's whole log is read
/// once for the hashes it ever held (a key revoked there is never imported again). A failed import is said once
/// (as `ALERT`) and tried again next pass.
pub(super) async fn pass(state: &super::State, memo: &mut Passes) {
    let Some(auth) = state.token_auth() else {
        return;
    };
    let keys: Vec<KeyCaller> = auth.file_key_callers();
    if keys.iter().all(|k| k.admin) {
        state.file_keys.record(&[]);
        return;
    }
    let store = state.store.read().unwrap().clone();
    let dir = match Directory::load(&store).await {
        Ok(d) => d,
        // Not loaded yet (a server warming up), or the catalog is failing: the next pass.
        Err(_) => return,
    };
    let more = |user: &str| {
        super::token_principal(state, user.to_string(), vec![])
            .groups()
            .to_vec()
    };
    let through = state.directory_only;
    let mut fates = fates(&dir, keys.clone(), through, more, |sha| {
        memo.were_keys.contains(sha)
    });
    let writer = state.writer.holds();
    let importable = fates
        .iter()
        .any(|(_, f)| matches!(f, Fate::Importable { .. }));
    // Read the log for these keys before importing one (and, on another server, once a minute, so what it
    // says matches): a hash it ever held stays the directory's to decide.
    let due = writer
        || memo
            .read_at
            .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(60));
    if importable && due {
        memo.read_at = Some(std::time::Instant::now());
        match ever_directory_keys(&store).await {
            Ok(ever) => {
                let mine: BTreeSet<String> = keys.iter().map(|k| k.sha256.clone()).collect();
                memo.were_keys.extend(ever.intersection(&mine).cloned());
            }
            Err(e) => {
                eprintln!(
                    "ALERT file keys: the directory's log can't be read, so none is imported: {e:#}"
                );
                state.file_keys.record(&fates);
                return;
            }
        }
    }
    for (k, f) in &mut fates {
        if matches!(f, Fate::Importable { .. }) && memo.were_keys.contains(&k.sha256) {
            *f = fate(&dir, k, through, &[], true);
        }
    }
    if writer {
        let held = auth.file_key_hashes();
        for (k, f) in &mut fates {
            let Fate::Importable { org, name } = f.clone() else {
                continue;
            };
            let (actor, change) = import(k, &org, held.clone());
            match apply(&store, &actor, &change).await {
                Ok(_) => {
                    eprintln!(
                        "file key {} ({}) imported into the directory as {org}/{name}: the same key, revocable there with no restart",
                        k.id, k.user
                    );
                    memo.were_keys.insert(k.sha256.clone());
                    *f = Fate::Imported { org, name };
                    memo.failed.remove(&k.id);
                    *state.providers.lock().unwrap() = None;
                }
                Err(e) => {
                    let said = format!("{e:#}");
                    if memo.failed.get(&k.id) != Some(&said) {
                        eprintln!(
                            "ALERT file key {} ({}) not imported into {org}: {said}",
                            k.id, k.user
                        );
                        memo.failed.insert(k.id.clone(), said);
                    }
                }
            }
        }
    }
    state.file_keys.record(&fates);
}
