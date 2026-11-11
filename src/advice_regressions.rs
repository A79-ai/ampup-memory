//! Review checks from the regressions a scope's merged pull requests fixed (`POST /advice {files, from:
//! "regressions"}`): for a change, what earlier changes like it broke, as conditional checks a reviewer can test the
//! diff against.
//!
//! [`crate::advice_files`] condenses the compiled skills whose sources changed the same files. A fix is a sharper
//! source: it records what had to stay true and how it broke and, when it names one (`Regression from #N`), the
//! change that broke it. In a replay of one repository's regressions, a reviewer that was handed the rule an
//! earlier pull request recorded pointed at the code the later fix rewrote in 5 of 6 runs, against 0 of 3 without
//! it; the same facts served as dozens of loose search hits, with nothing saying to check them against the change,
//! were ignored. A check says when it applies and what to verify, and names where it broke.
//!
//! How a change's checks are made:
//! 1. **Regressions.** Each merged pull request the caller may read, merged before `as_of` (µs; now when absent),
//!    that fixed something: its title starts `fix`, `bug`, `hotfix` or `revert` (after optional `[TICKET]`s), or a
//!    line of its title or description opens with `Regression from #N` (after at most a list marker and emphasis,
//!    outside code blocks; the convention quoted mid-sentence is no claim). A pull request is its episode as the PR
//!    ingest writes it (`# <title> (pr <repo>#<n>, merged <day>)`, the description, `## Files changed`) or as the
//!    engineering corpus does (`PR #<n>: <title>`, the title again, the description, `Files changed:`), dated at
//!    its merge. Each fix is one regression: its area is the fix's files and those of each change it names that
//!    the caller may read; its words are the fix's title, description and paths and the named changes' titles. The
//!    change itself (`pr`) and a fix naming it are never one.
//! 2. **Matching.** Three rankings, fused by reciprocal rank (k [`RRF_K`]):
//!    - files: each changed file the area shares counts 1/log2(2 + n), n the pull requests that changed it, half
//!      for a file only the breaking change touched; a file more than [`HUB_SHARE`] of them changed (generated
//!      clients, shared config) counts for nothing;
//!    - words: BM25 of the change's title, description start and file paths against the regression's words, both
//!      read as the full-text index reads them, identifiers split (`DraftEmailCard` is `draft email card`);
//!    - meaning: the cosine of the change's title and description start against the fix's, embedded with the
//!      store's embedding model ([`crate::search::embed`]). A fix often shares no file with the change it warns
//!      about: the same feature lives in a new component, or the same mistake is made elsewhere.
//!
//!    Each score is weighted by the fix's age at `as_of`: 1 when new, 3/4 at [`HALF_LIFE_DAYS`], toward 1/2 for
//!    old ones. The best [`CANDIDATES`] go on.
//! 3. **Phrasing.** The routing model (Claude Haiku 5.5 by default, [`Extractor::route_from_env`]) reads the change
//!    (title, description start, files: never the diff) and the candidates (each fix's title, files, description
//!    start and the titles of the changes it names) and writes one check for each regression this change could
//!    break again, at most `top` (5 by default): `If this change …, verify …`. The source pull requests are added
//!    to each check from the regression, never from the model (`; it broke before in #N (fixed in #M).`), and the
//!    checks keep the ranking's order. A reply naming no candidate serves nothing.
//!
//! Reading: the first answer in a process reads every pull request episode of the scope, [`READ_BATCH`] at a time,
//! in the background; an answer waits [`READ_WAIT`] for them and is otherwise refused (503) while the reading goes
//! on for the next one. The fixes' texts are embedded the same way, through the embedding cache
//! ([`crate::search::embed_cache`]); an answer waits [`EMBED_WAIT`] for them and is otherwise ranked by files and
//! words alone (`meaning: false`). Each is kept by episode id (an id names its text) and served only to a caller
//! the session lets see that id. The condensing replies are kept as [`crate::advice_files`] keeps them.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;

use crate::advice_files::{Asked, Issue};
use crate::extract::Extractor;
use crate::{Principal, Session, Store};

/// Checks served at most, by default.
pub const TOP: usize = 5;
/// Regressions the model reads for one change.
pub const CANDIDATES: usize = 8;
/// Reciprocal rank fusion's constant.
const RRF_K: f64 = 10.0;
/// BM25's parameters, as the full-text index has them.
const K1: f64 = 1.2;
const B: f64 = 0.75;
/// A file more than this share of the pull requests read changed counts for nothing (at least [`HUB_MIN`] of them).
pub const HUB_SHARE: f64 = 0.02;
const HUB_MIN: f64 = 20.0;
/// A fix this many days older than the change counts 3/4; much older ones toward 1/2.
pub const HALF_LIFE_DAYS: f64 = 180.0;
/// A description's start, in characters: as words, as meaning, and as the model reads each fix.
const WORD_CHARS: usize = 1_500;
const MEANING_CHARS: usize = 1_200;
const PROMPT_CHARS: usize = 1_500;
/// The change's description as the model reads it, and the most a caller's description is read.
const CHANGE_CHARS: usize = 1_500;
const DESCRIPTION_MAX: usize = 20_000;
/// The change's files the model is shown, and each fix's.
const CHANGE_FILES: usize = 40;
const FIX_FILES: usize = 12;
const SHARED_SHOWN: usize = 6;
/// A fix's description kept in memory, in characters.
const KEPT: usize = 2_000;
/// Episodes read in one go, and how long an answer waits for a scope's first read.
pub const READ_BATCH: usize = 500;
pub const READ_WAIT: Duration = Duration::from_secs(15);
/// Texts embedded in one go, and how long an answer waits for the fixes' vectors.
const EMBED_BATCH: usize = 256;
pub const EMBED_WAIT: Duration = Duration::from_secs(5);
/// A day, in microseconds.
const DAY_US: f64 = 86_400e6;

/// The scope's pull requests aren't read yet: the caller may ask again shortly (503).
#[derive(Debug)]
pub struct NotReady(pub String);
impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for NotReady {}

/// The checks for one change.
#[derive(Serialize)]
pub struct Checks {
    /// The checks, each `- If this change …, verify …; it broke before in #N (fixed in #M).`, best first.
    pub checks: Vec<String>,
    /// The regressions the model read, best first.
    pub regressions: Vec<Found>,
    /// Whether meaning ranked them (the fixes' vectors were ready and the change's was embedded).
    pub meaning: bool,
    /// Always empty here: the shape `{files}` answers with.
    pub skills: Vec<String>,
    /// The linked issues the caller may read, in the order asked.
    pub issues: Vec<Issue>,
}

#[derive(Serialize)]
pub struct Found {
    /// The fix, `owner/repo#n`.
    pub fix: String,
    pub title: String,
    /// The day it merged.
    pub merged: String,
    /// The changes it says broke what it fixed, `owner/repo#n`.
    pub introduced_by: Vec<String>,
    pub score: f64,
    /// Its rank by files, words and meaning, where it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_words: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_meaning: Option<usize>,
    /// The change's files its area shares.
    pub shared: Vec<String>,
    /// The check written from it: its index in `checks`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<usize>,
}

/// A merged pull request, as its episode tells it.
struct Pr {
    repo: String,
    number: u64,
    title: String,
    /// Its description, as words and meaning read it (markup, links and HTML comments left out); a fix's only.
    about: String,
    files: Vec<String>,
    /// The earlier pull requests it says it fixed a regression from, in order.
    claims: Vec<u64>,
    /// Whether it fixed something: its title says so, or it claims a regression.
    fix: bool,
    /// A fix's words (its title, description start and paths), as the index reads them.
    words: HashSet<String>,
}

/// Each episode read, by id (its text's hash, so it never changes): the pull request, or none.
static READ: LazyLock<Mutex<HashMap<String, Option<Arc<Pr>>>>> = LazyLock::new(Default::default);
/// The episodes being read now.
static READING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
/// Each fix's vector, by episode id (unit length), and those being embedded now.
static VECTORS: LazyLock<Mutex<HashMap<String, Arc<Vec<f32>>>>> = LazyLock::new(Default::default);
static EMBEDDING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
/// Parsed episodes kept; cleared past this.
const READ_MAX: usize = 400_000;

// --- Reading the pull requests -------------------------------------------------------------------------------------

/// The pull requests `who` may read in `session`, each with its episode id and merge time: from [`READ`] where they
/// are, the rest read now (the wait at most [`READ_WAIT`]).
async fn read_scope(
    session: &Session,
    store: &Store,
    who: &Principal,
) -> Result<Vec<(String, i64, Arc<Pr>)>> {
    let vis = session.vis();
    let ids: Vec<(String, String, i64)> = session
        .sourced(store, who, &["github:"])
        .await?
        .into_iter()
        .filter(|(id, source, _)| {
            crate::error_leads::pr_source(source) && vis.is_current(id, who, None)
        })
        .collect();
    let deadline = Instant::now() + READ_WAIT;
    let mut started = false;
    loop {
        let (ready, pending, missing) = {
            let read = READ.lock().unwrap();
            let reading = READING.lock().unwrap();
            let (mut ready, mut pending, mut missing) = (vec![], 0, vec![]);
            for (id, source, at) in &ids {
                match read.get(id) {
                    Some(Some(p)) => ready.push((id.clone(), *at, p.clone())),
                    Some(None) => {}
                    None if reading.contains(id) => pending += 1,
                    None => missing.push((id.clone(), source.clone())),
                }
            }
            (ready, pending, missing)
        };
        if !started && !missing.is_empty() {
            started = true;
            let fresh: Vec<(String, String)> = {
                let mut reading = READING.lock().unwrap();
                missing
                    .into_iter()
                    .filter(|(id, _)| reading.insert(id.clone()))
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
            return Err(NotReady(format!(
                "{pending} of this scope's pull requests aren't read yet ({READ_WAIT:?}): ask again shortly"
            ))
            .into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Reads and parses these episodes into [`READ`], a batch at a time.
async fn read_bodies(store: Store, scope: crate::Scope, episodes: Vec<(String, String)>) {
    for batch in episodes.chunks(READ_BATCH) {
        let ids: Vec<String> = batch.iter().map(|(id, _)| id.clone()).collect();
        // A body not read (the read failed) is left unread: the next call that wants it asks again.
        let bodies = match store.episode_bodies(&scope, ids).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("regression checks: reading {scope}'s episodes failed: {e:#}");
                HashMap::new()
            }
        };
        let parsed: Vec<(String, Option<Arc<Pr>>)> = batch
            .iter()
            .filter_map(|(id, source)| {
                let body = bodies.get(id)?;
                let pr = parse(source, &String::from_utf8_lossy(body));
                Some((id.clone(), pr.map(Arc::new)))
            })
            .collect();
        {
            let mut read = READ.lock().unwrap();
            if read.len() + parsed.len() > READ_MAX {
                read.clear();
            }
            read.extend(parsed);
        }
        let mut reading = READING.lock().unwrap();
        for (id, _) in batch {
            reading.remove(id);
        }
    }
}

/// A pull request episode in either format (see the module's header); none for anything else.
fn parse(source: &str, body: &str) -> Option<Pr> {
    let text = crate::error_leads::unchunk(body);
    let first = text.lines().next()?.trim();
    let (repo, number, title, about, files) = if let Some(rest) = first.strip_prefix("PR #") {
        // The engineering corpus: `PR #<n>: <title>`, the title again, the description, `Files changed:` and a
        // path a line.
        let (n, title) = rest.split_once(':')?;
        let url = source.strip_prefix("github:https://github.com/")?;
        let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
        let repo = format!("{}/{}", parts.first()?, parts.get(1)?);
        let body = text.split_once("\n\n").map_or("", |(_, b)| b);
        let (about, list) = match body.rfind("\n\nFiles changed:\n") {
            Some(i) => (&body[..i], &body[i + "\n\nFiles changed:\n".len()..]),
            None => (body.strip_suffix("\n\nFiles changed:").unwrap_or(body), ""),
        };
        let title = title.trim();
        let about = about.strip_prefix(title).unwrap_or(about);
        let mut files: Vec<String> = vec![];
        for p in list.lines().map(str::trim).filter(|p| !p.is_empty()) {
            if !files.iter().any(|q| q == p) {
                files.push(p.to_string());
            }
        }
        (
            repo,
            n.trim().parse().ok()?,
            title.to_string(),
            about.to_string(),
            files,
        )
    } else {
        // The PR ingest: `# <title> (pr <repo>#<n>, merged <day>)`, `By <login> · <url>`, the description, then
        // `## Files changed` (`- <path> (+a −d)`), `## Reviews` and `## Discussion`. The description may hold
        // sections of its own, so it ends at the last of the ingest's.
        let head = first.strip_prefix("# ")?;
        let i = head.rfind(" (pr ")?;
        let (title, tail) = (&head[..i], &head[i + " (pr ".len()..]);
        let (pr, _) = tail.split_once(", merged ")?;
        let (repo, n) = pr.rsplit_once('#')?;
        let rest = text.split_once('\n').map_or("", |(_, r)| r);
        let rest = match rest.split_once('\n') {
            Some((by, r)) if by.starts_with("By ") => r,
            _ if rest.starts_with("By ") => "",
            _ => rest,
        };
        let end = [
            "\n## Files changed\n",
            "\n## Reviews\n",
            "\n## Discussion\n",
        ]
        .iter()
        .filter_map(|h| rest.rfind(h))
        .min()
        .unwrap_or(rest.len());
        let mut files = vec![];
        const HEAD: &str = "\n## Files changed\n";
        if let Some(i) = rest.rfind(HEAD) {
            for line in rest[i + HEAD.len()..].lines() {
                if line.starts_with("## ") {
                    break;
                }
                // `- <path> (+a −b)`; `- … and N more` names none.
                if let Some((path, _)) = line.strip_prefix("- ").and_then(|l| l.rsplit_once(" (+"))
                {
                    files.push(path.to_string());
                }
            }
        }
        (
            repo.trim().to_string(),
            n.trim().parse().ok()?,
            title.trim().to_string(),
            rest[..end].to_string(),
            files,
        )
    };
    let claims = claims(number, &title, &about);
    let fix = crate::advice_files::fixes(&title) || !claims.is_empty();
    let (about, words) = if fix {
        let about = clean(&about);
        let words = word_set(&format!(
            "{title}\n{}\n{}",
            crate::advice_files::chars(&about, WORD_CHARS),
            files.join(" ")
        ));
        (crate::advice_files::chars(&about, KEPT).to_string(), words)
    } else {
        (String::new(), HashSet::new())
    };
    Some(Pr {
        repo,
        number,
        title,
        about,
        files,
        claims,
        fix,
        words,
    })
}

/// The earlier pull requests a fix says it fixed a regression from: each line of its title or description that
/// opens with `Regression from #N` (after at most a list marker and emphasis), outside code blocks, N below its own.
fn claims(number: u64, title: &str, about: &str) -> Vec<u64> {
    let mut found = vec![];
    let mut fenced = false;
    for line in std::iter::once(title).chain(about.lines()) {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if let Some(n) = claim(t)
            && n < number
            && !found.contains(&n)
        {
            found.push(n);
        }
    }
    found
}

/// N, where a line opens with `Regression from #N`: after at most a list marker (`-`, `*`, `+`, `1.`, `1)`) and
/// emphasis (`**`, `__`, `*`, `_`).
fn claim(line: &str) -> Option<u64> {
    let mut t = line;
    for marker in ["- ", "* ", "+ "] {
        if let Some(r) = t.strip_prefix(marker) {
            t = r.trim_start();
            break;
        }
    }
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && matches!(t.as_bytes().get(digits), Some(b'.' | b')')) {
        t = t[digits + 1..].trim_start();
    }
    for em in ["**", "__", "*", "_"] {
        if let Some(r) = t.strip_prefix(em) {
            t = r;
            break;
        }
    }
    const SAID: &str = "regression from #";
    let head = t.get(..SAID.len())?;
    if !head.eq_ignore_ascii_case(SAID) {
        return None;
    }
    let rest = &t[SAID.len()..];
    let n = rest.bytes().take_while(u8::is_ascii_digit).count();
    rest[..n].parse().ok()
}

/// A description as words and meaning read it: HTML comments, tags and links left out.
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.split_once("-->").map_or("", |(_, after)| after);
            out.push(' ');
        } else if rest.starts_with('<')
            && rest[1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '/')
            && let Some(end) = rest.find('>')
        {
            rest = &rest[end + 1..];
            out.push(' ');
        } else if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == ')' || c == ']' || c == '>')
                .unwrap_or(rest.len());
            rest = &rest[end..];
        } else {
            let c = rest.chars().next().unwrap_or_default();
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out.trim().to_string()
}

/// `text` with its whitespace runs as one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Identifiers split into words (`DraftEmailCard` and `draft_email-card.tsx` both `draft email card …`).
fn split_identifiers(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut prev: Option<char> = None;
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if matches!(c, '/' | '.' | '_' | '-' | '`') {
            out.push(' ');
            prev = None;
            continue;
        }
        if let Some(p) = prev
            && c.is_uppercase()
            && (p.is_lowercase()
                || p.is_ascii_digit()
                || (p.is_uppercase() && chars.get(i + 1).is_some_and(|n| n.is_lowercase())))
        {
            out.push(' ');
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// A text's informative words, stemmed as the full-text index stems them, identifiers split.
fn word_set(text: &str) -> HashSet<String> {
    let words: Vec<String> = crate::search::terms(&split_identifiers(text))
        .into_iter()
        .filter(|t| crate::search::informative(t) && t.chars().count() > 1)
        .collect();
    crate::search::stems(&words.join(" "))
}

/// A fix's text as meaning reads it: its title and description start.
fn meaning_text(title: &str, about: &str) -> String {
    format!(
        "{title}\n{}",
        crate::advice_files::chars(&one_line(about), MEANING_CHARS)
    )
}

// --- Meaning ----------------------------------------------------------------------------------------------------

/// The vectors of these fixes (episode id, text), embedding the missing ones in the background: none unless every
/// one is ready within [`EMBED_WAIT`].
async fn vectors(fixes: Vec<(String, String)>) -> Option<HashMap<String, Arc<Vec<f32>>>> {
    let deadline = Instant::now() + EMBED_WAIT;
    let mut started = false;
    loop {
        let (ready, pending, missing) = {
            let have = VECTORS.lock().unwrap();
            let doing = EMBEDDING.lock().unwrap();
            let (mut ready, mut pending, mut missing) = (HashMap::new(), 0, vec![]);
            for (id, text) in &fixes {
                match have.get(id) {
                    Some(v) => {
                        ready.insert(id.clone(), v.clone());
                    }
                    None if doing.contains(id) => pending += 1,
                    None => missing.push((id.clone(), text.clone())),
                }
            }
            (ready, pending, missing)
        };
        if !started && !missing.is_empty() {
            started = true;
            let fresh: Vec<(String, String)> = {
                let mut doing = EMBEDDING.lock().unwrap();
                missing
                    .into_iter()
                    .filter(|(id, _)| doing.insert(id.clone()))
                    .collect()
            };
            if !fresh.is_empty() {
                tokio::spawn(embed_fixes(fresh));
            }
            continue;
        }
        if pending == 0 {
            // What failed to embed is missing still: meaning ranks nothing this time, and the next call asks again.
            return (ready.len() == fixes.len()).then_some(ready);
        }
        if Instant::now() >= deadline {
            eprintln!(
                "regression checks: {pending} fixes aren't embedded yet ({EMBED_WAIT:?}): ranked by files and words"
            );
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Embeds these fixes into [`VECTORS`], a batch at a time, through the embedding cache.
async fn embed_fixes(fixes: Vec<(String, String)>) {
    for batch in fixes.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
        match crate::search::embed_cache::embed_cached(texts).await {
            Ok(vs) => {
                let mut have = VECTORS.lock().unwrap();
                for ((id, _), v) in batch.iter().zip(vs) {
                    have.insert(id.clone(), Arc::new(v));
                }
            }
            Err(e) => eprintln!(
                "regression checks: embedding {} fixes failed: {e:#}",
                batch.len()
            ),
        }
        let mut doing = EMBEDDING.lock().unwrap();
        for (id, _) in batch {
            doing.remove(id);
        }
    }
}

// --- The checks ---------------------------------------------------------------------------------------------------

/// The checks for the change `asked` describes, from the pull requests `who` may read; `cx` phrases them. The
/// linked issues are read meanwhile.
pub async fn checks(
    session: &Session,
    store: &Store,
    who: &Principal,
    cx: &Extractor,
    asked: Asked<'_>,
) -> Result<Checks> {
    let ((checks, regressions, meaning), issues) = futures::try_join!(
        condensed(session, store, who, cx, &asked),
        crate::advice_files::issues(session, store, who, asked.issues),
    )?;
    Ok(Checks {
        checks,
        regressions,
        meaning,
        skills: vec![],
        issues,
    })
}

/// One regression, as a candidate.
struct Candidate {
    id: String,
    at: i64,
    fix: Arc<Pr>,
    intros: Vec<Arc<Pr>>,
    shared: Vec<String>,
    by_files: Option<usize>,
    by_words: Option<usize>,
    by_meaning: Option<usize>,
    score: f64,
}

/// The checks, the regressions they came from, and whether meaning ranked them.
async fn condensed(
    session: &Session,
    store: &Store,
    who: &Principal,
    cx: &Extractor,
    asked: &Asked<'_>,
) -> Result<(Vec<String>, Vec<Found>, bool)> {
    let in_repo = |r: &str| asked.repo.is_none_or(|want| want.eq_ignore_ascii_case(r));
    let as_of = asked
        .as_of
        .unwrap_or_else(|| chrono::Utc::now().timestamp_micros());
    let own = |p: &Pr| asked.pr.is_some_and(|n| p.number == n);

    // Every pull request merged before `as_of`, in the repository, once each (a scope holding a pull request in both
    // formats keeps the one listing more files).
    let mut prs: HashMap<(String, u64), (String, i64, Arc<Pr>)> = HashMap::new();
    for (id, at, pr) in read_scope(session, store, who).await? {
        if at >= as_of || !in_repo(&pr.repo) {
            continue;
        }
        let key = (pr.repo.to_ascii_lowercase(), pr.number);
        if prs
            .get(&key)
            .is_none_or(|(_, _, have)| pr.files.len() > have.files.len())
        {
            prs.insert(key, (id, at, pr));
        }
    }

    // How many pull requests changed each of the change's files.
    let files: BTreeSet<&str> = asked.files.iter().map(String::as_str).collect();
    let mut changed: HashMap<&str, usize> = HashMap::new();
    for (_, _, pr) in prs.values() {
        for f in pr.files.iter().collect::<BTreeSet<_>>() {
            if let Some(f) = files.get(f.as_str()) {
                *changed.entry(f).or_default() += 1;
            }
        }
    }
    let hub = (HUB_SHARE * prs.len() as f64).max(HUB_MIN);
    let weight = |f: &str| {
        let n = changed.get(f).copied().unwrap_or(0) as f64;
        if n > hub { 0.0 } else { 1.0 / (2.0 + n).log2() }
    };

    // The regressions: the fixes, each with the changes it names that the caller may read.
    let mut found: Vec<Candidate> = vec![];
    for (id, at, fix) in prs.values() {
        if !fix.fix || own(fix) || asked.pr.is_some_and(|n| fix.claims.contains(&n)) {
            continue;
        }
        let intros: Vec<Arc<Pr>> = fix
            .claims
            .iter()
            .filter_map(|n| prs.get(&(fix.repo.to_ascii_lowercase(), *n)))
            .map(|(_, _, p)| p.clone())
            .collect();
        found.push(Candidate {
            id: id.clone(),
            at: *at,
            fix: fix.clone(),
            intros,
            shared: vec![],
            by_files: None,
            by_words: None,
            by_meaning: None,
            score: 0.0,
        });
    }
    if found.is_empty() {
        return Ok((vec![], vec![], false));
    }

    // Files.
    let mut file_scores: Vec<(usize, f64)> = vec![];
    for (i, c) in found.iter_mut().enumerate() {
        let fix_files: HashSet<&str> = c.fix.files.iter().map(String::as_str).collect();
        let intro_files: HashSet<&str> = c
            .intros
            .iter()
            .flat_map(|p| p.files.iter().map(String::as_str))
            .collect();
        let mut score = 0.0;
        for f in &files {
            let w = weight(f);
            if w == 0.0 {
                continue;
            }
            if fix_files.contains(f) {
                score += w;
                c.shared.push(f.to_string());
            } else if intro_files.contains(f) {
                score += 0.5 * w;
                c.shared.push(f.to_string());
            }
        }
        if score > 0.0 {
            file_scores.push((i, score));
        }
    }

    // Words.
    let description = crate::advice_files::chars(asked.description, DESCRIPTION_MAX);
    let query = word_set(&format!(
        "{}\n{}\n{}",
        asked.title,
        crate::advice_files::chars(&clean(description), WORD_CHARS),
        asked.files.join(" ")
    ));
    let docs: Vec<HashSet<String>> = found
        .iter()
        .map(|c| {
            let mut w = c.fix.words.clone();
            for p in &c.intros {
                w.extend(word_set(&p.title));
            }
            w
        })
        .collect();
    let n = docs.len() as f64;
    let avg = (docs.iter().map(HashSet::len).sum::<usize>() as f64 / n).max(1.0);
    let idf: Vec<(&String, f64)> = query
        .iter()
        .map(|t| {
            let df = docs.iter().filter(|d| d.contains(t)).count() as f64;
            (t, (1.0 + (n - df + 0.5) / (df + 0.5)).ln())
        })
        .filter(|(_, idf)| *idf > 0.0)
        .collect();
    let word_scores: Vec<(usize, f64)> = docs
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let norm = K1 * (1.0 - B + B * d.len() as f64 / avg);
            let score: f64 = idf
                .iter()
                .filter(|(t, _)| d.contains(*t))
                .map(|(_, f)| f * (K1 + 1.0) / (1.0 + norm))
                .sum();
            (score > 0.0).then_some((i, score))
        })
        .collect();

    // Meaning.
    let change_text = meaning_text(asked.title, &clean(description));
    let (fixed, asked_vector) = futures::join!(
        vectors(
            found
                .iter()
                .map(|c| (c.id.clone(), meaning_text(&c.fix.title, &c.fix.about)))
                .collect()
        ),
        crate::search::embed::embed(vec![change_text]),
    );
    let asked_vector = match asked_vector {
        Ok(mut v) => v.pop(),
        Err(e) => {
            eprintln!("regression checks: embedding the change failed: {e:#}");
            None
        }
    };
    let meaning_scores: Option<Vec<(usize, f64)>> = match (fixed, asked_vector) {
        (Some(fixed), Some(q)) => Some(
            found
                .iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    let v = fixed.get(&c.id)?;
                    Some((i, v.iter().zip(&q).map(|(a, b)| f64::from(a * b)).sum()))
                })
                .collect(),
        ),
        _ => None,
    };
    let meaning = meaning_scores.is_some();

    // Fused, by recency.
    let ranked = |mut scores: Vec<(usize, f64)>| -> Vec<(usize, usize)> {
        scores.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then(found[b.0].at.cmp(&found[a.0].at))
                .then(found[a.0].fix.number.cmp(&found[b.0].fix.number))
        });
        scores
            .into_iter()
            .enumerate()
            .map(|(r, (i, _))| (i, r + 1))
            .collect()
    };
    let (by_files, by_words, by_meaning) = (
        ranked(file_scores),
        ranked(word_scores),
        meaning_scores.map(ranked).unwrap_or_default(),
    );
    for (i, r) in by_files {
        found[i].by_files = Some(r);
    }
    for (i, r) in by_words {
        found[i].by_words = Some(r);
    }
    for (i, r) in by_meaning {
        found[i].by_meaning = Some(r);
    }
    for c in &mut found {
        let rrf: f64 = [c.by_files, c.by_words, c.by_meaning]
            .into_iter()
            .flatten()
            .map(|r| 1.0 / (RRF_K + r as f64))
            .sum();
        let age = ((as_of - c.at) as f64 / DAY_US).max(0.0);
        c.score =
            (rrf * (0.5 + 0.5 * (-age / HALF_LIFE_DAYS).exp2()) * 10_000.0).round() / 10_000.0;
    }
    found.retain(|c| c.score > 0.0);
    found.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.at.cmp(&a.at))
            .then(a.fix.number.cmp(&b.fix.number))
    });
    found.truncate(CANDIDATES);
    if found.is_empty() {
        return Ok((vec![], vec![], meaning));
    }

    // Phrased.
    let name = |repo: &str, n: u64| match asked.repo {
        Some(_) => format!("#{n}"),
        None => format!("{repo}#{n}"),
    };
    let blocks: Vec<String> = found
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut lines = vec![format!(
                "[{}] Fix {} (merged {}): {}",
                i + 1,
                name(&c.fix.repo, c.fix.number),
                day(c.at),
                c.fix.title
            )];
            for p in &c.intros {
                lines.push(format!("Broke in {}: {}", name(&p.repo, p.number), p.title));
            }
            let shown: Vec<&str> = c
                .fix
                .files
                .iter()
                .take(FIX_FILES)
                .map(String::as_str)
                .collect();
            let more = c.fix.files.len().saturating_sub(FIX_FILES);
            lines.push(format!(
                "Files: {}{}",
                shown.join(", "),
                if more > 0 {
                    format!(" (and {more} more)")
                } else {
                    String::new()
                }
            ));
            if !c.shared.is_empty() {
                let shared: Vec<&str> = c
                    .shared
                    .iter()
                    .take(SHARED_SHOWN)
                    .map(String::as_str)
                    .collect();
                lines.push(format!("Shares with this change: {}", shared.join(", ")));
            }
            let about = paragraphs(crate::advice_files::chars(&c.fix.about, PROMPT_CHARS));
            if !about.is_empty() {
                lines.push(about);
            }
            lines.join("\n")
        })
        .collect();
    let change = match asked.pr {
        Some(n) => format!("PR #{n}: {}", asked.title),
        None => format!("PR: {}", asked.title),
    };
    let shown: Vec<&str> = asked
        .files
        .iter()
        .take(CHANGE_FILES)
        .map(String::as_str)
        .collect();
    let prompt = format!(
        "You are preparing review checks for a pull request from the regressions this repository fixed before.\n\n\
         The change under review:\n{change}\nDescription (start): {}\nChanged files: {}\n\n\
         Below are up to {} earlier fixes, each numbered, with the change it says broke things when it names one. \
         Each records an invariant: something that must stay true, which an earlier change broke.\n\n\
         For each fix whose invariant this change could break again (it touches the same code, feature or setting, \
         or makes the same kind of change), write one check, as one line:\n\
         [n] If this change <the condition: what in this change would put the invariant at risk, concretely>, \
         verify <the invariant: what must stay true, naming the code, setting, test or behavior the fix names>.\n\
         Use only what the fixes say. Leave out a fix whose invariant this change can't plausibly break, and leave \
         out generic advice (style, add tests, handle errors). At most {} lines, the most likely to be broken \
         first. If no fix applies, reply NONE.\n\n{}\n\nReply with the lines only.",
        crate::advice_files::chars(&one_line(&clean(description)), CHANGE_CHARS),
        shown.join(", "),
        found.len(),
        asked.top,
        blocks.join("\n\n")
    );
    let reply = crate::advice_files::condense(cx, &prompt).await?;

    // Each line naming a candidate, once, in the ranking's order, with its sources.
    let mut written: Vec<(usize, String)> = vec![];
    for line in reply.lines() {
        let Some((i, text)) = numbered(line) else {
            continue;
        };
        if i == 0 || i > found.len() || written.iter().any(|(j, _)| *j == i - 1) {
            continue;
        }
        written.push((i - 1, text));
    }
    written.sort_by_key(|(i, _)| *i);
    written.truncate(asked.top);
    let mut out: Vec<Found> = found
        .iter()
        .map(|c| Found {
            fix: format!("{}#{}", c.fix.repo, c.fix.number),
            title: c.fix.title.clone(),
            merged: day(c.at),
            introduced_by: c
                .intros
                .iter()
                .map(|p| format!("{}#{}", p.repo, p.number))
                .collect(),
            score: c.score,
            by_files: c.by_files,
            by_words: c.by_words,
            by_meaning: c.by_meaning,
            shared: c.shared.clone(),
            check: None,
        })
        .collect();
    let mut checks = vec![];
    for (i, text) in written {
        let c = &found[i];
        let fixed = name(&c.fix.repo, c.fix.number);
        let source = if c.intros.is_empty() {
            format!("it broke before (fixed in {fixed})")
        } else {
            let intros: Vec<String> = c.intros.iter().map(|p| name(&p.repo, p.number)).collect();
            format!(
                "it broke before in {} (fixed in {fixed})",
                intros.join(", ")
            )
        };
        let text = text.trim_end_matches(['.', ';', ' ']);
        out[i].check = Some(checks.len());
        checks.push(format!("- {text}; {source}."));
    }
    Ok((checks, out, meaning))
}

/// A reply line `[n] text` (after an optional `- ` or `* `), as (n, text).
fn numbered(line: &str) -> Option<(usize, String)> {
    let t = line.trim();
    let t = t
        .strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .unwrap_or(t)
        .trim_start();
    let (n, text) = t.strip_prefix('[')?.split_once(']')?;
    let (n, text) = (n.trim().parse().ok()?, text.trim());
    (!text.is_empty()).then(|| (n, text.to_string()))
}

/// Text with its blank lines dropped.
fn paragraphs(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The day (UTC) of a time in microseconds.
fn day(us: i64) -> String {
    chrono::DateTime::from_timestamp_micros(us)
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}
