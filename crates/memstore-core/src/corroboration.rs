//! Who supports a fact: how many independent sources say what it says. A value said again is no news: its fact
//! keeps the time it was first stated, and only a source independent of the ones that stated it counts as support or
//! as stating it again. A source is independent of a fact when it is none of the sources that stated the fact
//! (by [`crate::origin::sayer`]: a note's author, else the source itself), its words aren't an agent's
//! ([`crate::origin`]) and it isn't an echo: a note whose writer was served the fact by a read just before writing it
//! (reconcile reads what the server recorded of that, and drops an echo before anything here sees it).
//!
//! Reconcile drops a new fact that gives the value an open fact gives. Where its episode is independent of that fact,
//! it records that the episode stated the fact again: an engine document of ids only, `restated/<fact>/<episode>`,
//! from a [`crate::conflicts::SOURCE`] source, committed with the facts reconcile kept, as of when the episode
//! happened. No reader reads the record itself ([`ACL`]): a read counts the episode only for a reader who may read
//! it. Withdrawing the extractor takes its records too, and an erasure of the episode or of the fact deletes the
//! record (`Erasure`, as it does a record of a conflict). A source saying the same thing again, an agent restating
//! what it read, and an echo leave no record.
//!
//! - [`count`]: the independent sources of a fact and of the facts giving its value (as a search folds them), as a
//!   reader may read them. Reads serve it (`corroboration`); nothing ranks by it, so saying something often never
//!   makes it rank first.
//! - [`Said`]: what a value [`crate::freshness`] marks stale was last stated again by, an independent source only.

use std::collections::{HashMap, HashSet};

use crate::origin::Origin;
use crate::{Fact, Principal, Vis};

/// Where restatements are recorded, `restated/<fact>/<episode>`.
pub const PREFIX: &str = "restated/";
/// The ACL of a restatement's record: an entry no reader holds (no sign-in names an address under `.invalid`).
pub const ACL: &str = "user:restated@memstore.invalid";

/// The key of the record that `episode` stated `fact` again.
pub fn key(fact: &str, episode: &str) -> String {
    format!("{PREFIX}{fact}/{episode}")
}

/// The fact and the episode a restatement's key names.
pub fn named(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix(PREFIX).and_then(|r| r.split_once('/'))
}

/// The recorded restatements, from the committed documents' versions by key: each fact with the episodes that stated
/// it again and when. Only `records` count, the committed episodes from a [`crate::conflicts::SOURCE`] source that
/// aren't retracted: a document a caller files under [`PREFIX`] records nothing.
pub(crate) fn recorded(
    documents: &HashMap<String, Vec<(i64, String)>>,
    records: &HashSet<String>,
) -> HashMap<String, Vec<(i64, String)>> {
    let mut out: HashMap<String, Vec<(i64, String)>> = HashMap::new();
    for (key, versions) in documents {
        let Some((fact, episode)) = named(key) else {
            continue;
        };
        let Some(at) = versions
            .iter()
            .filter(|(_, id)| records.contains(id))
            .map(|(t, _)| *t)
            .min()
        else {
            continue;
        };
        out.entry(fact.to_string())
            .or_default()
            .push((at, episode.to_string()));
    }
    out
}

/// Who stated `f` (the sayers of the episodes it cites and of those that stated it again) that `who` may read, as
/// `vis` holds them; with `agents`, an agent's too. The first state holding an episode says.
fn sayers(vis: &[&Vis], who: &Principal, f: &Fact, agents: bool) -> HashSet<u64> {
    let bot = f.cite.as_deref().is_some_and(crate::origin::by_bot);
    let cited = f
        .source_episodes
        .iter()
        .enumerate()
        .map(|(i, e)| (e.as_str(), i == 0 && bot));
    let again = vis
        .iter()
        .flat_map(|v| v.restated(&f.fact_id))
        .map(|(_, e)| (e.as_str(), false));
    cited
        .chain(again)
        .filter_map(|(e, bot)| {
            let v = vis.iter().find(|v| v.can_see_episode(e, who))?;
            let agent = bot || v.episode_origin(e) == Some(Origin::Agent);
            (agents || !agent).then_some(v.episode_sayer(e)?)
        })
        .collect()
}

/// How many independent sources `who` may read say what `facts` say (a fact and the facts giving its value): the
/// distinct sources of the episodes they cite and that stated them again, an agent's left out.
pub fn count(vis: &[&Vis], who: &Principal, facts: &[&Fact]) -> usize {
    facts
        .iter()
        .flat_map(|f| sayers(vis, who, f, false))
        .collect::<HashSet<u64>>()
        .len()
}

/// What tells a value stated again from one only repeated, as `who` may read the states `vis` hold
/// ([`crate::freshness::Stated`]).
pub struct Said<'a> {
    pub vis: Vec<&'a Vis>,
    pub who: &'a Principal,
}

impl crate::freshness::Stated for Said<'_> {
    fn again(&self, f: &Fact, by: &Fact) -> bool {
        let theirs = sayers(&self.vis, self.who, f, true);
        sayers(&self.vis, self.who, by, false)
            .iter()
            .any(|s| !theirs.contains(s))
    }

    fn restated(&self, f: &Fact) -> Option<i64> {
        self.vis
            .iter()
            .flat_map(|v| v.restated(&f.fact_id))
            .filter(|(_, e)| self.vis.iter().any(|v| v.can_see_episode(e, self.who)))
            .map(|(t, _)| *t)
            .max()
    }
}
