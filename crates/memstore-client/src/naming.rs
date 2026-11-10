//! The product's names, and the old ones that keep working. The product is AmpUp Memory: its command is
//! `ampup-memory` (the store and server) and `ampup-memory-client` (what a person's machine runs), its settings are
//! `AMPUP_MEMORY_*` variables, and a machine keeps its contexts and sign-ins in `~/.ampup-memory`. Before, all of
//! these were memstore's: `memstore`, `memstore-client`, `MEMSTORE_*` and `~/.memstore`, and those still work:
//!
//! - the release archives, install.sh and the images put a `memstore` (and `memstore-client`) link beside the
//!   binary, which behaves the same under either name;
//! - [`startup`], first thing in each binary's `main`, reads every `MEMSTORE_X` as `AMPUP_MEMORY_X` (the new
//!   name wins where both are set), says once which old names it found, and moves an old `~/.memstore` to
//!   `~/.ampup-memory`, leaving a link where it was.
//!
//! So everything else reads only the new names. CI refuses `"MEMSTORE_…"` outside the few places listed in
//! `.github/scripts/old-env-names.sh`: this module, and the names other programs (an engine image older than
//! this one, the control plane) read or write.
//!
//! Crate names, Rust paths, metric names, HTTP headers and routes, catalog namespaces and the API key prefix
//! keep memstore's name: they are interfaces, not what a person types.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// What the product is called in what a person reads.
pub const PRODUCT: &str = "AmpUp Memory";
/// The store and server's command.
pub const BIN: &str = "ampup-memory";
/// The client's command.
pub const CLIENT_BIN: &str = "ampup-memory-client";
/// The store and server's command before the rename: a link to [`BIN`] where it is installed.
pub const OLD_BIN: &str = "memstore";
/// The client's command before the rename: a link to [`CLIENT_BIN`] where it is installed (removed in a later
/// release).
pub const OLD_CLIENT_BIN: &str = "memstore-client";
/// The name the MCP server gives itself (`serverInfo`) and the installers give its entries in agents' configs.
pub const MCP_SERVER: &str = "ampup-memory";
/// What the MCP server's entries were called before: installing replaces them, uninstalling removes them.
pub const OLD_MCP_SERVERS: [&str; 2] = ["memstore", "ampup-enterprise-brain"];
/// Every setting's prefix.
pub const ENV_PREFIX: &str = "AMPUP_MEMORY_";
/// Every setting's prefix before the rename, still read ([`startup`]).
pub const OLD_ENV_PREFIX: &str = "MEMSTORE_";
/// A machine's state directory under `$HOME`: contexts, sign-ins, a local store, a deployment's credential.
pub const STATE_DIR: &str = ".ampup-memory";
/// The state directory before the rename: moved to [`STATE_DIR`] on first run, a link to it left here.
pub const OLD_STATE_DIR: &str = ".memstore";

/// The release this was built as (`v0.1.0-rc.7`), from the release workflow's `AMPUP_MEMORY_RELEASE` (or, from a
/// workflow not yet renamed, `MEMSTORE_RELEASE`), if it set one.
pub const RELEASE: Option<&str> = match option_env!("AMPUP_MEMORY_RELEASE") {
    Some(v) => Some(v),
    None => option_env!("MEMSTORE_RELEASE"),
};

/// The current name of setting `name`: `MEMSTORE_X` is `AMPUP_MEMORY_X`; any other name is itself.
pub fn env_name(name: &str) -> Cow<'_, str> {
    match name.strip_prefix(OLD_ENV_PREFIX) {
        Some(rest) if !rest.is_empty() => Cow::Owned(format!("{ENV_PREFIX}{rest}")),
        _ => Cow::Borrowed(name),
    }
}

/// The old name of setting `name`: `AMPUP_MEMORY_X` was `MEMSTORE_X`; any other name is itself. For what
/// another program reads by the old name (an engine image from before the rename).
pub fn old_env_name(name: &str) -> Cow<'_, str> {
    match name.strip_prefix(ENV_PREFIX) {
        Some(rest) if !rest.is_empty() => Cow::Owned(format!("{OLD_ENV_PREFIX}{rest}")),
        _ => Cow::Borrowed(name),
    }
}

/// `$HOME/.ampup-memory`.
pub fn state_dir() -> PathBuf {
    home().join(STATE_DIR)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
}

/// What [`adopt_old_env`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OldEnv {
    /// Old names read as their new ones.
    pub adopted: Vec<String>,
    /// Old names set beside their new ones, so not read.
    pub ignored: Vec<String>,
}

impl OldEnv {
    /// The one line a process says about its old names, if it found any.
    pub fn notice(&self) -> Option<String> {
        if self.adopted.is_empty() && self.ignored.is_empty() {
            return None;
        }
        let new = |v: &[String]| {
            v.iter()
                .map(|n| env_name(n).into_owned())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut s = String::from("deprecated: MEMSTORE_* settings are AMPUP_MEMORY_* now;");
        if !self.adopted.is_empty() {
            s.push_str(&format!(
                " read {} as {} (rename them: the old names stop working in a later release)",
                self.adopted.join(", "),
                new(&self.adopted)
            ));
        }
        if !self.ignored.is_empty() {
            s.push_str(&format!(
                "{} ignored {}: {} set too, and the new name wins",
                if self.adopted.is_empty() { "" } else { ";" },
                self.ignored.join(", "),
                new(&self.ignored)
            ));
        }
        Some(s)
    }
}

/// Every `MEMSTORE_X` in this process's environment becomes `AMPUP_MEMORY_X`, unless `AMPUP_MEMORY_X` is set
/// too (the new name wins), and the old name is removed either way: the process, and every process it starts,
/// sees the new names alone.
///
/// # Safety
///
/// It writes the environment: call it before any other thread starts (first in `main`, before a runtime).
pub unsafe fn adopt_old_env() -> OldEnv {
    let mut found = OldEnv::default();
    let old: Vec<(String, std::ffi::OsString)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v)))
        .filter(|(k, _)| k.len() > OLD_ENV_PREFIX.len() && k.starts_with(OLD_ENV_PREFIX))
        .collect();
    for (name, value) in old {
        let new = env_name(&name).into_owned();
        if std::env::var_os(&new).is_some() {
            found.ignored.push(name.clone());
        } else {
            // SAFETY: the caller's: no other thread runs yet.
            unsafe { std::env::set_var(&new, value) };
            found.adopted.push(name.clone());
        }
        // SAFETY: as above.
        unsafe { std::env::remove_var(&name) };
    }
    found.adopted.sort();
    found.ignored.sort();
    let _ = FOUND.set(found.adopted.clone());
    found
}

/// The old names [`adopt_old_env`] read as new ones.
static FOUND: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Whether setting `name` (a new name) came from its old one in this process's environment: for a name another
/// program reads from the same environment (an agent reading its token), which must be given the name it has.
pub fn set_by_old_name(name: &str) -> bool {
    let old = old_env_name(name);
    FOUND.get().is_some_and(|f| f.iter().any(|n| *n == old))
}

/// What [`migrate_state_dir`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Migrated {
    /// Nothing to move: no old directory, or the new one is there already.
    Nothing,
    /// The old directory moved to the new path, with a link to it where it was.
    Moved,
    /// Moved, but the link couldn't be made (the old path is free; nothing is lost).
    MovedNoLink(String),
    /// Couldn't move it (it is still at the old path, and read from there).
    Failed(String),
}

/// Moves `home/.memstore` to `home/.ampup-memory` when the new one doesn't exist and the old one does, and
/// leaves a link at the old path, so an older binary (or a script) that names it still finds everything. Never
/// copies or deletes: one rename, which either happens whole or not at all. Two first runs at once: one rename
/// wins; the other finds the old path gone or a link (a rename can't put a link over a directory) and leaves it.
pub fn migrate_state_dir(home: &Path) -> Migrated {
    let old = home.join(OLD_STATE_DIR);
    let new = home.join(STATE_DIR);
    if std::fs::symlink_metadata(&new).is_ok() {
        return Migrated::Nothing;
    }
    match std::fs::symlink_metadata(&old) {
        // Already a link (to the new one, by an earlier run, or the person's own): leave it.
        Ok(m) if m.file_type().is_symlink() => return Migrated::Nothing,
        Ok(m) if m.is_dir() => {}
        _ => return Migrated::Nothing,
    }
    if let Err(e) = std::fs::rename(&old, &new) {
        // Another first run moved it meanwhile: the new one is there now.
        if std::fs::symlink_metadata(&new).is_ok() {
            return Migrated::Nothing;
        }
        return Migrated::Failed(format!("move {} to {}: {e}", old.display(), new.display()));
    }
    #[cfg(unix)]
    let link = std::os::unix::fs::symlink(STATE_DIR, &old);
    #[cfg(not(unix))]
    let link: std::io::Result<()> = Ok(());
    match link {
        Ok(()) => Migrated::Moved,
        // Another run made it: the same link.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Migrated::Moved,
        Err(e) => {
            Migrated::MovedNoLink(format!("link {} to {}: {e}", old.display(), new.display()))
        }
    }
}

/// First thing in each binary's `main`: the old settings read as the new ones ([`adopt_old_env`]) and the old
/// state directory moved ([`migrate_state_dir`]), each saying so once on stderr.
///
/// # Safety
///
/// As [`adopt_old_env`]: before any other thread starts.
pub unsafe fn startup() {
    // SAFETY: the caller's.
    let found = unsafe { adopt_old_env() };
    if let Some(n) = found.notice() {
        eprintln!("{BIN}: {n}");
    }
    // A state directory named outright is used as it is.
    if std::env::var_os("AMPUP_MEMORY_CONFIG_DIR").is_some_and(|d| !d.is_empty()) {
        return;
    }
    match migrate_state_dir(&home()) {
        Migrated::Nothing => {}
        Migrated::Moved => eprintln!(
            "{BIN}: moved ~/{OLD_STATE_DIR} to ~/{STATE_DIR} (~/{OLD_STATE_DIR} links to it)"
        ),
        Migrated::MovedNoLink(e) => {
            eprintln!("{BIN}: moved ~/{OLD_STATE_DIR} to ~/{STATE_DIR}, but {e}")
        }
        Migrated::Failed(e) => eprintln!("{BIN}: couldn't {e}: using ~/{OLD_STATE_DIR}"),
    }
}

/// The state directory to use: `~/.ampup-memory`, or `~/.memstore` where moving it failed (it is still there).
pub fn current_state_dir() -> PathBuf {
    let new = state_dir();
    let old = home().join(OLD_STATE_DIR);
    if std::fs::symlink_metadata(&new).is_err() && std::fs::metadata(&old).is_ok_and(|m| m.is_dir())
    {
        return old;
    }
    new
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_map_both_ways() {
        assert_eq!(env_name("MEMSTORE_URL"), "AMPUP_MEMORY_URL");
        assert_eq!(env_name("AMPUP_MEMORY_URL"), "AMPUP_MEMORY_URL");
        assert_eq!(env_name("MEMSTORE_"), "MEMSTORE_");
        assert_eq!(env_name("HOME"), "HOME");
        assert_eq!(old_env_name("AMPUP_MEMORY_FLAG_X"), "MEMSTORE_FLAG_X");
        assert_eq!(old_env_name("OPENAI_API_KEY"), "OPENAI_API_KEY");
    }

    #[test]
    fn state_dir_moves_once_and_links() {
        let home = tempfile::tempdir().unwrap();
        let old = home.path().join(OLD_STATE_DIR);
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("config.json"), "{}").unwrap();
        assert_eq!(migrate_state_dir(home.path()), Migrated::Moved);
        let new = home.path().join(STATE_DIR);
        assert_eq!(
            std::fs::read_to_string(new.join("config.json")).unwrap(),
            "{}"
        );
        assert!(
            std::fs::symlink_metadata(&old)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(old.join("config.json")).unwrap(),
            "{}"
        );
        // Again: nothing to do.
        assert_eq!(migrate_state_dir(home.path()), Migrated::Nothing);
    }

    #[test]
    fn concurrent_first_runs_lose_nothing() {
        let home = tempfile::tempdir().unwrap();
        let old = home.path().join(OLD_STATE_DIR);
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("credentials.json"), "secret").unwrap();
        let runs: Vec<_> = (0..8)
            .map(|_| {
                let h = home.path().to_path_buf();
                std::thread::spawn(move || migrate_state_dir(&h))
            })
            .collect();
        let done: Vec<_> = runs.into_iter().map(|r| r.join().unwrap()).collect();
        assert!(
            done.iter()
                .all(|d| matches!(d, Migrated::Moved | Migrated::Nothing)),
            "{done:?}"
        );
        assert_eq!(
            done.iter().filter(|d| **d == Migrated::Moved).count(),
            1,
            "{done:?}"
        );
        let new = home.path().join(STATE_DIR);
        assert_eq!(
            std::fs::read_to_string(new.join("credentials.json")).unwrap(),
            "secret"
        );
        assert_eq!(
            std::fs::read_to_string(old.join("credentials.json")).unwrap(),
            "secret"
        );
    }

    #[test]
    fn a_new_dir_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(OLD_STATE_DIR)).unwrap();
        std::fs::create_dir(home.path().join(STATE_DIR)).unwrap();
        assert_eq!(migrate_state_dir(home.path()), Migrated::Nothing);
        assert!(home.path().join(OLD_STATE_DIR).is_dir());
    }

    #[test]
    fn notice_names_what_it_read() {
        let f = OldEnv {
            adopted: vec!["MEMSTORE_URL".into()],
            ignored: vec!["MEMSTORE_ORG".into()],
        };
        let n = f.notice().unwrap();
        assert!(n.contains("read MEMSTORE_URL as AMPUP_MEMORY_URL"), "{n}");
        assert!(
            n.contains("ignored MEMSTORE_ORG: AMPUP_MEMORY_ORG set too"),
            "{n}"
        );
        assert_eq!(OldEnv::default().notice(), None);
    }
}
