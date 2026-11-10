//! Non-admin keys in the keys file (`AMPUP_MEMORY_API_KEYS`), on their way out. A deployment's keys belong in the
//! directory (`memstore apikey`): two key systems side by side are how one bearer came to name two callers. So
//! each server says, for every non-admin file key, what becomes of it (a log line when that changes, the
//! `memstore_file_keys` gauge, `memstore identity check`), and the server holding the writer lease imports each
//! one that a directory key can stand for exactly: the same key, now revocable in the directory.
//!
//! Only a key the directory can hold with no more and no less than it reads now is imported: a server that
//! reads every request through the directory (`AMPUP_MEMORY_DIRECTORY=required`), a key holding no groups, whose
//! user is a member of one org and of none of its teams. Every other key stays the file's and is named in
//! an `ALERT` line with what to do. The import is the operator's own `apikey import`, verified (this server
//! holds the key), planned inside the directory's commit like any other change: a key revoked in the
//! directory (its hash retired) is never imported again, and one already imported is left as it is.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::auth::KeyCaller;
use crate::people::directory::{
    Actor, Change, Directory, FileKey, KeyRole, SignIn, apply, key_name_of, user_id,
};

/// What becomes of one non-admin file key.
#[derive(Clone, Debug, PartialEq)]
pub enum Fate {
    /// Its verified import, the directory's key `org/name`, answers for it on every server.
    Imported { org: String, name: String },
    /// Refused already, and not imported: why.
    Refused(String),
    /// Read from the keys file still; a writer key `org/name` would read just what it does, so it is imported.
    Importable { org: String, name: String },
    /// Read from the keys file still, and no directory key would read just what it does: why, and what to do.
    Kept(String),
}

impl Fate {
    /// The `state` label of `memstore_file_keys`.
    fn state(&self) -> &'static str {
        match self {
            Fate::Imported { .. } => "imported",
            Fate::Refused(_) => "refused",
            Fate::Importable { .. } | Fate::Kept(_) => "file",
        }
    }

    /// What a server says of the key `k` (never the key, nor its hash).
    pub fn line(&self, k: &KeyCaller) -> String {
        let who = format!("file key {} ({})", k.id, k.user);
        match self {
            Fate::Imported { org, name } => format!(
                "{who} is the directory's key {org}/{name}: revoke it there (ampup-memory apikey revoke {name} --org {org}), and take it out of the keys file"
            ),
            Fate::Refused(why) => {
                format!("ALERT {who} is refused ({why}): take it out of the keys file")
            }
            Fate::Importable { org, name } => format!(
                "ALERT {who} is read from the keys file, and non-admin file keys are being retired: the server holding the writer lease imports it as {org}/{name}, the same key"
            ),
            Fate::Kept(why) => format!(
                "ALERT {who} is read from the keys file, and non-admin file keys are being retired, but it can't be imported as it is: {why}"
            ),
        }
    }
}

/// What becomes of the non-admin file key `k` against the directory `dir`, decided as a request with it is
/// (`request_principal`): its verified import if it has one, else refused if revoked or another directory
/// key's hash, else the file's. `through_directory`: every request is read through the directory
/// (`AMPUP_MEMORY_DIRECTORY=required`); otherwise one that selects no org reads the served scope, which no
/// directory key does. `more_groups`: the groups this server gives the key's user on top of the key's own (the
/// server's groups, closures), which a directory key holds none of.
pub fn fate(
    dir: &Directory,
    k: &KeyCaller,
    through_directory: bool,
    more_groups: &[String],
) -> Fate {
    let sha = k.sha256.to_ascii_lowercase();
    let twins: Vec<(&String, &String, bool)> = dir
        .orgs
        .iter()
        .flat_map(|(o, org)| {
            org.keys
                .iter()
                .filter(|(_, d)| d.sha256 == sha)
                .map(move |(n, d)| (o, n, d.verified_import))
        })
        .collect();
    if let Some((org, name, _)) = twins.iter().find(|t| t.2) {
        return Fate::Imported {
            org: org.to_string(),
            name: name.to_string(),
        };
    }
    if dir.retired_keys.contains(&sha) {
        return Fate::Refused("it was revoked in the directory".into());
    }
    if let Some((org, name, _)) = twins.first() {
        return Fate::Refused(format!(
            "it is also the directory's key {org}/{name}, so it names two callers and is served as neither"
        ));
    }
    if !k.groups.is_empty() {
        return Fate::Kept(format!(
            "it holds groups ({}), and a directory key holds none: give the service a key of the team those groups stand for (ampup-memory apikey create --team <team>)",
            k.groups.join(", ")
        ));
    }
    if !more_groups.is_empty() {
        return Fate::Kept(format!(
            "this server gives its user groups ({}: the server's groups, or pushed closures), and a directory key holds none",
            more_groups.join(", ")
        ));
    }
    if !through_directory {
        return Fate::Kept(
            "a request that selects no org reads this server's own scope (AMPUP_MEMORY_DIRECTORY isn't required), which no directory key reads".into(),
        );
    }
    let user = user_id(&k.user);
    let held = dir.access(&user, &SignIn::default());
    let org = match held.iter().collect::<Vec<_>>().as_slice() {
        [] => {
            return Fate::Kept(format!(
                "its user {user} is a member of no org, so it reads nothing here; add them to the org the key serves and it is imported"
            ));
        }
        [(org, a)] if a.teams.is_empty() => org.to_string(),
        [(org, a)] => {
            let teams: Vec<&str> = a.teams.iter().map(String::as_str).collect();
            return Fate::Kept(format!(
                "its user {user} is also in teams of {org} ({}), which a key of {org} doesn't read: import it for the scope it serves (ampup-memory apikey import --org {org} [--team <team>] --from <keys file> --only {})",
                teams.join(", "),
                k.id
            ));
        }
        many => {
            let orgs: Vec<&str> = many.iter().map(|(o, _)| o.as_str()).collect();
            return Fate::Kept(format!(
                "its user {user} is a member of several orgs ({}), and a directory key reads one: import it into the one it serves (ampup-memory apikey import --org <org> --from <keys file> --only {}) and give each other one a key of its own",
                orgs.join(", "),
                k.id
            ));
        }
    };
    let Some(name) = key_name_of(&k.id) else {
        return Fate::Kept(format!(
            "its id {:?} makes no key name (lower-case letters, digits, single underscores): give it another id in the keys file",
            k.id
        ));
    };
    if dir
        .orgs
        .get(&org)
        .is_some_and(|o| o.keys.contains_key(&name))
    {
        return Fate::Kept(format!(
            "org {org} has a key named {name} already: revoke that one, or give this row another id in the keys file"
        ));
    }
    Fate::Importable { org, name }
}

/// The import that makes `k` the directory's key `org/name`: the operator's `apikey import` of the one row,
/// as a writer key of the org (what a file key may do there), verified because this server holds the key.
pub fn import(
    k: &KeyCaller,
    org: &str,
    file_keys: std::collections::BTreeSet<String>,
) -> (Actor, Change) {
    let sha = k.sha256.to_ascii_lowercase();
    let actor = Actor {
        user: "server:keys-file".into(),
        superuser: true,
        via: SignIn::default(),
        owner: false,
        vouched_keys: [sha.clone()].into(),
        file_keys,
    };
    let change = Change::ImportKeys {
        org: org.to_string(),
        team: None,
        role: KeyRole::Writer,
        keys: vec![FileKey {
            id: k.id.clone(),
            sha256: sha,
            user: k.user.clone(),
            groups: k.groups.clone(),
            admin: false,
        }],
        self_import: false,
    };
    (actor, change)
}

/// A server's report of its non-admin file keys: the gauge, and what it last said of each (so it says it again
/// only when that changes).
pub struct Report {
    /// `memstore_file_keys{id, user, state}`: 1 for each non-admin file key, `state` `file` (still read from the
    /// keys file), `imported` or `refused`.
    pub gauge: prometheus::IntGaugeVec,
    said: Mutex<BTreeMap<String, String>>,
}

impl Default for Report {
    fn default() -> Self {
        Report {
            gauge: prometheus::IntGaugeVec::new(
                prometheus::Opts::new(
                    "memstore_file_keys",
                    "Non-admin keys in the keys file, by id and user (never the key): state file (still read from the file, being retired), imported (the directory's now) or refused",
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
                "keys file: {} non-admin keys: {} still read from the file, {} imported into the directory, {} refused",
                fates.len(),
                count("file"),
                count("imported"),
                count("refused")
            );
        }
        *said = now;
    }
}

/// Every non-admin key `auth` holds, with its fate against `dir`.
pub fn fates(
    dir: &Directory,
    keys: Vec<KeyCaller>,
    through_directory: bool,
    more_groups: impl Fn(&str) -> Vec<String>,
) -> Vec<(KeyCaller, Fate)> {
    keys.into_iter()
        .filter(|k| !k.admin)
        .map(|k| {
            let f = fate(dir, &k, through_directory, &more_groups(&k.user));
            (k, f)
        })
        .collect()
}

/// One pass on a server: report its non-admin file keys, and if it holds the writer lease, import each one
/// that a directory key stands for exactly. A failed import is said once (as `ALERT`) and tried again next pass.
pub(super) async fn pass(state: &super::State, failed: &mut BTreeMap<String, String>) {
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
    let mut fates = fates(&dir, keys, state.directory_only, more);
    if state.writer.holds() {
        let held = auth.file_key_hashes();
        for (k, f) in &mut fates {
            let Fate::Importable { org, name } = f else {
                continue;
            };
            let (actor, change) = import(k, org, held.clone());
            match apply(&store, &actor, &change).await {
                Ok(_) => {
                    eprintln!(
                        "file key {} ({}) imported into the directory as {org}/{name}: the same key, revocable there with no restart",
                        k.id, k.user
                    );
                    *f = Fate::Imported {
                        org: org.clone(),
                        name: name.clone(),
                    };
                    failed.remove(&k.id);
                    *state.providers.lock().unwrap() = None;
                }
                Err(e) => {
                    let said = format!("{e:#}");
                    if failed.get(&k.id) != Some(&said) {
                        eprintln!(
                            "ALERT file key {} ({}) not imported into {org}: {said}",
                            k.id, k.user
                        );
                        failed.insert(k.id.clone(), said);
                    }
                }
            }
        }
    }
    state.file_keys.record(&fates);
}
