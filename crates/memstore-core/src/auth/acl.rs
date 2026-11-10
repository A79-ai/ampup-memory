//! Which episodes a caller may read, fast.
//!
//! The hierarchy (org chart, teams, roles, account teams, Slack channel membership) lives in the
//! customer's systems. It reaches the engine already flattened into a caller's **closure**: the
//! `groups` on their `Principal`. Episodes carry only a short flat ACL (1-3 entries), never an
//! expanded member list, so a membership change rewrites one caller's closure and no episode.
//!
//! [`AclIndex`] answers "may this closure read this episode" with roaring bitmaps: one bitmap of
//! episode ordinals per ACL entry, and a caller's visible set is the OR of the bitmaps of the
//! entries their closure can match. The result is cached per closure, so after the first request
//! a visibility test is one bit test.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use roaring::RoaringBitmap;

/// What an ACL id may be made of. Group names from an IdP that don't fit (a space, a colon, a comma) can never
/// match an ACL entry, and a colon would let a group pose as another kind of entry (`role:`), so they are dropped.
pub fn is_acl_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.@$".contains(&b))
}

/// An id as compared. Emails (anything with an `@`) are case-insensitive, so an IdP's `Amy@X.com` and a
/// writer's `amy@x.com` are one person; every other id (a Snowflake user, a role) stays exact. Only ASCII
/// letters fold: a Unicode lower-casing would make a Kelvin sign (`K`) in someone else's address equal to `k`.
pub fn norm_id(id: &str) -> String {
    if id.contains('@') {
        id.to_ascii_lowercase()
    } else {
        id.to_string()
    }
}

/// A user id as disabling compares it, wherever it is kept (the directory, a disabled-users file) or looked up:
/// trimmed, ASCII letters folded, whatever the id. Wider than [`norm_id`] on purpose: a disable may refuse a
/// second account that differs only in case, never miss the one it names.
pub fn disabled_id(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

/// An ACL entry as compared: the ids of `user:` and `group:` entries (a Google group is an email) go through
/// [`norm_id`].
pub fn norm_entry(entry: &str) -> String {
    match entry.split_once(':') {
        Some((kind @ ("user" | "group"), id)) => format!("{kind}:{}", norm_id(id)),
        _ => entry.to_string(),
    }
}

/// The ACL entries a caller can match: `*`, `user:<id>`, `group:<g>`, `role:<r>`. A closure entry
/// `role:<r>` is a held role and never matches a `group:` entry (mirrors `Principal::can_see`).
pub fn tokens_of(user: &str, groups: &[String]) -> Vec<String> {
    let mut t = Vec::with_capacity(groups.len() + 2);
    t.push("*".to_string());
    t.push(format!("user:{}", norm_id(user)));
    for g in groups {
        match g.strip_prefix("role:") {
            Some(r) => t.push(format!("role:{r}")),
            None => t.push(format!("group:{}", norm_id(g))),
        }
    }
    t.sort_unstable();
    t.dedup();
    t
}

/// Cap on cached per-closure bitmaps (a dense one is ~100 KB at 1M episodes); a full cache is dropped whole (they rebuild in microseconds).
const CACHE_MAX: usize = 512;

static NEXT_INDEX: AtomicU64 = AtomicU64::new(1);

/// Episode ordinals and, per ACL entry, the bitmap of episodes it grants. Built once per ledger
/// state (it lives inside the branch's visibility snapshot).
pub struct AclIndex {
    id: u64,
    ordinal: HashMap<String, u32>,
    /// Episode ids by ordinal.
    ids: Vec<String>,
    by_entry: HashMap<String, RoaringBitmap>,
    cache: Mutex<HashMap<Vec<String>, Arc<RoaringBitmap>>>,
}

impl AclIndex {
    pub fn build(acls: &HashMap<String, Vec<String>>) -> Self {
        let mut ordinal = HashMap::with_capacity(acls.len());
        let mut ids = Vec::with_capacity(acls.len());
        let mut by_entry: HashMap<String, RoaringBitmap> = HashMap::new();
        for (i, (episode, acl)) in acls.iter().enumerate() {
            let o = i as u32;
            ordinal.insert(episode.clone(), o);
            ids.push(episode.clone());
            for entry in acl {
                by_entry.entry(norm_entry(entry)).or_default().insert(o);
            }
        }
        AclIndex {
            id: NEXT_INDEX.fetch_add(1, Ordering::Relaxed),
            ordinal,
            ids,
            by_entry,
            cache: Mutex::default(),
        }
    }

    /// Identifies this snapshot, so a caller can memoize its visible set per snapshot.
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn ordinal(&self, episode: &str) -> Option<u32> {
        self.ordinal.get(episode).copied()
    }

    /// The episodes a closure may read, given as its sorted [`tokens_of`].
    pub fn visible(&self, tokens: &[String]) -> Arc<RoaringBitmap> {
        if let Some(b) = self.cache.lock().unwrap().get(tokens) {
            return b.clone();
        }
        let mut seen = RoaringBitmap::new();
        for t in tokens {
            if let Some(b) = self.by_entry.get(t) {
                seen |= b;
            }
        }
        let seen = Arc::new(seen);
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= CACHE_MAX {
            cache.clear();
        }
        cache.insert(tokens.to_vec(), seen.clone());
        seen
    }

    /// The ids of the episodes in `seen`, when there are at most `max` of them: a narrow reader's whole
    /// visible set, small enough to hand to the search index as a prefilter. `None` for a wide one.
    pub fn ids_if_narrow(&self, seen: &RoaringBitmap, max: usize) -> Option<Vec<String>> {
        (seen.len() <= max as u64)
            .then(|| seen.iter().map(|o| self.ids[o as usize].clone()).collect())
    }

    /// Episodes in the index.
    pub fn len(&self) -> usize {
        self.ordinal.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ordinal.is_empty()
    }
}

/// Membership changes pushed by the customer's systems (IdP/SCIM, account-team, Slack join/leave
/// events), applied on top of whatever groups a caller's token carries. Changes take effect on the
/// caller's next request: no token re-minting, no re-indexing.
#[derive(Default)]
pub struct ClosureStore {
    /// The pilot team csv's closures by email (`crate::people::team`) in force, under every caller's token
    /// groups and the edits: [`Self::team_of`] the two sources below.
    team: Mutex<HashMap<String, Vec<String>>>,
    /// This server's team csv (`AMPUP_MEMORY_TEAM_CSV`), read at start.
    team_file: Mutex<Option<HashMap<String, Vec<String>>>>,
    /// The team csv last pushed to the directory (`POST /team`, `team.csv`), until a clear.
    team_pushed: Mutex<Option<HashMap<String, Vec<String>>>>,
    edits: Mutex<HashMap<String, Edit>>,
    generation: AtomicU64,
    /// Users refused outright (lower-cased: an IdP's casing of an email must not slip past), and the file
    /// they are kept in so a restart doesn't let them back in.
    disabled: Mutex<std::collections::BTreeSet<String>>,
    disabled_file: Option<std::path::PathBuf>,
    /// What the directory says, deployment-wide (`user.disable`/`user.enable`): the last decision about
    /// each user, whichever server made it. It overrides the file: a user the directory let back in
    /// through another server is in, though this server's file still lists them. (A server puts its file's
    /// users the directory has no word about into the directory at start: [`Self::undecided_in_file`].)
    shared_disabled: Mutex<std::collections::BTreeMap<String, bool>>,
    /// The directory fold the edits, team and decisions above were last synced from ([`Self::sync`]): how
    /// many changes it holds, and its digest. A fold with fewer changes is older and never taken.
    synced: Mutex<Option<(u64, String)>>,
}

#[derive(Default, Clone, Debug, PartialEq)]
struct Edit {
    add: Vec<String>,
    remove: Vec<String>,
}

impl ClosureStore {
    /// A store whose disabled users are kept in `path` (a JSON list), read now and rewritten on every
    /// change. A file that exists but can't be read is an error: starting without it would let disabled users in.
    pub fn with_disabled_file(path: std::path::PathBuf) -> Result<Self> {
        let disabled = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Vec<String>>(&text)
                .with_context(|| format!("disabled users file {}", path.display()))?
                .into_iter()
                .map(|u| disabled_id(&u))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        Ok(ClosureStore {
            disabled: Mutex::new(disabled),
            disabled_file: Some(path),
            ..Default::default()
        })
    }

    /// Whether any of a caller's ids is disabled: by the directory's last decision about it, else by this
    /// server's disabled-users file.
    pub fn any_disabled(&self, ids: &[String]) -> bool {
        let set = self.disabled.lock().unwrap();
        let shared = self.shared_disabled.lock().unwrap();
        ids.iter().any(|id| {
            let id = disabled_id(id);
            match shared.get(&id) {
                Some(d) => *d,
                None => set.contains(&id),
            }
        })
    }

    /// The users this server's disabled-users file lists that the directory (`dir`) has no decision about:
    /// a server puts them in the directory (`user.disable`) at start, so every other server refuses them too.
    pub fn undecided_in_file(&self, dir: &crate::people::directory::Directory) -> Vec<String> {
        self.disabled
            .lock()
            .unwrap()
            .iter()
            .filter(|u| !dir.disabled.contains_key(*u))
            .cloned()
            .collect()
    }

    /// Take the deployment-wide state from the directory (`dir`, folded to `events` changes with `digest`): its
    /// disabled-user decisions, every user's closure edit, and its pushed team csv. Every server shares these:
    /// a change made through one is in force on every other once it has read the directory, which it does
    /// before each sign-in decision. Unchanged since the last sync (the same digest), or older than it (fewer
    /// changes: a request that read the directory before another request's newer read), nothing to do. A team
    /// csv that doesn't parse is an error, and nothing is taken from this directory.
    pub fn sync(
        &self,
        events: u64,
        digest: &str,
        dir: &crate::people::directory::Directory,
    ) -> Result<()> {
        let mut synced = self.synced.lock().unwrap();
        if let Some((n, d)) = &*synced
            && (events < *n || (events == *n && d == digest))
        {
            return Ok(());
        }
        let pushed = match &dir.team_csv {
            Some(csv) => Some(
                crate::people::team::Team::parse(csv)
                    .context("the directory's team csv")?
                    .closures()
                    .into_iter()
                    .collect::<HashMap<_, _>>(),
            ),
            None => None,
        };
        *self.shared_disabled.lock().unwrap() = dir.disabled.clone();
        *self.edits.lock().unwrap() = dir
            .closure_edits
            .iter()
            .map(|(u, e)| {
                let edit = Edit {
                    add: e.add.clone(),
                    remove: e.remove.clone(),
                };
                (u.clone(), edit)
            })
            .collect();
        *self.team_pushed.lock().unwrap() = pushed;
        self.recompute_team();
        *synced = Some((events, digest.to_string()));
        self.generation.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Keep `user` in (or out of) this server's disabled-users file, when it has one, as the directory now
    /// says (the server wrote `user.disable`/`user.enable` there first; [`Self::sync`] puts it in force). A
    /// change that couldn't be saved is an error, not a silent loss at the next restart.
    pub fn set_disabled(&self, user: &str, disabled: bool) -> Result<()> {
        let mut set = self.disabled.lock().unwrap();
        let mut next = set.clone();
        if disabled {
            next.insert(disabled_id(user));
        } else {
            next.remove(&disabled_id(user));
        }
        if let Some(path) = &self.disabled_file {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_string(&next)?)
                .and_then(|_| std::fs::rename(&tmp, path))
                .with_context(|| format!("save {}", path.display()))?;
        }
        *set = next;
        self.generation.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Groups to add and remove for `user`; a later call replaces the earlier one.
    pub fn set(&self, user: &str, add: Vec<String>, remove: Vec<String>) {
        let mut e = self.edits.lock().unwrap();
        if add.is_empty() && remove.is_empty() {
            e.remove(&norm_id(user));
        } else {
            e.insert(norm_id(user), Edit { add, remove });
        }
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// This server's own team csv's closures (`AMPUP_MEMORY_TEAM_CSV`, emails already normalized).
    pub fn set_team(&self, closures: HashMap<String, Vec<String>>) {
        *self.team_file.lock().unwrap() = Some(closures);
        self.recompute_team();
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn recompute_team(&self) {
        let file = self.team_file.lock().unwrap();
        let pushed = self.team_pushed.lock().unwrap();
        *self.team.lock().unwrap() = team_of(file.as_ref(), pushed.as_ref());
    }
}

/// The team closures in force from a server's own csv (`AMPUP_MEMORY_TEAM_CSV`) and the one pushed to the
/// directory (`POST /team`, until `DELETE /team` clears it). Either alone is the team. With both, the team is
/// the people both list, each with the groups the pushed csv (the later word, a reorg) gives them: someone
/// taken out of either (a leaver removed from the config file after a push, or from a push while the file
/// still lists them) holds no team group, and a joiner needs both.
pub fn team_of(
    file: Option<&HashMap<String, Vec<String>>>,
    pushed: Option<&HashMap<String, Vec<String>>>,
) -> HashMap<String, Vec<String>> {
    match (file, pushed) {
        (None, None) => HashMap::new(),
        (Some(t), None) | (None, Some(t)) => t.clone(),
        (Some(f), Some(p)) => p
            .iter()
            .filter(|(person, _)| f.contains_key(*person))
            .map(|(person, groups)| (person.clone(), groups.clone()))
            .collect(),
    }
}

impl ClosureStore {
    /// Bumps whenever any caller's closure was edited; a cached principal is only good for the
    /// generation it was made in.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn apply(&self, user: &str, mut groups: Vec<String>) -> Vec<String> {
        if let Some(team) = self.team.lock().unwrap().get(&norm_id(user)) {
            for g in team {
                if !groups.contains(g) {
                    groups.push(g.clone());
                }
            }
        }
        let edits = self.edits.lock().unwrap();
        match edits.get(&norm_id(user)) {
            None => groups,
            Some(e) => {
                let mut g: Vec<String> = groups
                    .into_iter()
                    .filter(|g| !e.remove.contains(g))
                    .collect();
                for a in &e.add {
                    if !g.contains(a) {
                        g.push(a.clone());
                    }
                }
                g
            }
        }
    }
}

/// Slack episodes. An episode's ACL names its channel, never its members; membership lives in each
/// member's closure (`closure_entry`), so a join or leave touches one caller and no episode.
/// The group a record shared with a whole org names (`group:<org>.all`): everyone the directory admits to the
/// org's memory holds it there, its members and its own keys alike. Not the bare org name, which a source's
/// own group (a Google group `acme`, the `default` org) could share.
pub fn org_wide(org: &str) -> String {
    format!("{org}.all")
}

pub mod slack {
    /// ACL entries for an episode from a Slack channel. Public channels are readable by the whole
    /// org; private channels, DMs and group DMs by whoever holds that channel in their closure.
    /// The org prefix keeps tenants' channel ids from colliding.
    pub fn episode_acl(org: &str, channel_id: &str, is_private: bool) -> Vec<String> {
        if is_private {
            vec![format!("group:{}", closure_entry(org, channel_id))]
        } else {
            vec![format!("group:{org}")]
        }
    }

    /// The closure entry that holds a member's access to a channel's history.
    pub fn closure_entry(org: &str, channel_id: &str) -> String {
        format!("{org}.slack.{channel_id}")
    }
}
