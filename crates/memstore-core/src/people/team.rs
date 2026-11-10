//! A team pilot without an IdP: a CSV of people (`email,name,manager,role,team`) is the membership
//! source. Each person's closure is derived from it, so a writer can address an episode to a team
//! (`group:team.sales`), a role (`group:role.ae`) or one person and their management chain
//! (`group:chain.amy@x.com`: Amy, her manager, their manager, up to the top). A manager sees what their
//! reports' chain entries grant and nothing a sibling team wrote. The CSV is loaded at start
//! (`MEMSTORE_TEAM_CSV`) and replaced whole by an admin `POST /team`; `memstore team check` shows what
//! each person would hold, and `memstore team keys` issues one API key per person to paste into a client.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::auth::{self, is_acl_id, norm_id};

/// One row of the CSV. Emails are compared as [`norm_id`] compares them.
#[derive(Debug, Clone, PartialEq)]
pub struct Person {
    pub email: String,
    pub name: String,
    pub manager: Option<String>,
    pub role: Option<String>,
    pub team: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Team {
    people: Vec<Person>,
    by_email: HashMap<String, usize>,
}

/// A group name from free text: ASCII letters and digits lower-cased, anything else a single `-`. `Sales
/// (West)` and `sales west` are one team; the result is an ACL id.
pub fn slug(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    (!out.is_empty()).then_some(out)
}

impl Team {
    /// Parse the CSV. The header names the columns (any order, any case); `email` is required, the
    /// rest optional. Every manager must be a listed person, no one may be their own ancestor, and
    /// every email must be usable as an ACL id.
    pub fn parse(csv: &str) -> Result<Team> {
        let mut rdr = csv::ReaderBuilder::new()
            .trim(csv::Trim::All)
            .flexible(true)
            .from_reader(csv.as_bytes());
        let headers: Vec<String> = rdr
            .headers()
            .context("csv header")?
            .iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .collect();
        let col = |name: &str| headers.iter().position(|h| h == name);
        let email_col = col("email").context("the csv needs an `email` column")?;
        let (name_col, manager_col, role_col, team_col) =
            (col("name"), col("manager"), col("role"), col("team"));
        let cell = |rec: &csv::StringRecord, c: Option<usize>| -> Option<String> {
            c.and_then(|c| rec.get(c))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        let mut people = Vec::new();
        let mut by_email = HashMap::new();
        for (i, rec) in rdr.records().enumerate() {
            let rec = rec.with_context(|| format!("csv line {}", i + 2))?;
            let Some(email) = cell(&rec, Some(email_col)) else {
                continue; // a blank line
            };
            let email = norm_id(&email);
            anyhow::ensure!(
                email.contains('@') && is_acl_id(&email),
                "line {}: {email:?} is not an email usable as an id",
                i + 2
            );
            anyhow::ensure!(
                by_email.insert(email.clone(), people.len()).is_none(),
                "line {}: {email} is listed twice",
                i + 2
            );
            let manager = cell(&rec, manager_col).map(|m| norm_id(&m));
            for (what, v) in [
                ("role", cell(&rec, role_col)),
                ("team", cell(&rec, team_col)),
            ] {
                if let Some(v) = &v {
                    anyhow::ensure!(
                        slug(v).is_some(),
                        "line {}: {what} {v:?} has no letters or digits",
                        i + 2
                    );
                }
            }
            people.push(Person {
                name: cell(&rec, name_col).unwrap_or_else(|| email.clone()),
                email,
                manager,
                role: cell(&rec, role_col),
                team: cell(&rec, team_col),
            });
        }
        let team = Team { people, by_email };
        for p in &team.people {
            if let Some(m) = &p.manager {
                anyhow::ensure!(
                    team.by_email.contains_key(m),
                    "{}: manager {m} is not in the csv",
                    p.email
                );
                anyhow::ensure!(m != &p.email, "{}: is their own manager", p.email);
            }
            // Walk up; a chain longer than the team is a cycle.
            let mut seen = 0;
            let mut cur = p.manager.as_deref();
            while let Some(m) = cur {
                seen += 1;
                anyhow::ensure!(
                    seen <= team.people.len(),
                    "{}: management chain loops",
                    p.email
                );
                cur = team.people[team.by_email[m]].manager.as_deref();
            }
        }
        Ok(team)
    }

    pub fn people(&self) -> &[Person] {
        &self.people
    }

    pub fn get(&self, email: &str) -> Option<&Person> {
        self.by_email.get(&norm_id(email)).map(|&i| &self.people[i])
    }

    /// The closure entries a person holds: their team and role, their own chain, and the chain of
    /// everyone under them. Sorted; empty for someone not in the csv.
    pub fn closure(&self, email: &str) -> Vec<String> {
        let Some(p) = self.get(email) else {
            return vec![];
        };
        let mut out = HashSet::new();
        if let Some(t) = p.team.as_deref().and_then(slug) {
            out.insert(format!("team.{t}"));
        }
        if let Some(r) = p.role.as_deref().and_then(slug) {
            out.insert(format!("role.{r}"));
        }
        for q in &self.people {
            if q.email == p.email || self.reports_to(q, &p.email) {
                out.insert(format!("chain.{}", q.email));
            }
        }
        let mut v: Vec<String> = out.into_iter().collect();
        v.sort();
        v
    }

    /// Everyone's closure, by email.
    pub fn closures(&self) -> BTreeMap<String, Vec<String>> {
        self.people
            .iter()
            .map(|p| (p.email.clone(), self.closure(&p.email)))
            .collect()
    }

    /// Whether `manager` is anywhere above `p`.
    fn reports_to(&self, p: &Person, manager: &str) -> bool {
        let mut cur = p.manager.as_deref();
        let mut hops = 0;
        while let Some(m) = cur {
            if m == manager {
                return true;
            }
            hops += 1;
            if hops > self.people.len() {
                return false; // parse refuses loops; belt and braces
            }
            cur = self.people[self.by_email[m]].manager.as_deref();
        }
        false
    }
}

/// A fresh API key for a pilot user. The key is shown once; only its sha256 goes in the keys file.
pub fn new_key() -> String {
    use rand::RngCore;
    let mut b = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut b);
    format!("{}{}", auth::KEY_PREFIX, auth::hex(&b))
}

/// Newly issued (email, key) pairs, and every key row to write.
pub type IssuedKeys = (Vec<(String, String)>, Vec<Value>);

/// Issue one API key per person in `team` and add them to the keys file's rows (`MEMSTORE_API_KEYS`
/// format), replacing any earlier row whose id is that email. `only` limits it to those people;
/// otherwise everyone without a row gets one, and `rotate` re-issues for everyone. Returns the new rows'
/// (email, key) pairs and the full rows to write.
pub fn issue_keys(
    team: &Team,
    existing: &[Value],
    only: &[String],
    rotate: bool,
) -> Result<IssuedKeys> {
    let only: HashSet<String> = only.iter().map(|e| norm_id(e)).collect();
    for e in &only {
        anyhow::ensure!(team.get(e).is_some(), "{e} is not in the csv");
    }
    let had: HashSet<String> = existing
        .iter()
        .filter_map(|r| r["id"].as_str().map(norm_id))
        .collect();
    let mut rows: Vec<Value> = existing.to_vec();
    let mut issued = Vec::new();
    for p in team.people() {
        let wanted = if only.is_empty() {
            rotate || !had.contains(&p.email)
        } else {
            only.contains(&p.email)
        };
        if !wanted {
            continue;
        }
        let key = new_key();
        rows.retain(|r| r["id"].as_str().map(norm_id).as_deref() != Some(&p.email));
        rows.push(json!({
            "id": p.email,
            "sha256": auth::hex(&<sha2::Sha256 as sha2::Digest>::digest(key.as_bytes())),
            "user": p.email,
            "groups": [],
            "admin": false,
        }));
        issued.push((p.email.clone(), key));
    }
    Ok((issued, rows))
}
