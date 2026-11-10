//! Similar past errors and how they were fixed: what `get_advice` adds when it is asked about an error, where the
//! scope's `error_leads` setting is on (`Settings::error_leads`; a team's wins over its org's, off by default).
//!
//! **When.** The question carries an error: a Sentry short ID (`API-SERVICE-1AB`) or issue link
//! (`https://<org>.sentry.io/issues/<id>/`) of an issue the caller may read, an exception and its message
//! (`KeyError: 'org_id'`), or stack frames (Python's `File "…", line N, in f`, JavaScript's `at f (…:N:M)`, or the
//! Sentry feed's `path:N in f`). A question with none of these is answered as before.
//!
//! **From what.** The `sentry:` episodes the caller may read, here and in the scopes this one reads (`reads`), one
//! per issue in the Sentry feed format: `# <title> (sentry <project>, …)`, `<short ID> · <link>`,
//! `Culprit:`, `## Stack trace` (in-app frames, `path:line in function`, most recent call last) and `## Fixed by`
//! (`### <repo>#<n>: <title> (merged <day>)`, `Files changed:` and the PR's description). And the merged PRs among
//! the caller's `github:` episodes whose title or description names a Sentry issue (by a short ID of a project
//! the issues show, or a sentry.io issue link): the eng corpus's `PR #<n>: <title>` with `Files changed:`, or the
//! PR ingest's `# <title> (pr <repo>#<n>, merged <day>)` with `## Files changed`. A PR goes with each issue it
//! names; one naming only issues no `sentry:` episode holds (older than Sentry keeps) is a lead of its own. Long
//! episodes are read whole again where the corpus split them into overlapping chunks.
//!
//! **Matched** as a `/sentry-similar` lookup matches, two rankings fused by reciprocal rank (k 10; a tie
//! goes to the first):
//! - deterministic: a past error with the same exception type and culprit (3), the same message once ids,
//!   numbers and quoted values are left out (2), shared in-app frames (1 each; else shared files, 0.5 each) and the
//!   same innermost frame (1), or a non-generic exception type alone (0.5); plus, for its fixes, each of the
//!   question's frames in a file a fix changed (1, the innermost 2). Files so many errors' stacks or fixes run
//!   through that sharing one says little (middleware, decorators, clients: in more than a quarter of the stacks,
//!   or changed by more than a tenth of the fixes, and more than 3) count for neither. A match needs a reason.
//!   Best first, the newer fix on a tie;
//! - by words: BM25 over the words the full-text index keeps, the error's title, culprit and nearest three frames
//!   against each past error's text and its fixes'. No embedding: the lookup
//!   adds no model call to `get_advice`.
//!
//! Each takes its best 3, and the fused best 3 are served. The question's own issue, and a fix naming it, are never
//! a lead of it. Reading a scope's episodes the first time takes a while (thousands of PRs): a call waits
//! [`READ_WAIT`] for them and is otherwise answered without leads, while the reading goes on for the next call.
//! Each episode is read once per server: an episode's id is its text's hash.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::{Principal, Session, Store};

/// Leads served at most.
pub const MAX_LEADS: usize = 3;
/// How long a call waits for a scope's episodes to be read before it is answered without leads.
pub const READ_WAIT: Duration = Duration::from_secs(5);
/// Reciprocal rank fusion's constant.
const RRF_K: f32 = 10.0;
/// BM25's parameters, as the full-text index (and advice routing) has them.
const K1: f32 = 1.2;
const B: f32 = 0.75;
/// A lead's fixes (its newest), and a fix's files and description, as a lead shows them.
const LEAD_FIXES: usize = 3;
const LEAD_FILES: usize = 8;
const LEAD_DESCRIPTION: usize = 400;
/// The most of a description kept in memory.
const KEPT_DESCRIPTION: usize = 1_500;
/// Episodes read in one go.
const READ_BATCH: usize = 500;
/// Parsed episodes kept; cleared past this.
const PARSED_MAX: usize = 400_000;
/// The search text's length at most.
const QUERY_CHARS: usize = 600;
/// The corpus format's chunks: 6,000 characters, each overlapping the last by 500.
const CHUNK: usize = 6_000;
const OVERLAP: usize = 500;

/// Exception types too common to say two errors are alike on their own.
const GENERIC_TYPES: &[&str] = &[
    "Error",
    "Exception",
    "ExceptionGroup",
    "HTTPError",
    "HTTPException",
    "HTTPStatusError",
    "RuntimeError",
    "StepError",
    "TypeError",
    "ValueError",
];
/// What an exception type's name ends with, where the question names one in its own words.
const TYPE_ENDINGS: &[&str] = &[
    "Error",
    "Exception",
    "Exit",
    "Interrupt",
    "Warning",
    "Timeout",
    "Failure",
    "Fault",
    "Rejection",
    "StopIteration",
    "StopAsyncIteration",
];
const CODE: &[&str] = &[".py", ".ts", ".tsx", ".js", ".jsx"];

/// One similar past error and how it was fixed.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Lead {
    pub issue: LeadIssue,
    /// Why it matched, in words.
    pub why: Vec<String>,
    /// The PRs that fixed it, newest first: at most [`LEAD_FIXES`].
    pub fixes: Vec<LeadFix>,
    /// The scope it was read from, when that is another the caller's scope reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct LeadIssue {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct LeadFix {
    /// `<repo>#<number>`.
    pub pr: String,
    pub link: String,
    pub title: String,
    /// The day it was merged, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged: Option<String>,
    /// Its changed files: those of the question's frames first, at most [`LEAD_FILES`].
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub more_files: usize,
    /// Its description's start, at most [`LEAD_DESCRIPTION`] characters.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// An in-app stack frame: a repo path and its function.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Frame {
    path: String,
    function: String,
}

/// What the lookup compares of an error: the question's, or a past issue's.
#[derive(Clone, Debug, Default)]
struct Error {
    /// Sentry's numeric id, from its link.
    id: Option<String>,
    short_id: Option<String>,
    link: Option<String>,
    title: String,
    culprit: String,
    types: Vec<String>,
    /// In-app, outermost call first.
    frames: Vec<Frame>,
}

/// A fixing PR, from a `## Fixed by` entry or a PR episode.
#[derive(Clone, Debug, Default)]
struct Fix {
    repo: String,
    number: u64,
    title: String,
    /// `YYYY-MM-DD`.
    merged: Option<String>,
    files: Vec<String>,
    /// Files changed beyond those listed.
    more: usize,
    description: String,
    /// What its title and description name: short-ID-like words and sentry.io issue ids (a PR episode's only).
    refs: Vec<String>,
    words: Arc<HashSet<String>>,
}

impl Fix {
    fn key(&self) -> (String, u64) {
        (self.repo.to_lowercase(), self.number)
    }
}

/// A `sentry:` episode, read.
#[derive(Debug)]
struct Issue {
    error: Error,
    /// The project's short-ID prefix (`API-SERVICE`), from the short ID or the title line's project.
    prefixes: Vec<String>,
    fixes: Vec<Fix>,
    words: Arc<HashSet<String>>,
}

/// What an episode reads as.
#[derive(Debug)]
enum Parsed {
    Issue(Arc<Issue>),
    Pr(Arc<Fix>),
    Neither,
}

/// Each episode read, by its id (its text's hash, so it never changes).
static PARSED: LazyLock<Mutex<HashMap<String, Arc<Parsed>>>> = LazyLock::new(Default::default);
/// The episodes being read now.
static READING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// Whether `question` carries an error at all (a short-ID-like word, a sentry.io issue link, an exception and its
/// message, or a stack frame): a cheap look, before anything is read.
pub fn asks_about_error(question: &str) -> bool {
    !sentry_ids(question).is_empty()
        || !short_ids(question).is_empty()
        || !exception_lines(question, &HashSet::new()).is_empty()
        || !question_frames(question).is_empty()
}

/// Up to [`MAX_LEADS`] similar past errors and their fixes for the error `question` carries, from the `sentry:` and
/// `github:` episodes `who` may read in `session`'s scope and in `also`'s. None when it carries none, when nothing
/// is alike, or (said in the log) when the episodes couldn't be read in time.
pub async fn leads(
    session: &Session,
    store: &Store,
    who: &Principal,
    question: &str,
    also: &[Session],
) -> Vec<Lead> {
    if !asks_about_error(question) {
        return vec![];
    }
    let started = Instant::now();
    let mut scopes = vec![(None, session)];
    scopes.extend(also.iter().map(|s| (Some(s.scope().to_string()), s)));
    // Each scope at once: a first read of one waits on its own.
    let reads = futures::future::join_all(
        scopes
            .into_iter()
            .map(|(tag, s)| async move { (tag, s, read_scope(s, store, who).await) }),
    )
    .await;
    let mut read: Vec<(Option<String>, Vec<Arc<Parsed>>)> = vec![];
    for (tag, s, got) in reads {
        match got {
            Ok(p) => read.push((tag, p)),
            Err(e) => {
                eprintln!(
                    "error leads: {} (read by {}) not read: {e:#}",
                    s.scope(),
                    session.scope()
                );
            }
        }
    }
    let corpus = Corpus::build(&read);
    let Some(query) = corpus.query(question) else {
        return vec![];
    };
    let found = corpus.similar(&query);
    eprintln!(
        "error leads: {} for {} past errors and {} fixes in {} ms",
        found.len(),
        corpus.leads.len(),
        corpus.fixes,
        started.elapsed().as_millis()
    );
    found
}

/// The leads as `get_advice` adds them to its text.
pub fn render(leads: &[Lead]) -> String {
    if leads.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "## Similar past errors and how they were fixed\nLeads, not answers: check whether the same cause and fix apply here.\n",
    );
    for (n, l) in leads.iter().enumerate() {
        let name = match (&l.issue.short_id, &l.issue.title) {
            (Some(s), Some(t)) => format!("{s}: {t}"),
            (Some(s), None) => s.clone(),
            (None, Some(t)) => t.clone(),
            (None, None) => "A past error".into(),
        };
        out += &format!("\n{}. {name}", n + 1);
        if let Some(link) = &l.issue.link {
            out += &format!(" ({link})");
        }
        if let Some(s) = &l.scope {
            out += &format!(" [{s}]");
        }
        out += "\n";
        if !l.why.is_empty() {
            out += &format!("   Why: {}\n", l.why.join("; "));
        }
        for f in &l.fixes {
            out += &format!("   Fixed by {} \"{}\"", f.pr, f.title);
            if let Some(d) = &f.merged {
                out += &format!(" (merged {d})");
            }
            out += &format!(": {}\n", f.link);
            if !f.files.is_empty() {
                out += &format!("   Files: {}", f.files.join(", "));
                if f.more_files > 0 {
                    out += &format!(" (and {} more)", f.more_files);
                }
                out += "\n";
            }
            if !f.description.is_empty() {
                out += &format!("   {}\n", f.description);
            }
        }
    }
    out
}

// --- Reading the episodes ----------------------------------------------------------------------------------------

/// Whether a `github:` source is a merged PR's own episode: `github:https://github.com/<repo>/pull/<n>` (the eng
/// corpus) or `github:<repo>#<n>` (the PR ingest), not a review thread's or a review's.
fn pr_source(source: &str) -> bool {
    let Some(rest) = source.strip_prefix("github:") else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(url) = rest.strip_prefix("https://github.com/") {
        let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
        return parts.len() == 4 && parts[2] == "pull" && digits(parts[3]);
    }
    rest.split_once('#')
        .is_some_and(|(repo, n)| repo.contains('/') && !repo.contains(':') && digits(n))
}

/// The `sentry:` and PR episodes `who` may read in `session`, read: from [`PARSED`] where they are, the rest read
/// now (the wait at most [`READ_WAIT`]).
async fn read_scope(
    session: &Session,
    store: &Store,
    who: &Principal,
) -> anyhow::Result<Vec<Arc<Parsed>>> {
    let ids: Vec<(String, String, i64)> = session
        .sourced(store, who, &["sentry:", "github:"])
        .await?
        .into_iter()
        .filter(|(_, source, _)| source.starts_with("sentry:") || pr_source(source))
        .collect();
    let deadline = Instant::now() + READ_WAIT;
    let mut started = false;
    loop {
        // Each episode: read, being read, or neither (not asked for yet, or unreadable when it was).
        let (ready, pending, missing) = {
            let parsed = PARSED.lock().unwrap();
            let reading = READING.lock().unwrap();
            let (mut ready, mut pending, mut missing) = (vec![], 0, vec![]);
            for (id, source, at) in &ids {
                match parsed.get(id) {
                    Some(p) => ready.push(p.clone()),
                    None if reading.contains(id) => pending += 1,
                    None => missing.push((id.clone(), source.clone(), *at)),
                }
            }
            (ready, pending, missing)
        };
        if !started && !missing.is_empty() {
            started = true;
            let fresh: Vec<(String, String, i64)> = {
                let mut reading = READING.lock().unwrap();
                missing
                    .into_iter()
                    .filter(|(id, _, _)| reading.insert(id.clone()))
                    .collect()
            };
            if !fresh.is_empty() {
                let (store, scope) = (store.clone(), session.scope().clone());
                tokio::spawn(async move { read_bodies(store, scope, fresh).await });
            }
            continue;
        }
        // What this call asked to read and still isn't (its body unreadable now) is left out; the next call asks
        // again.
        if pending == 0 {
            return Ok(ready);
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "{pending} of its episodes aren't read yet ({READ_WAIT:?}); the reading goes on for the next call"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Reads and parses these episodes into [`PARSED`], a batch at a time.
async fn read_bodies(store: Store, scope: crate::Scope, episodes: Vec<(String, String, i64)>) {
    for batch in episodes.chunks(READ_BATCH) {
        let ids: Vec<String> = batch.iter().map(|(id, _, _)| id.clone()).collect();
        // A body not read (the read failed) is left unread: the next call that wants it asks again.
        let bodies = match store.episode_bodies(&scope, ids).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("error leads: reading {scope}'s episodes failed: {e:#}");
                HashMap::new()
            }
        };
        let parsed: Vec<(String, Arc<Parsed>)> = batch
            .iter()
            .filter_map(|(id, source, at)| {
                let body = bodies.get(id)?;
                let p = parse(source, &String::from_utf8_lossy(body), *at);
                Some((id.clone(), Arc::new(p)))
            })
            .collect();
        {
            let mut cache = PARSED.lock().unwrap();
            if cache.len() + parsed.len() > PARSED_MAX {
                cache.clear();
            }
            cache.extend(parsed);
        }
        let mut reading = READING.lock().unwrap();
        for (id, _, _) in batch {
            reading.remove(id);
        }
    }
}

/// An episode, read as a Sentry issue or a PR that names one.
fn parse(source: &str, body: &str, occurred_at: i64) -> Parsed {
    let body = unchunk(body);
    if source.starts_with("sentry:") {
        return parse_issue(&body).map_or(Parsed::Neither, |i| Parsed::Issue(Arc::new(i)));
    }
    parse_pr(source, &body, occurred_at).map_or(Parsed::Neither, |f| Parsed::Pr(Arc::new(f)))
}

/// The text as written before the corpus format split it: the title line, a blank line, then the body's chunks
/// joined by newlines, each repeating the last one's final [`OVERLAP`] characters. Each seam is checked; text that
/// doesn't read so is kept as it is.
fn unchunk(text: &str) -> String {
    let Some((title, body)) = text.split_once("\n\n") else {
        return text.to_string();
    };
    let chars: Vec<char> = body.chars().collect();
    if chars.len() <= CHUNK {
        return text.to_string();
    }
    let mut out: Vec<char> = chars[..CHUNK].to_vec();
    let mut at = CHUNK;
    while at < chars.len() {
        // A seam: a newline, then a chunk starting with the last one's final OVERLAP characters.
        if chars[at] != '\n' || chars.len() < at + 1 + OVERLAP {
            return text.to_string();
        }
        let next = &chars[at + 1..(at + 1 + CHUNK).min(chars.len())];
        let tail = &out[out.len() - OVERLAP..];
        if next[..OVERLAP] != *tail {
            return text.to_string();
        }
        out.extend_from_slice(&next[OVERLAP..]);
        at += 1 + next.len();
    }
    format!("{title}\n\n{}", out.into_iter().collect::<String>())
}

/// A `sentry:` episode in the feed's format.
fn parse_issue(text: &str) -> Option<Issue> {
    let mut lines = text.lines();
    let head = lines.next()?.trim();
    let head = head.strip_prefix("# ")?;
    let (title, project) = match head.rfind(" (sentry ") {
        Some(i) => {
            let project = head[i + " (sentry ".len()..]
                .split([',', ')'])
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            (head[..i].trim().to_string(), project)
        }
        None => (head.to_string(), String::new()),
    };
    let mut error = Error {
        title: title.clone(),
        types: title_type(&title).into_iter().collect(),
        ..Default::default()
    };
    let mut fixes: Vec<Fix> = vec![];
    let mut section = "";
    let mut fix: Option<Fix> = None;
    let mut about: Vec<&str> = vec![];
    fn finish(fix: &mut Option<Fix>, about: &mut Vec<&str>, fixes: &mut Vec<Fix>) {
        if let Some(mut f) = fix.take() {
            f.description = clip(about.join("\n").trim(), KEPT_DESCRIPTION);
            if !fixes.iter().any(|g| g.key() == f.key()) {
                fixes.push(f);
            }
        }
        about.clear();
    }
    for line in lines {
        if let Some(h) = line.strip_prefix("## ") {
            finish(&mut fix, &mut about, &mut fixes);
            section = match h.trim() {
                "Stack trace" => "stack",
                "Fixed by" => "fixed",
                _ => "other",
            };
            continue;
        }
        match section {
            "" => {
                if let Some(c) = line.strip_prefix("Culprit: ") {
                    error.culprit = c.trim().to_string();
                } else if let Some((short, link)) = line.split_once(" · ") {
                    let short = short.trim();
                    if short_id_like(short) {
                        error.short_id = Some(short.to_string());
                    }
                    let link = link.trim();
                    if let Some(id) = sentry_ids(link).into_iter().next() {
                        error.id = Some(id);
                        error.link = Some(link.to_string());
                    }
                }
            }
            "stack" => {
                if let Some(f) = feed_frame(line)
                    && error.frames.last() != Some(&f)
                {
                    error.frames.push(f);
                }
            }
            "fixed" => {
                if let Some(h) = line.strip_prefix("### ")
                    && let Some(f) = fix_heading(h)
                {
                    finish(&mut fix, &mut about, &mut fixes);
                    fix = Some(f);
                } else if let Some(f) = fix.as_mut() {
                    if let Some(files) = line.strip_prefix("Files changed: ")
                        && f.files.is_empty()
                        && about.is_empty()
                    {
                        let (list, more) = match files.rfind(" (and ") {
                            Some(i) if files.ends_with(" more)") => (
                                &files[..i],
                                files[i + " (and ".len()..files.len() - " more)".len()]
                                    .replace(',', "")
                                    .parse()
                                    .unwrap_or(0),
                            ),
                            _ => (files, 0),
                        };
                        f.files = list
                            .split(", ")
                            .map(str::trim)
                            .filter(|p| !p.is_empty())
                            .map(String::from)
                            .collect();
                        f.more = more;
                    } else {
                        about.push(line);
                    }
                }
            }
            _ => {}
        }
    }
    finish(&mut fix, &mut about, &mut fixes);
    error.short_id.as_ref()?;
    let mut prefixes = vec![];
    if let Some(s) = &error.short_id
        && let Some((p, _)) = s.rsplit_once('-')
    {
        prefixes.push(p.to_string());
    }
    if !project.is_empty() {
        let p = project.to_uppercase();
        if !prefixes.contains(&p) {
            prefixes.push(p);
        }
    }
    for f in &mut fixes {
        f.words = Arc::new(crate::search::stems(&fix_text(f)));
    }
    let words = Arc::new(crate::search::stems(&error_text(&error)));
    Some(Issue {
        error,
        prefixes,
        fixes,
        words,
    })
}

/// `<repo>#<n>: <title> (merged <day>)`, a `## Fixed by` entry's heading.
fn fix_heading(h: &str) -> Option<Fix> {
    let (pr, rest) = h.split_once(": ")?;
    let (repo, n) = pr.trim().rsplit_once('#')?;
    let number = n.parse().ok()?;
    let rest = rest.trim();
    let (title, merged) = match rest.rfind(" (merged ") {
        Some(i) if rest.ends_with(')') => (
            rest[..i].trim(),
            Some(
                rest[i + " (merged ".len()..rest.len() - 1]
                    .trim()
                    .to_string(),
            )
            .filter(|d| d != "?"),
        ),
        _ => (rest, None),
    };
    Some(Fix {
        repo: repo.trim().to_string(),
        number,
        title: title.to_string(),
        merged,
        ..Default::default()
    })
}

/// A PR episode: the eng corpus's (`PR #<n>: <title>`, the title again, the description, `Files changed:` and a
/// path a line), or the PR ingest's (`# <title> (pr <repo>#<n>, merged <day>)`, `By … · <url>`, the description,
/// `## Files changed` with `- <path> (+a −d)` lines). Only one whose title or description names something
/// Sentry-like is kept.
fn parse_pr(source: &str, text: &str, occurred_at: i64) -> Option<Fix> {
    let first = text.lines().next()?.trim();
    let mut fix = if let Some(rest) = first.strip_prefix("PR #") {
        let (n, title) = rest.split_once(':')?;
        let url = source.strip_prefix("github:https://github.com/")?;
        let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
        let repo = format!("{}/{}", parts.first()?, parts.get(1)?);
        let body = text.split_once("\n\n").map_or("", |(_, b)| b);
        let (about, files) = match body.rfind("\n\nFiles changed:\n") {
            Some(i) => (&body[..i], &body[i + "\n\nFiles changed:\n".len()..]),
            None => match body.strip_suffix("\n\nFiles changed:") {
                Some(b) => (b, ""),
                None => (body, ""),
            },
        };
        let title = title.trim().to_string();
        // The description follows the title, said again.
        let about = about
            .strip_prefix(title.as_str())
            .map(str::trim_start)
            .unwrap_or(about);
        let mut paths: Vec<String> = vec![];
        for p in files.lines().map(str::trim).filter(|p| !p.is_empty()) {
            if !paths.iter().any(|q| q == p) {
                paths.push(p.to_string());
            }
        }
        Fix {
            repo,
            number: n.trim().parse().ok()?,
            title,
            merged: day_of(occurred_at),
            files: paths,
            description: about.trim().to_string(),
            ..Default::default()
        }
    } else {
        let head = first.strip_prefix("# ")?;
        let i = head.rfind(" (pr ")?;
        let (title, tail) = (&head[..i], &head[i + " (pr ".len()..]);
        let (pr, merged) = tail.split_once(", merged ")?;
        let (repo, n) = pr.rsplit_once('#')?;
        let merged = merged.trim_end_matches(')').trim().to_string();
        let mut about: Vec<&str> = vec![];
        let mut paths: Vec<String> = vec![];
        let mut more = 0;
        let mut section = "about";
        for line in text.lines().skip(1) {
            if let Some(h) = line.strip_prefix("## ") {
                section = if h.trim() == "Files changed" {
                    "files"
                } else {
                    "other"
                };
                continue;
            }
            match section {
                "about" if !line.starts_with("By ") || !about.is_empty() => about.push(line),
                "files" => {
                    if let Some(p) = line.strip_prefix("- ") {
                        if let Some(n) = p
                            .strip_prefix("… and ")
                            .and_then(|r| r.strip_suffix(" more"))
                        {
                            more = n.trim().parse().unwrap_or(0);
                        } else {
                            let p = p.rsplit_once(" (+").map_or(p, |(p, _)| p).trim();
                            paths.push(p.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        Fix {
            repo: repo.trim().to_string(),
            number: n.trim().parse().ok()?,
            title: title.trim().to_string(),
            merged: Some(merged).filter(|m| !m.is_empty()),
            files: paths,
            more,
            description: about.join("\n").trim().to_string(),
            ..Default::default()
        }
    };
    let named = format!("{}\n{}", fix.title, fix.description);
    let mut refs: Vec<String> = sentry_ids(&named);
    for s in short_ids(&named) {
        if !refs.contains(&s) {
            refs.push(s);
        }
    }
    if refs.is_empty() {
        return None;
    }
    fix.refs = refs;
    fix.description = clip(&fix.description, KEPT_DESCRIPTION);
    fix.words = Arc::new(crate::search::stems(&fix_text(&fix)));
    Some(fix)
}

/// The day (UTC) of a time in microseconds.
fn day_of(micros: i64) -> Option<String> {
    (micros > 0).then(|| {
        chrono::DateTime::from_timestamp_micros(micros)
            .map(|t| t.format("%Y-%m-%d").to_string())
            .unwrap_or_default()
    })
}

/// A past error's words: its title, culprit and frames.
fn error_text(e: &Error) -> String {
    let frames: Vec<String> = e
        .frames
        .iter()
        .map(|f| format!("{} {}", f.path, f.function))
        .collect();
    format!("{}\n{}\n{}", e.title, e.culprit, frames.join("\n"))
}

/// A fix's words: its title, files and description.
fn fix_text(f: &Fix) -> String {
    format!("{}\n{}\n{}", f.title, f.files.join("\n"), f.description)
}

/// `text` at most `n` characters, cut at a space, ending in an ellipsis.
fn clip(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        return text.to_string();
    }
    let cut: String = text.chars().take(n.saturating_sub(1)).collect();
    let at = cut.rfind([' ', '\n']).filter(|&i| i > cut.len() / 2);
    format!("{}…", at.map_or(cut.as_str(), |i| &cut[..i]).trim_end())
}

// --- Reading the question ----------------------------------------------------------------------------------------

/// The sentry.io issue ids `text` links (`sentry.io/issues/<id>`, `sentry.io/organizations/<org>/issues/<id>`).
fn sentry_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for (i, _) in text.match_indices("sentry.io/") {
        let mut rest = &text[i + "sentry.io/".len()..];
        if let Some(r) = rest.strip_prefix("organizations/") {
            let Some((_, r)) = r.split_once('/') else {
                continue;
            };
            rest = r;
        }
        let Some(r) = rest.strip_prefix("issues/") else {
            continue;
        };
        let id: String = r.chars().take_while(char::is_ascii_digit).collect();
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Whether a word is shaped as a Sentry short ID: upper-case words joined by dashes (`API-SERVICE-1AB`).
fn short_id_like(w: &str) -> bool {
    let parts: Vec<&str> = w.split('-').collect();
    parts.len() >= 2
        && parts[0].starts_with(|c: char| c.is_ascii_uppercase())
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        })
}

/// The words of `text` shaped as Sentry short IDs, each once, in order: maximal runs of letters, digits and dashes.
fn short_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for w in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
        if short_id_like(w) && !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    }
    out
}

/// The exception type a Sentry title starts with (`KeyError: 'x'`), its module left out.
fn title_type(title: &str) -> Option<String> {
    let (name, _) = title.split_once(':')?;
    identifier(name).then(|| name.rsplit('.').next().unwrap_or(name).to_string())
}

/// A (dotted) identifier.
fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.split('.').all(|p| {
            p.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// Whether `name` (its module left out) reads as an exception type: an upper-case name ending as one does, or one a
/// past error has.
fn exception_name(name: &str, known: &HashSet<String>) -> bool {
    name.starts_with(|c: char| c.is_ascii_uppercase())
        && (TYPE_ENDINGS.iter().any(|e| name.ends_with(e)) || known.contains(name))
}

/// The lines of `text` that state an exception (`[module.]Type: message`, or a bare `Type` alone on its line), as
/// (type, title): the type without its module, the title as Sentry writes it.
fn exception_lines(text: &str, known: &HashSet<String>) -> Vec<(String, String)> {
    let mut out = vec![];
    for line in text.lines() {
        let line = line.trim();
        let (name, message) = match line.split_once(':') {
            Some((n, m)) => (n, Some(m.trim())),
            None => (line, None),
        };
        if !identifier(name) {
            continue;
        }
        let ty = name.rsplit('.').next().unwrap_or(name);
        if !exception_name(ty, known) {
            continue;
        }
        let title = match message {
            Some(m) if !m.is_empty() => format!("{ty}: {m}"),
            _ => ty.to_string(),
        };
        out.push((ty.to_string(), title));
    }
    out
}

/// A repo path as frames and fixes name it: the image's `/app/` and `./`, `webpack:///` and `app:///` left out;
/// none for a library's file, a bundle or anything not code.
fn repo_path(raw: &str) -> Option<String> {
    let mut p = raw.trim();
    for pre in ["webpack:///", "webpack://", "app:///", "app://"] {
        if let Some(r) = p.strip_prefix(pre) {
            p = r;
        }
    }
    p = p.strip_prefix("/app/").unwrap_or(p);
    while let Some(r) = p.strip_prefix("./") {
        p = r;
    }
    let library = [
        "site-packages/",
        "dist-packages/",
        "node_modules/",
        "/lib/python",
    ];
    if p.is_empty()
        || p.contains('<')
        || p.contains("://")
        || p.starts_with("assets/")
        || library.iter().any(|l| p.contains(l))
        || !CODE.iter().any(|e| p.ends_with(e))
    {
        return None;
    }
    Some(p.to_string())
}

/// A frame as the feed writes it: `path:line in function` (or `path in function`).
fn feed_frame(line: &str) -> Option<Frame> {
    let (at, function) = line.trim().rsplit_once(" in ")?;
    let path = strip_line(at);
    let path = repo_path(path).filter(|p| !p.starts_with('/'))?;
    Some(Frame {
        path,
        function: function.trim().to_string(),
    })
}

/// A location without its `:line` (and `:column`).
fn strip_line(at: &str) -> &str {
    let mut p = at.trim();
    for _ in 0..2 {
        if let Some((head, n)) = p.rsplit_once(':')
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
        {
            p = head;
        }
    }
    p
}

/// The stack frames `text` holds, outermost call first, its own paths as written: Python's `File "<path>", line
/// <n>, in <f>`, JavaScript's `at <f> (<path>:<n>:<m>)` (innermost first, so turned around), and the feed's and
/// Sentry's `<path>:<n> in <f>` (or `<path> in <f> at line <n>`).
fn question_frames(text: &str) -> Vec<(String, String)> {
    let (mut py, mut js, mut feed) = (vec![], vec![], vec![]);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("File \"")
            && let Some((path, after)) = rest.split_once('"')
        {
            let function = after
                .rsplit_once(", in ")
                .map_or("", |(_, f)| f.trim())
                .to_string();
            py.push((path.to_string(), function));
        } else if let Some(rest) = line.strip_prefix("at ") {
            let (function, at) = match rest.rsplit_once(" (") {
                Some((f, at)) => (
                    f.trim_start_matches("async ").trim(),
                    at.trim_end_matches(')'),
                ),
                None => ("", rest),
            };
            let path = strip_line(at);
            if CODE.iter().any(|e| path.ends_with(e)) {
                js.push((path.to_string(), function.to_string()));
            }
        } else if let Some((at, function)) = line.rsplit_once(" in ") {
            let function = function
                .split_once(" at line ")
                .map_or(function, |(f, _)| f)
                .trim();
            let path = strip_line(at);
            if !path.contains(' ') && CODE.iter().any(|e| path.ends_with(e)) {
                feed.push((path.to_string(), function.to_string()));
            }
        }
    }
    js.reverse();
    py.into_iter().chain(js).chain(feed).collect()
}

// --- The corpus ----------------------------------------------------------------------------------------------------

/// A past error with its fixes, or a fix whose errors no episode holds: what a lead is made of.
struct Past {
    /// The error; for a fix alone, its first ref.
    error: Error,
    fixes: Vec<Fix>,
    /// Its own words and its fixes'.
    words: HashSet<String>,
    scope: Option<String>,
    /// When its newest fix was merged (`YYYY-MM-DD`; empty when unknown): a tie goes to the newer.
    newest: String,
}

/// Fixes naming only issues no episode holds, by the first they name: the scope they were read from, what they name,
/// and the fixes.
type Alone = (Option<String>, Vec<String>, Vec<Fix>);

struct Corpus {
    leads: Vec<Past>,
    /// Every issue read, by short ID and by Sentry id: the question's may be one with no fix.
    issues: HashMap<String, Arc<Issue>>,
    /// The exception types past errors have.
    types: HashSet<String>,
    /// Every path the past errors' frames and fixes name, for a question's absolute paths.
    paths: BTreeSet<String>,
    /// Distinct fixes.
    fixes: usize,
}

impl Corpus {
    fn build(read: &[(Option<String>, Vec<Arc<Parsed>>)]) -> Corpus {
        // Issues, each once (the caller's own scope first), and the PRs naming one.
        let mut issues: Vec<(Option<String>, Arc<Issue>)> = vec![];
        let mut by_ref: HashMap<String, usize> = HashMap::new();
        let mut prs: Vec<(Option<String>, Arc<Fix>)> = vec![];
        let mut seen_prs: HashSet<(String, u64)> = HashSet::new();
        for (tag, parsed) in read {
            for p in parsed {
                match p.as_ref() {
                    Parsed::Issue(i) => {
                        let keys: Vec<String> = i
                            .error
                            .short_id
                            .iter()
                            .chain(i.error.id.iter())
                            .cloned()
                            .collect();
                        if keys.iter().any(|k| by_ref.contains_key(k)) {
                            continue;
                        }
                        for k in keys {
                            by_ref.insert(k, issues.len());
                        }
                        issues.push((tag.clone(), i.clone()));
                    }
                    Parsed::Pr(f) => {
                        if seen_prs.insert(f.key()) {
                            prs.push((tag.clone(), f.clone()));
                        }
                    }
                    Parsed::Neither => {}
                }
            }
        }
        let prefixes: HashSet<&str> = issues
            .iter()
            .flat_map(|(_, i)| i.prefixes.iter().map(String::as_str))
            .collect();
        // Each issue's fixes: its `## Fixed by`, with the PRs naming it (a PR's own episode gives its files whole).
        let mut fixes_of: Vec<Vec<Fix>> = issues.iter().map(|(_, i)| i.fixes.clone()).collect();
        // A fix naming only issues no episode holds: a lead by the first it names, with what else it names.
        let mut alone: BTreeMap<String, Alone> = BTreeMap::new();
        for (tag, f) in &prs {
            let refs: Vec<&String> = f
                .refs
                .iter()
                .filter(|r| {
                    r.bytes().all(|b| b.is_ascii_digit())
                        || r.rsplit_once('-')
                            .is_some_and(|(p, _)| prefixes.contains(p))
                })
                .collect();
            if refs.is_empty() {
                continue;
            }
            let named: BTreeSet<usize> = refs
                .iter()
                .filter_map(|r| by_ref.get(*r).copied())
                .collect();
            if named.is_empty() {
                let (_, named, fixes) = alone
                    .entry(refs[0].clone())
                    .or_insert_with(|| (tag.clone(), vec![], vec![]));
                for r in &refs {
                    if !named.contains(*r) {
                        named.push((*r).clone());
                    }
                }
                fixes.push((**f).clone());
                continue;
            }
            for i in named {
                let list = &mut fixes_of[i];
                match list.iter_mut().find(|g| g.key() == f.key()) {
                    Some(g) => {
                        // The PR's own episode lists more of its files; the feed's description is kept.
                        if f.files.len() > g.files.len() {
                            g.files = f.files.clone();
                            g.more = f.more;
                        }
                        if g.description.is_empty() {
                            g.description = f.description.clone();
                        }
                        g.refs = f.refs.clone();
                        g.words = f.words.clone();
                    }
                    None => list.push((**f).clone()),
                }
            }
        }
        let mut leads = vec![];
        let mut fix_keys: HashSet<(String, u64)> = HashSet::new();
        for ((tag, i), mut fixes) in issues.iter().zip(fixes_of) {
            if fixes.is_empty() {
                continue;
            }
            fixes.sort_by(|a, b| b.merged.cmp(&a.merged));
            let mut words: HashSet<String> = (*i.words).clone();
            for f in &fixes {
                fix_keys.insert(f.key());
                words.extend(f.words.iter().cloned());
            }
            leads.push(Past {
                error: i.error.clone(),
                newest: fixes[0].merged.clone().unwrap_or_default(),
                fixes,
                words,
                scope: tag.clone(),
            });
        }
        for (tag, named, mut fixes) in alone.into_values() {
            fixes.sort_by(|a, b| b.merged.cmp(&a.merged));
            let mut words = HashSet::new();
            for f in &fixes {
                fix_keys.insert(f.key());
                words.extend(f.words.iter().cloned());
            }
            let numeric = |r: &&String| r.bytes().all(|b| b.is_ascii_digit());
            let id = named.iter().find(numeric).cloned();
            let error = Error {
                link: id
                    .as_ref()
                    .and_then(|id| fixes.iter().find_map(|f| sentry_link(&f.description, id))),
                id,
                short_id: named.iter().find(|r| !numeric(r)).cloned(),
                ..Default::default()
            };
            leads.push(Past {
                error,
                newest: fixes[0].merged.clone().unwrap_or_default(),
                fixes,
                words,
                scope: tag,
            });
        }
        let mut types = HashSet::new();
        let mut paths = BTreeSet::new();
        let mut all: HashMap<String, Arc<Issue>> = HashMap::new();
        for (_, i) in &issues {
            types.extend(i.error.types.iter().cloned());
            paths.extend(i.error.frames.iter().map(|f| f.path.clone()));
            for k in i.error.short_id.iter().chain(i.error.id.iter()) {
                all.insert(k.clone(), i.clone());
            }
        }
        for l in &leads {
            for f in &l.fixes {
                paths.extend(f.files.iter().cloned());
            }
        }
        Corpus {
            leads,
            issues: all,
            types,
            paths,
            fixes: fix_keys.len(),
        }
    }

    /// The error `question` carries: the issue it names (by short ID or link) where one is read, else the
    /// exception and frames it states.
    fn query(&self, question: &str) -> Option<Error> {
        for r in sentry_ids(question).into_iter().chain(short_ids(question)) {
            if let Some(i) = self.issues.get(&r) {
                return Some(i.error.clone());
            }
        }
        let stated = exception_lines(question, &self.types);
        let frames: Vec<Frame> = question_frames(question)
            .into_iter()
            .filter_map(|(path, function)| {
                let path = self.resolve(&path)?;
                Some(Frame { path, function })
            })
            .fold(vec![], |mut v, f| {
                if v.last() != Some(&f) {
                    v.push(f);
                }
                v
            });
        if stated.is_empty() && frames.is_empty() {
            return None;
        }
        let mut types: Vec<String> = vec![];
        for (t, _) in &stated {
            if !types.contains(t) {
                types.push(t.clone());
            }
        }
        // As Sentry names an error: by the exception raised last, and the innermost in-app frame's module.
        let title = stated.last().map(|(_, t)| t.clone()).unwrap_or_default();
        let culprit = question
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("Culprit: ")
                    .map(|c| c.trim().to_string())
            })
            .or_else(|| {
                let f = frames.last()?;
                let module = f.path.strip_suffix(".py")?.replace('/', ".");
                Some(format!("{module} in {}", f.function))
            })
            .unwrap_or_default();
        Some(Error {
            title,
            culprit,
            types,
            frames,
            ..Default::default()
        })
    }

    /// A question's path as the repo's: as written where it is one, else (an absolute path) the longest path the past
    /// errors or fixes name that it ends with.
    fn resolve(&self, raw: &str) -> Option<String> {
        let p = repo_path(raw)?;
        if !p.starts_with('/') {
            return Some(p);
        }
        self.paths
            .iter()
            .filter(|q| p.ends_with(&format!("/{q}")))
            .max_by_key(|q| q.len())
            .cloned()
    }

    /// Files so many past errors' stacks or fixes run through (middleware, decorators, clients) that sharing one
    /// says little.
    fn hubs(&self) -> HashSet<String> {
        let mut stacks: HashMap<&str, usize> = HashMap::new();
        let mut fixed: HashMap<&str, usize> = HashMap::new();
        let mut seen: HashSet<(String, u64)> = HashSet::new();
        let mut with_stacks = 0;
        for l in &self.leads {
            let paths: HashSet<&str> = l.error.frames.iter().map(|f| f.path.as_str()).collect();
            if !paths.is_empty() {
                with_stacks += 1;
            }
            for p in paths {
                *stacks.entry(p).or_default() += 1;
            }
            for f in &l.fixes {
                if seen.insert(f.key()) {
                    for p in f.files.iter().collect::<HashSet<_>>() {
                        *fixed.entry(p.as_str()).or_default() += 1;
                    }
                }
            }
        }
        let (s, f) = ((with_stacks / 4).max(3), (self.fixes / 10).max(3));
        stacks
            .into_iter()
            .filter(|(_, n)| *n > s)
            .chain(fixed.into_iter().filter(|(_, n)| *n > f))
            .map(|(p, _)| p.to_string())
            .collect()
    }

    /// The best [`MAX_LEADS`] leads for `q`: the deterministic ranking and the words' fused.
    fn similar(&self, q: &Error) -> Vec<Lead> {
        let own = |e: &Error| {
            (e.id.is_some() && e.id == q.id) || (e.short_id.is_some() && e.short_id == q.short_id)
        };
        let names_q = |f: &Fix| {
            f.refs
                .iter()
                .any(|r| Some(r) == q.id.as_ref() || Some(r) == q.short_id.as_ref())
        };
        // Each candidate with its fixes that aren't the question's own.
        let candidates: Vec<(usize, Vec<&Fix>)> = self
            .leads
            .iter()
            .enumerate()
            .filter(|(_, l)| !own(&l.error))
            .map(|(i, l)| {
                (
                    i,
                    l.fixes.iter().filter(|f| !names_q(f)).collect::<Vec<_>>(),
                )
            })
            .filter(|(_, fixes)| !fixes.is_empty())
            .collect();
        let hubs = self.hubs();
        // Deterministic.
        let frames: Vec<&Frame> = q
            .frames
            .iter()
            .filter(|f| !hubs.contains(&f.path))
            .collect();
        let mut det: Vec<(usize, f32, Vec<String>)> = vec![];
        for (i, fixes) in &candidates {
            let l = &self.leads[*i];
            let (mut score, mut why) = issue_match(q, &l.error, &hubs);
            let best = fixes
                .iter()
                .map(|f| {
                    let touched: Vec<usize> = frames
                        .iter()
                        .enumerate()
                        .filter(|(_, fr)| f.files.contains(&fr.path))
                        .map(|(j, _)| j)
                        .collect();
                    let s: f32 = touched
                        .iter()
                        .map(|&j| if j + 1 == frames.len() { 2.0 } else { 1.0 })
                        .sum();
                    (s, touched)
                })
                .max_by(|a, b| a.0.total_cmp(&b.0))
                .unwrap_or_default();
            if best.0 > 0.0 {
                score += best.0;
                let paths: BTreeSet<&str> =
                    best.1.iter().map(|&j| frames[j].path.as_str()).collect();
                why.push(format!(
                    "the fix changed {}",
                    paths
                        .iter()
                        .take(2)
                        .map(|p| format!("`{p}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !why.is_empty() {
                det.push((*i, score, why));
            }
        }
        det.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| self.leads[b.0].newest.cmp(&self.leads[a.0].newest))
        });
        det.truncate(MAX_LEADS);
        // By words.
        let words = self.searched(q, &candidates);
        // Fused.
        let mut fused: Vec<(usize, f32, Vec<String>)> = vec![];
        for list in [&det, &words] {
            for (rank, (i, _, why)) in list.iter().enumerate() {
                let at = match fused.iter().position(|(j, _, _)| j == i) {
                    Some(at) => at,
                    None => {
                        fused.push((*i, 0.0, vec![]));
                        fused.len() - 1
                    }
                };
                fused[at].1 += 1.0 / (RRF_K + rank as f32 + 1.0);
                for w in why {
                    if !fused[at].2.contains(w) {
                        fused[at].2.push(w.clone());
                    }
                }
            }
        }
        // Stable: a tie keeps the order first seen, the deterministic ranking's first.
        fused.sort_by(|a, b| b.1.total_cmp(&a.1));
        fused.truncate(MAX_LEADS);
        fused
            .into_iter()
            .map(|(i, _, why)| {
                let fixes = &candidates
                    .iter()
                    .find(|(j, _)| *j == i)
                    .expect("a candidate")
                    .1;
                self.lead(i, fixes, q, why)
            })
            .collect()
    }

    /// The candidates whose words (theirs and their fixes') best match the error's title, culprit and nearest three
    /// frames, by BM25: the best [`MAX_LEADS`] with any.
    fn searched(
        &self,
        q: &Error,
        candidates: &[(usize, Vec<&Fix>)],
    ) -> Vec<(usize, f32, Vec<String>)> {
        let n = candidates.len();
        if n == 0 {
            return vec![];
        }
        let text = query_text(q);
        let words: Vec<String> = crate::search::terms(&text)
            .into_iter()
            .filter(|t| crate::search::informative(t))
            .collect();
        let terms: BTreeSet<String> = crate::search::stems(&words.join(" ")).into_iter().collect();
        // Each stem by the question's word for it (the first in order), to say why in its words.
        let mut said: BTreeMap<String, String> = BTreeMap::new();
        let mut sorted = words.clone();
        sorted.sort();
        for w in sorted {
            for t in crate::search::stems(&w) {
                said.entry(t).or_insert_with(|| w.clone());
            }
        }
        let docs: Vec<&HashSet<String>> = candidates
            .iter()
            .map(|(i, _)| &self.leads[*i].words)
            .collect();
        let avg = (docs.iter().map(|d| d.len()).sum::<usize>() as f32 / n as f32).max(1.0);
        let idf: Vec<(&String, f32)> = terms
            .iter()
            .map(|t| {
                let df = docs.iter().filter(|d| d.contains(t)).count() as f32;
                (t, (1.0 + (n as f32 - df + 0.5) / (df + 0.5)).ln())
            })
            .filter(|(_, idf)| *idf > 0.0)
            .collect();
        let mut scored: Vec<(usize, f32, Vec<String>)> = candidates
            .iter()
            .zip(&docs)
            .filter_map(|((i, _), d)| {
                let norm = K1 * (1.0 - B + B * d.len() as f32 / avg);
                let mut hit: Vec<(&String, f32)> = idf
                    .iter()
                    .filter(|(t, _)| d.contains(*t))
                    .map(|(t, f)| (*t, *f))
                    .collect();
                if hit.is_empty() {
                    return None;
                }
                let score: f32 = hit.iter().map(|(_, f)| f * (K1 + 1.0) / (1.0 + norm)).sum();
                hit.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
                let named: Vec<String> = hit
                    .iter()
                    .take(3)
                    .map(|(t, _)| format!("`{}`", said.get(*t).unwrap_or(t)))
                    .collect();
                Some((
                    *i,
                    score,
                    vec![format!("its words match the error's: {}", named.join(", "))],
                ))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| self.leads[b.0].newest.cmp(&self.leads[a.0].newest))
        });
        scored.truncate(MAX_LEADS);
        scored
    }

    /// Candidate `i` as a lead for `q`, with these fixes.
    fn lead(&self, i: usize, fixes: &[&Fix], q: &Error, why: Vec<String>) -> Lead {
        let l = &self.leads[i];
        let frame_files: Vec<&str> = q.frames.iter().rev().map(|f| f.path.as_str()).collect();
        Lead {
            issue: LeadIssue {
                short_id: l.error.short_id.clone(),
                link: l.error.link.clone(),
                title: Some(l.error.title.clone()).filter(|t| !t.is_empty()),
            },
            why,
            fixes: fixes
                .iter()
                .take(LEAD_FIXES)
                .map(|f| {
                    // The files of the question's frames first, then source files, then the rest.
                    let mut files: Vec<&String> = f.files.iter().collect();
                    files.sort_by_key(|p| {
                        (
                            frame_files
                                .iter()
                                .position(|q| q == p)
                                .unwrap_or(usize::MAX),
                            !is_source(p),
                        )
                    });
                    let shown: Vec<String> = files
                        .iter()
                        .take(LEAD_FILES)
                        .map(|p| p.to_string())
                        .collect();
                    LeadFix {
                        pr: format!("{}#{}", f.repo, f.number),
                        link: format!("https://github.com/{}/pull/{}", f.repo, f.number),
                        title: f.title.clone(),
                        merged: f.merged.clone(),
                        more_files: f.files.len() - shown.len() + f.more,
                        files: shown,
                        description: short_description(&f.description),
                    }
                })
                .collect(),
            scope: l.scope.clone(),
        }
    }
}

/// How alike two errors are: the same exception type and culprit, the same message (ids and numbers aside),
/// shared in-app frames or files, the same innermost frame. No reason, no match.
fn issue_match(q: &Error, past: &Error, hubs: &HashSet<String>) -> (f32, Vec<String>) {
    let mut score = 0.0;
    let mut why = vec![];
    let same: Vec<&String> = q.types.iter().filter(|t| past.types.contains(t)).collect();
    if !same.is_empty() && !q.culprit.is_empty() && q.culprit == past.culprit {
        score += 3.0;
        why.push("same exception and culprit".to_string());
    } else if same.iter().any(|t| !GENERIC_TYPES.contains(&t.as_str())) {
        score += 0.5;
    }
    if !q.title.is_empty() && normalized_title(&q.title) == normalized_title(&past.title) {
        score += 2.0;
        why.push("same error message".to_string());
    }
    let q_frames: BTreeSet<&Frame> = q
        .frames
        .iter()
        .filter(|f| !hubs.contains(&f.path))
        .collect();
    let p_frames: BTreeSet<&Frame> = past.frames.iter().collect();
    let frames: Vec<&&Frame> = q_frames.intersection(&p_frames).collect();
    let q_files: BTreeSet<&str> = q_frames.iter().map(|f| f.path.as_str()).collect();
    let p_files: BTreeSet<&str> = past.frames.iter().map(|f| f.path.as_str()).collect();
    let files: Vec<&&str> = q_files.intersection(&p_files).collect();
    if !frames.is_empty() {
        score += frames.len() as f32;
        let mut named: Vec<String> = frames
            .iter()
            .map(|f| {
                if f.function.is_empty() {
                    format!("`{}`", f.path)
                } else {
                    format!(
                        "`{}` in `{}`",
                        f.path.rsplit('/').next().unwrap_or(&f.path),
                        f.function
                    )
                }
            })
            .collect();
        named.sort();
        why.push(format!(
            "shared frame {}",
            named[..named.len().min(2)].join(", ")
        ));
    } else if !files.is_empty() {
        score += 0.5 * files.len() as f32;
        let named: Vec<String> = files.iter().take(2).map(|p| format!("`{p}`")).collect();
        why.push(format!("shared file {}", named.join(", ")));
    }
    if let (Some(a), Some(b)) = (q.frames.last(), past.frames.last())
        && a == b
    {
        score += 1.0;
    }
    if why.is_empty() {
        (0.0, vec![])
    } else {
        (score, why)
    }
}

/// The title without what differs between two events of one bug: quoted values, ids, hashes and numbers.
fn normalized_title(title: &str) -> String {
    let s: Vec<char> = title.to_lowercase().chars().collect();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let hex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
    let mut out = String::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        let boundary = i == 0 || !word(s[i - 1]);
        if (c == '\'' || c == '"')
            && let Some(j) = s[i + 1..].iter().position(|&d| d == c)
        {
            out.push(' ');
            i += j + 2;
            continue;
        }
        if boundary && hex(c) {
            // A UUID: 8 hex, a dash, then 27 of hex and dashes, at word boundaries.
            if s.len() >= i + 36
                && s[i..i + 8].iter().all(|&d| hex(d))
                && s[i + 8] == '-'
                && s[i + 9..i + 36].iter().all(|&d| hex(d) || d == '-')
                && s.get(i + 36).is_none_or(|&d| !word(d))
            {
                out.push(' ');
                i += 36;
                continue;
            }
            let run = s[i..].iter().take_while(|&&d| hex(d)).count();
            if run >= 12 && s.get(i + run).is_none_or(|&d| !word(d)) {
                out.push(' ');
                i += run;
                continue;
            }
        }
        if c.is_ascii_digit() {
            out.push(' ');
            i += s[i..].iter().take_while(|d| d.is_ascii_digit()).count();
            continue;
        }
        out.push(c);
        i += 1;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The search text for an error: its title, culprit and nearest frames.
fn query_text(e: &Error) -> String {
    let start = e.frames.len().saturating_sub(3);
    let frames: Vec<String> = e.frames[start..]
        .iter()
        .map(|f| format!("{} {}", f.path, f.function))
        .collect();
    let frames = frames.join(" ");
    let joined: Vec<&str> = [e.title.as_str(), e.culprit.as_str(), frames.as_str()]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect();
    joined.join(" | ").chars().take(QUERY_CHARS).collect()
}

/// Code that isn't a test, a generated file or a migration.
fn is_source(p: &str) -> bool {
    let name = p.rsplit('/').next().unwrap_or(p);
    CODE.iter().any(|e| p.ends_with(e))
        && !(name.starts_with("test_")
            || name.ends_with("_test.py")
            || name.contains(".test.")
            || name == "conftest.py"
            || p.ends_with(".d.ts")
            || p.split('/')
                .any(|d| d.starts_with("generated") || d == "migrations"))
}

/// A sentry.io link to issue `id` that `text` holds.
fn sentry_link(text: &str, id: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || "()<>[]\"'".contains(c))
        .find(|w| w.contains("sentry.io/") && sentry_ids(w).iter().any(|i| i == id))
        .map(|w| w.trim_end_matches(['.', ',', ';']).to_string())
}

/// A description's start as a lead shows it: no headings, comments or blank lines, at most [`LEAD_DESCRIPTION`]
/// characters.
fn short_description(text: &str) -> String {
    let mut out = String::new();
    let mut in_comment = false;
    for line in text.lines() {
        let line = line.trim();
        if in_comment {
            in_comment = !line.contains("-->");
            continue;
        }
        if line.starts_with("<!--") {
            in_comment = !line.contains("-->");
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out += line;
        if out.chars().count() > LEAD_DESCRIPTION {
            break;
        }
    }
    clip(&out, LEAD_DESCRIPTION)
}
