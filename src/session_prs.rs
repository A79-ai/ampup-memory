//! The pull requests a session opened or pushed to, and what later came of them: an outcome no model reads in.
//!
//! **Linking.** As a session ends, its transcript is read for the PRs it opened or pushed to ([`links_in`]): a
//! `gh pr create` (or a `create_pull_request` tool) whose result names the PR's URL, and a `git push` whose result
//! names the repository and the branch (a PR on that branch is matched to it later). A harness whose transcript
//! carries no tool calls (Codex's) gives what the agent said: a PR's URL in a line saying it opened one. The links
//! are kept on the session (`prs`), which Studio shows.
//!
//! **Grading.** What became of a PR reaches its sessions as events (`POST /sessions/prs`, [`crate::sessions::pr_event`]),
//! sent for the person whose sessions they are (a recorder key names them in `x-memstore-as`, as a CI job does
//! at a merge): `merged`, `closed` (unmerged), `reverted`, and `regression` (a later PR said "Regression from #N"
//! of it). [`outcome`] turns them into the session's reward, the surest kind ([`crate::advice`]'s outcome): a PR
//! reverted, or one a later PR fixed a regression of, is a failure; one merged is a success, a firm one once it
//! has stood [`STANDING_DAYS`] days with neither; one closed unmerged says nothing either way.

use serde::{Deserialize, Serialize};

/// Days a merged PR stands with no revert or regression before its sessions count as firmly right.
pub const STANDING_DAYS: i64 = 14;

/// The most PRs one session keeps.
const MAX_PRS: usize = 20;
/// The most events one PR keeps.
const MAX_EVENTS: usize = 20;

/// A PR a session opened or pushed to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Pr {
    /// `owner/repo`.
    pub repo: String,
    /// Its number, once known: from its URL, or from an event matched by its branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    /// The branch pushed to, for a push that named no PR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `created` (a tool opened it), `pushed` (a push to its branch) or `said` (the agent said it opened it).
    pub how: String,
    /// What later came of it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Event>,
}

/// What came of a PR.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// `merged`, `closed`, `reverted` or `regression`.
    pub kind: String,
    /// When (µs).
    pub at: i64,
    /// The PR that reverted it, or that fixed a regression it caused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<u64>,
}

/// The event kinds [`crate::sessions::pr_event`] takes.
pub const KINDS: [&str; 4] = ["merged", "closed", "reverted", "regression"];

impl Pr {
    /// `owner/repo#number`, or its branch while its number isn't known.
    pub(crate) fn named(&self) -> String {
        match self.number {
            Some(n) => format!("{}#{n}", self.repo),
            None => format!(
                "{} (branch {})",
                self.repo,
                self.branch.as_deref().unwrap_or("?")
            ),
        }
    }

    /// Whether an event about `repo`'s PR `number` on `branch` is about this one.
    pub fn is(&self, repo: &str, number: u64, branch: Option<&str>) -> bool {
        self.repo.eq_ignore_ascii_case(repo)
            && (self.number == Some(number)
                || (self.number.is_none() && branch.is_some() && self.branch.as_deref() == branch))
    }
}

/// The PRs transcript `t` shows a session opened or pushed to, as the plugin's trace writes tool calls
/// (`  tool <name> <arguments>`, its result on the next line after `    -> `), each once.
pub fn links_in(t: &str) -> Vec<Pr> {
    let lines: Vec<&str> = t.lines().collect();
    let mut out: Vec<Pr> = vec![];
    let mut calls = 0;
    for (i, line) in lines.iter().enumerate() {
        let Some(call) = line.trim_start().strip_prefix("tool ") else {
            continue;
        };
        calls += 1;
        let result = lines
            .get(i + 1)
            .and_then(|l| l.trim_start().strip_prefix("-> "))
            .unwrap_or_default();
        let creates = call.contains("gh pr create") || call.contains("create_pull_request");
        if creates {
            for (repo, n) in pr_urls(result) {
                add(&mut out, repo, Some(n), None, "created");
            }
        } else if call.contains("git push")
            && let Some(repo) = pushed_repo(result)
            && let Some(branch) = pushed_branch(call, result)
        {
            add(&mut out, repo, None, Some(branch), "pushed");
        }
    }
    // A transcript with no tool calls (another harness's): the PRs the agent said it opened.
    if calls == 0 {
        for line in &lines {
            let l = line.to_ascii_lowercase();
            let said = ["opened", "created", "raised", "filed", "pr is up", "pr up"]
                .iter()
                .any(|w| l.contains(w));
            if said && !l.trim_start().starts_with("user:") {
                for (repo, n) in pr_urls(line) {
                    add(&mut out, repo, Some(n), None, "said");
                }
            }
        }
    }
    out
}

fn add(out: &mut Vec<Pr>, repo: String, number: Option<u64>, branch: Option<String>, how: &str) {
    if out.len() >= MAX_PRS
        || out.iter().any(|p| {
            p.repo.eq_ignore_ascii_case(&repo)
                && ((number.is_some() && p.number == number)
                    || (branch.is_some() && p.branch == branch))
        })
    {
        return;
    }
    out.push(Pr {
        repo,
        number,
        branch,
        how: how.into(),
        events: vec![],
    });
}

/// Each `github.com/<owner>/<repo>/pull/<n>` in `text`.
fn pr_urls(text: &str) -> Vec<(String, u64)> {
    let mut out = vec![];
    let mut rest = text;
    while let Some(at) = rest.find("github.com/") {
        rest = &rest[at + "github.com/".len()..];
        let parts: Vec<&str> = rest.splitn(4, '/').collect();
        if parts.len() >= 4 && parts[2] == "pull" {
            let digits: String = parts[3].chars().take_while(char::is_ascii_digit).collect();
            if let (Ok(n), true) = (
                digits.parse::<u64>(),
                name_ok(parts[0]) && name_ok(parts[1]),
            ) {
                out.push((format!("{}/{}", parts[0], parts[1]), n));
            }
        }
    }
    out
}

/// The repository a push's result names: `To github.com:<owner>/<repo>.git`, or its https form.
fn pushed_repo(result: &str) -> Option<String> {
    for marker in ["github.com:", "github.com/"] {
        let mut rest = result;
        while let Some(at) = rest.find(marker) {
            rest = &rest[at + marker.len()..];
            let mut parts = rest.splitn(3, '/');
            let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
                continue;
            };
            let repo: String = repo
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .collect();
            let repo = repo.trim_end_matches(".git").trim_end_matches('.');
            if name_ok(owner) && name_ok(repo) {
                return Some(format!("{owner}/{repo}"));
            }
        }
    }
    None
}

/// The branch a push went to: `<from> -> <to>` in its result, a `pull/new/<branch>` link, or the last argument of
/// the command (`git push -u origin <branch>`). Never `HEAD`.
fn pushed_branch(call: &str, result: &str) -> Option<String> {
    let ok = |b: &str| {
        let b = b.trim_matches(|c: char| c == '"' || c == '\'' || c == ',' || c == '}');
        (!b.is_empty() && b != "HEAD" && b.chars().all(branch_char)).then(|| b.to_string())
    };
    if let Some(at) = result.find("pull/new/") {
        let b: String = result[at + "pull/new/".len()..]
            .chars()
            .take_while(|c| branch_char(*c))
            .collect();
        if let Some(b) = ok(&b) {
            return Some(b);
        }
    }
    let words: Vec<&str> = result.split_whitespace().collect();
    if let Some(i) = words.iter().position(|w| *w == "->")
        && let Some(b) = words.get(i + 1).and_then(|w| ok(w))
    {
        return Some(b.trim_start_matches("refs/heads/").to_string());
    }
    let cmd = call.split("git push").nth(1)?;
    let cmd = cmd
        .split(['&', ';', '|', '"', '\\'])
        .next()
        .unwrap_or_default();
    let args: Vec<&str> = cmd
        .split_whitespace()
        .filter(|a| !a.starts_with('-'))
        .collect();
    // `git push origin <branch>` or `git push origin <local>:<remote>`; `git push` alone names none.
    let last = args.get(1)?;
    ok(last.rsplit(':').next().unwrap_or(last))
}

fn branch_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')
}

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `links` added to `into`, each once, with their events: a PR `into` holds by its branch takes the number one
/// names, and the events of both. PRs and their events only grow, so the PRs of every row a session was written as
/// are one list ([`crate::qa_log::list`]): a row another pod wrote from a read taken before an event landed loses
/// it from no one.
pub fn merge(into: &mut Vec<Pr>, links: Vec<Pr>) {
    for l in links {
        if let Some(have) = into.iter_mut().find(|p| {
            p.repo.eq_ignore_ascii_case(&l.repo)
                && ((l.number.is_some() && p.number == l.number)
                    || (l.branch.is_some() && p.branch == l.branch))
        }) {
            if have.number.is_none() {
                have.number = l.number;
            }
            if have.branch.is_none() {
                have.branch = l.branch;
            }
            if have.how != "created" && l.how == "created" {
                have.how = l.how;
            }
            for e in l.events {
                note(have, e);
            }
            continue;
        }
        if into.len() < MAX_PRS {
            into.push(l);
        }
    }
    dedupe(into);
}

/// `prs` with each PR once: a push linked by its branch and an opening linked by its number are one PR once an
/// event names both, kept as the opening (with the branch) and holding the events of both.
pub fn dedupe(prs: &mut Vec<Pr>) {
    let mut out: Vec<Pr> = vec![];
    for p in prs.drain(..) {
        match out.iter_mut().find(|o| {
            o.repo.eq_ignore_ascii_case(&p.repo) && o.number.is_some() && o.number == p.number
        }) {
            Some(o) => {
                if o.branch.is_none() {
                    o.branch = p.branch;
                }
                if p.how == "created" {
                    o.how = p.how;
                }
                for e in p.events {
                    note(o, e);
                }
            }
            None => out.push(p),
        }
    }
    *prs = out;
}

/// Adds `e` to `pr`'s events, once; whether it was new.
pub fn note(pr: &mut Pr, e: Event) -> bool {
    if pr.events.iter().any(|x| x.kind == e.kind && x.by == e.by) || pr.events.len() >= MAX_EVENTS {
        return false;
    }
    pr.events.push(e);
    true
}

/// The reward what came of a session's PRs gives it, and why, at `now` (µs); none while nothing decisive came of
/// them. Any PR reverted, or that a later PR fixed a regression of, makes it a failure. Else any merged makes it
/// a success: 1 once every merged one has stood [`STANDING_DAYS`] days, 0.5 before.
pub fn outcome(prs: &[Pr], now: i64) -> Option<(f64, String)> {
    for p in prs {
        for e in &p.events {
            let by = e.by.map(|n| format!(" (#{n})")).unwrap_or_default();
            match e.kind.as_str() {
                "reverted" => {
                    return Some((-1.0, format!("its PR {} was reverted{by}", p.named())));
                }
                "regression" => {
                    return Some((
                        -1.0,
                        format!(
                            "a later PR{by} fixed a regression its PR {} caused",
                            p.named()
                        ),
                    ));
                }
                _ => {}
            }
        }
    }
    let merged: Vec<(&Pr, i64)> = prs
        .iter()
        .filter_map(|p| {
            p.events
                .iter()
                .find(|e| e.kind == "merged")
                .map(|e| (p, e.at))
        })
        .collect();
    let (first, _) = merged.first()?;
    let standing = merged
        .iter()
        .all(|(_, at)| now - at >= STANDING_DAYS * 86_400_000_000);
    Some(match standing {
        true => (
            1.0,
            format!(
                "its PR {} merged and stood {STANDING_DAYS} days with no revert or regression",
                first.named()
            ),
        ),
        false => (
            0.5,
            format!(
                "its PR {} merged; no revert or regression reported yet",
                first.named()
            ),
        ),
    })
}
