//! Retention: how long a scope keeps what its sources wrote. A policy is a list of rules, each a source
//! prefix and a maximum age (or `forever`); an episode follows the rule with the longest prefix its source
//! starts with, and is kept when no rule matches. Age is counted from when the record happened at its
//! source (`occurred_at`), not when it was ingested, so re-ingesting an old record doesn't make it young.
//!
//! An episode past its age is gone for every reader at read time, with the facts citing it, as a
//! retracted one is: no job has to run for it to go, and it goes the moment it ages out, not at the next
//! write. Until a purge removes it from storage (`memstore org purge`, or where the org turned it on the nightly
//! window's scheduled purge, once past `AMPUP_MEMORY_PURGE_GRACE` under the policy then and now), loosening the policy
//! brings it back.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// The shortest age a rule may keep: a typo such as `90s` for `90d` must not hide a scope's whole history.
pub const MIN_AGE_SECS: i64 = 86_400;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Retention {
    pub rules: Vec<Rule>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    /// Sources starting with this (`slack:`, `slack:C09:`); empty matches every source.
    pub source_prefix: String,
    /// Seconds an episode is kept after it occurred; `None` keeps it for good.
    pub max_age_secs: Option<i64>,
}

impl Retention {
    /// Rules as the CLI and `/admin` take them: `<prefix>=<age>`, the age in days, weeks or years (`90d`,
    /// `12w`, `7y`) or `forever`; `=365d` (an empty prefix) is every source's default.
    pub fn parse(rules: &[String]) -> Result<Self> {
        let rules = rules
            .iter()
            .map(|r| {
                let (prefix, age) = r
                    .rsplit_once('=')
                    .ok_or_else(|| anyhow::anyhow!("rule {r:?} is not <source prefix>=<age>"))?;
                Ok(Rule {
                    source_prefix: prefix.to_string(),
                    max_age_secs: match age {
                        "forever" => None,
                        a => Some(age_secs(a)?),
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let policy = Retention { rules };
        policy.check()?;
        Ok(policy)
    }

    /// Each prefix once, and no age shorter than [`MIN_AGE_SECS`].
    pub fn check(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for r in &self.rules {
            anyhow::ensure!(
                seen.insert(r.source_prefix.as_str()),
                "source prefix {:?} has two rules",
                r.source_prefix
            );
            if let Some(a) = r.max_age_secs {
                anyhow::ensure!(
                    a >= MIN_AGE_SECS,
                    "{:?}: keep at least a day",
                    r.source_prefix
                );
            }
        }
        Ok(())
    }

    /// How long an episode from `source` is kept, in microseconds; `None` for good.
    pub fn max_age_us(&self, source: &str) -> Option<i64> {
        self.rules
            .iter()
            .filter(|r| source.starts_with(r.source_prefix.as_str()))
            .max_by_key(|r| r.source_prefix.len())
            .and_then(|r| r.max_age_secs)
            .map(|s| s.saturating_mul(1_000_000))
    }

    /// The rules as `<prefix>=<age>`, as they are given.
    pub fn describe(&self) -> Vec<String> {
        self.rules
            .iter()
            .map(|r| {
                let age = match r.max_age_secs {
                    None => "forever".to_string(),
                    Some(s) if s % (365 * 86_400) == 0 => format!("{}y", s / (365 * 86_400)),
                    Some(s) if s % (7 * 86_400) == 0 => format!("{}w", s / (7 * 86_400)),
                    Some(s) => format!("{}d", s / 86_400),
                };
                format!("{}={age}", r.source_prefix)
            })
            .collect()
    }
}

/// `90d`, `12w`, `7y` in seconds.
fn age_secs(age: &str) -> Result<i64> {
    let bad = || {
        anyhow::anyhow!("age {age:?} is a number of days, weeks or years (90d, 12w, 7y) or forever")
    };
    let (n, unit) = age.split_at(age.len().checked_sub(1).ok_or_else(bad)?);
    let n: i64 = n.parse().map_err(|_| bad())?;
    let day = 86_400;
    let per = match unit {
        "d" => day,
        "w" => 7 * day,
        "y" => 365 * day,
        _ => return Err(bad()),
    };
    n.checked_mul(per).filter(|s| *s > 0).ok_or_else(bad)
}
