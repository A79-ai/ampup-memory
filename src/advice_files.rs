//! Review checks keyed by the files a change touches (`POST /advice {files}`): what the scope's compiled skills
//! say about those files, as a reviewer's checks, not as steps to follow.
//!
//! The routed answer ([`crate::advice::get`]) serves the one skill whose task is most like the question: on a code
//! review, the how-to for a similar change. A reviewer needs something else: what earlier changes to the same files
//! broke, and which invariants they rest on. On our code-review benchmark (the 65 test pairs
//! whose library held a skill relevant to the bug, 2 runs, the bot's `regress-all` prompt), the routed procedure
//! raised catches from 14.6% to 18.5%, these checks to 19.2% with 22% fewer concerns, and the checks with the PR's
//! linked Linear issue to 19.2% (no difference significant on 65 pairs).
//!
//! How a change's checks are made:
//! 1. **Candidates.** Each skill the caller may read whose source pull requests (its sources' names,
//!    `github:<owner>/<repo>#<n>`, a review thread's `…#<n>/thread/…` naming its pull request) changed a file the
//!    change does. A pull request's files are its episode's `## Files changed` list, as the GitHub loader writes it.
//!    A skill citing a pull request the caller can't read, or the change itself (`pr`), is left out.
//! 2. **Ranking.** Each shared file counts 1/log2(2 + n), n the pull requests the caller may read that changed it,
//!    so a file every change touches counts least. A source that fixed something (its title starts `fix`, `bug`,
//!    `hotfix` or `revert`, after an optional `[TICKET]`) adds 0.5, one citing a human review thread 0.5. The best
//!    [`TOP`] are kept, newest first among equals.
//! 3. **Condensing.** The routing model (Claude Haiku 5.5 by default, [`Extractor::route_from_env`]) turns them into
//!    2 to 8 bullets, each naming a changed file, the invariant or past breakage, and its source pull request. It
//!    sees the change's title and files and the skills, never the diff.
//!
//! `issues` names issue keys (`ENG-123`) the change links: the newest version of each `linear:<workspace>/<KEY>`
//! episode the caller may read is returned with the checks, for the caller to show as it likes.
//!
//! Reading every pull request's files is one body read per pull request episode: the first answer in a process
//! reads them all, at most [`READS`] at once, and later answers only the new ones (episode ids name their content,
//! so a body read once is kept by id, and served only to a caller the session lets see that id).
//!
//! The condensing call is nearly all of an answer's time (on one deployment, 6 to 15 s of a 7 to 16 s answer: Claude Haiku
//! 5.5 thinks before it writes, about two thirds of the tokens it returns). A process keeps the last [`KEPT`]
//! condensing replies by the request that asked for them (the routing model and the whole prompt: the change's
//! title, number and files, and each skill's slug, sources and text at the version the caller sees), so the same
//! change asked again with the library unchanged (a re-run, a push that touched no other file) is answered
//! byte for byte as before, without a model call. A reply the model cut short is never kept. The linked issues are
//! read while the checks are made.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::Result;
use futures::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::extract::Extractor;
use crate::{Principal, Session, Store};

/// Skills condensed for one change.
pub const TOP: usize = 5;
/// Each skill's text, as the condenser reads it, in characters.
const SKILL_CHARS: usize = 3_000;
/// The change's files the condenser is shown.
const PROMPT_FILES: usize = 40;
/// Of a skill's shared files, those the condenser is shown.
const SHARED_SHOWN: usize = 6;
/// Episode bodies read at once.
const READS: usize = 32;
/// Most tokens a condensing reply may take.
const REPLY_TOKENS: u32 = 3_000;
/// Condensing replies a process keeps ([`condense`]); at about 2 KB each, a few MB.
const KEPT: usize = 1_024;

/// What a caller asks: the change's files, its title, its pull request (left out of the sources), its repository
/// (`owner/repo`: only its pull requests count; none, every repository's), the issue keys it links, and how many
/// skills to condense.
pub struct Asked<'a> {
    pub files: &'a [String],
    pub title: &'a str,
    pub pr: Option<u64>,
    pub repo: Option<&'a str>,
    pub issues: &'a [String],
    pub top: usize,
}

/// The checks for one change.
#[derive(Serialize)]
pub struct Checks {
    /// The bullets, each `- …`; none when no skill shares a file or the condenser found nothing to check.
    pub checks: Vec<String>,
    /// The skills condensed, best first.
    pub skills: Vec<Found>,
    /// The linked issues the caller may read, in the order asked.
    pub issues: Vec<Issue>,
}

#[derive(Serialize)]
pub struct Found {
    /// `skill:<slug>`.
    pub key: String,
    pub score: f64,
    /// The change's files its sources changed.
    pub shared: Vec<String>,
    /// Its source pull requests, `owner/repo#n`.
    pub sources: Vec<String>,
}

#[derive(Serialize)]
pub struct Issue {
    /// The key asked for, as the episode's source spells it.
    pub key: String,
    pub source: String,
    pub occurred_at: String,
    pub text: String,
}

/// A pull request, as its episode tells it.
struct Pr {
    title: String,
    files: Vec<String>,
}

/// A skill document's version: its sources' names and its text after the meta line.
struct SkillDoc {
    names: Vec<String>,
    text: String,
}

/// Bodies read once, by episode id (an id names its content).
static PRS: LazyLock<Mutex<HashMap<String, Arc<Pr>>>> = LazyLock::new(Default::default);
static SKILLS: LazyLock<Mutex<HashMap<String, Arc<SkillDoc>>>> = LazyLock::new(Default::default);

/// Condensing replies by the request that asked for them, the oldest dropped first past [`KEPT`].
#[derive(Default)]
struct Replies {
    by: HashMap<[u8; 32], Arc<str>>,
    order: VecDeque<[u8; 32]>,
}
static REPLIES: LazyLock<Mutex<Replies>> = LazyLock::new(Default::default);

/// `cx`'s condensing reply to `prompt`: the one this process kept for the same request (backend, model, token cap
/// and prompt), else the model's, kept when whole. Two asks at once both call the model; the first reply kept is
/// the one every later ask gets.
async fn condense(cx: &Extractor, prompt: &str) -> Result<String> {
    let key: [u8; 32] = Sha256::new()
        .chain_update(cx.backend())
        .chain_update([0])
        .chain_update(cx.model())
        .chain_update([0])
        .chain_update(REPLY_TOKENS.to_le_bytes())
        .chain_update(prompt)
        .finalize()
        .into();
    if let Some(r) = REPLIES.lock().unwrap().by.get(&key) {
        return Ok(r.to_string());
    }
    let (reply, whole) = cx.complete_whole(prompt, REPLY_TOKENS).await?;
    if !whole {
        return Ok(reply);
    }
    let mut kept = REPLIES.lock().unwrap();
    if let Some(r) = kept.by.get(&key) {
        return Ok(r.to_string());
    }
    if kept.order.len() >= KEPT
        && let Some(old) = kept.order.pop_front()
    {
        kept.by.remove(&old);
    }
    kept.order.push_back(key);
    kept.by.insert(key, reply.as_str().into());
    Ok(reply)
}

/// `github:<owner>/<repo>#<n>[/thread/…]` as (`owner/repo`, n, whether a review thread); none for another source.
fn pr_of(name: &str) -> Option<(&str, u64, bool)> {
    let (repo, rest) = name.strip_prefix("github:")?.split_once('#')?;
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let n = rest[..digits].parse().ok()?;
    match &rest[digits..] {
        "" => Some((repo, n, false)),
        t if t.starts_with("/thread/") => Some((repo, n, true)),
        _ => None,
    }
}

/// A pull request episode's title (its first line, without the loader's `(pr …, merged …)`) and the files its
/// `## Files changed` list names (the loader's last such section: a description may hold one of its own).
fn parse_pr(body: &str) -> Pr {
    let first = body.lines().next().unwrap_or_default();
    let first = first.strip_prefix("# ").unwrap_or(first);
    let title = first
        .rsplit_once(" (pr ")
        .map_or(first, |(t, _)| t)
        .to_string();
    let mut files = vec![];
    const HEAD: &str = "\n## Files changed\n";
    if let Some(i) = body.rfind(HEAD) {
        for line in body[i + HEAD.len()..].lines() {
            if line.starts_with("## ") {
                break;
            }
            // `- <path> (+a −b)`; `- … and N more` names none.
            if let Some((path, _)) = line.strip_prefix("- ").and_then(|l| l.rsplit_once(" (+")) {
                files.push(path.to_string());
            }
        }
    }
    Pr { title, files }
}

/// Whether a pull request's title says it fixed something: `fix`, `bug`, `hotfix` or `revert` first, after an
/// optional `[TICKET]`.
fn fixes(title: &str) -> bool {
    let t = title.trim_start();
    let t = match t.strip_prefix('[').and_then(|r| r.split_once(']')) {
        Some((_, rest)) => rest.trim_start(),
        None => t,
    };
    let t = t.to_ascii_lowercase();
    ["fix", "bug", "hotfix", "revert"]
        .iter()
        .any(|w| t.starts_with(w))
}

/// The first `n` characters.
fn chars(text: &str, n: usize) -> &str {
    text.char_indices().nth(n).map_or(text, |(i, _)| &text[..i])
}

/// The bullets of a condensing reply: none when it says NONE; else its `- ` (or `* `) lines, or every line when it
/// has none.
fn bullets(reply: &str) -> Vec<String> {
    let lines: Vec<&str> = reply
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines
        .first()
        .is_none_or(|l| l.trim().to_uppercase().starts_with("NONE"))
    {
        return vec![];
    }
    let marked: Vec<String> = lines
        .iter()
        .filter(|l| {
            let l = l.trim_start();
            l.starts_with("- ") || l.starts_with("* ")
        })
        .map(|l| l.to_string())
        .collect();
    if marked.is_empty() {
        lines.into_iter().map(String::from).collect()
    } else {
        marked
    }
}

/// An episode's id and body.
type Body = (String, Vec<u8>);

/// Bodies of `ids` not yet in `cache`, read as `who` and parsed, at most [`READS`] at once.
async fn fill<T>(
    session: &Session,
    store: &Store,
    who: &Principal,
    cache: &Mutex<HashMap<String, Arc<T>>>,
    ids: Vec<String>,
    parse: fn(&str) -> Option<T>,
) -> Result<()> {
    let missing: Vec<String> = {
        let have = cache.lock().unwrap();
        ids.into_iter()
            .filter(|id| !have.contains_key(id))
            .collect()
    };
    let read: Vec<Result<Option<Body>>> = futures::stream::iter(missing)
        .map(|id| async move {
            Ok(session
                .episode_body(store, who, &id)
                .await?
                .map(|b| (id, b)))
        })
        .buffer_unordered(READS)
        .collect()
        .await;
    let mut have = cache.lock().unwrap();
    for r in read {
        if let Some((id, body)) = r?
            && let Some(v) = parse(&String::from_utf8_lossy(&body))
        {
            have.insert(id, Arc::new(v));
        }
    }
    Ok(())
}

/// The episodes `who` may read on this session whose source passes `keep`: the newest of each source, as
/// source -> (episode id, occurred_at).
async fn newest_by_source(
    session: &Session,
    store: &Store,
    who: &Principal,
    keep: impl Fn(&str) -> bool,
) -> Result<HashMap<String, (String, i64)>> {
    let vis = session.vis();
    let snap = store
        .rows(&session.scope().namespace(), crate::EPISODES)
        .await?;
    let mut out: HashMap<String, (String, i64)> = HashMap::new();
    for b in snap.rows.iter() {
        let sources = crate::strings(b, "source");
        if !sources.iter().any(|s| keep(s)) {
            continue;
        }
        let (ids, commits) = (
            crate::strings(b, "episode_id"),
            crate::strings(b, "commit_id"),
        );
        let at = crate::timestamps(b, "occurred_at");
        for i in 0..ids.len() {
            if !keep(&sources[i])
                || !vis.committed.contains(&commits[i])
                || !vis.can_see_episode(&ids[i], who)
            {
                continue;
            }
            let t = at[i].unwrap_or_default();
            let e = out
                .entry(sources[i].clone())
                .or_insert_with(|| (ids[i].clone(), t));
            if t > e.1 {
                *e = (ids[i].clone(), t);
            }
        }
    }
    Ok(out)
}

/// The checks for the change `asked` describes, from what `who` may read; `cx` condenses them. The linked issues
/// are read meanwhile.
pub async fn checks(
    session: &Session,
    store: &Store,
    who: &Principal,
    cx: &Extractor,
    asked: Asked<'_>,
) -> Result<Checks> {
    let ((checks, skills), issues) = futures::try_join!(
        condensed(session, store, who, cx, &asked),
        issues(session, store, who, asked.issues),
    )?;
    Ok(Checks {
        checks,
        skills,
        issues,
    })
}

/// The bullets and the skills they were condensed from.
async fn condensed(
    session: &Session,
    store: &Store,
    who: &Principal,
    cx: &Extractor,
    asked: &Asked<'_>,
) -> Result<(Vec<String>, Vec<Found>)> {
    let in_repo = |r: &str| asked.repo.is_none_or(|want| want.eq_ignore_ascii_case(r));

    // Every pull request the caller may read, in the repository, with its files.
    let prs = newest_by_source(session, store, who, |s| {
        pr_of(s).is_some_and(|(r, _, thread)| !thread && in_repo(r))
    })
    .await?;
    fill(
        session,
        store,
        who,
        &PRS,
        prs.values().map(|(id, _)| id.clone()).collect(),
        |b| Some(parse_pr(b)),
    )
    .await?;
    let read: HashMap<(String, u64), (Arc<Pr>, i64)> = {
        let have = PRS.lock().unwrap();
        prs.iter()
            .filter_map(|(s, (id, at))| {
                let (r, n, _) = pr_of(s)?;
                Some(((r.to_string(), n), (have.get(id)?.clone(), *at)))
            })
            .collect()
    };
    let files: BTreeSet<&str> = asked.files.iter().map(String::as_str).collect();
    let mut changed: HashMap<&str, usize> = HashMap::new();
    for (pr, _) in read.values() {
        for f in pr.files.iter().collect::<BTreeSet<_>>() {
            if let Some(f) = files.get(f.as_str()) {
                *changed.entry(f).or_default() += 1;
            }
        }
    }
    let weight = |f: &str| 1.0 / (2.0 + changed.get(f).copied().unwrap_or(0) as f64).log2();

    // Every skill the caller may read, as the routing index lists them, at the version they see.
    let mut versions = BTreeMap::new();
    for s in crate::advice::skills(session, store, who).await? {
        let key = crate::compile::skill_key(&s.slug);
        if let Some((_, id)) = session.vis().version_at(&key, who, None) {
            versions.insert(s.slug, id.to_string());
        }
    }
    fill(
        session,
        store,
        who,
        &SKILLS,
        versions.values().cloned().collect(),
        |b| {
            let (meta, text) = crate::compile::parse_skill(b.as_bytes())?;
            let text = text
                .split_once('\n')
                .map_or("", |(_, t)| t)
                .trim()
                .to_string();
            Some(SkillDoc {
                names: meta.names,
                text,
            })
        },
    )
    .await?;

    struct Candidate {
        slug: String,
        score: f64,
        newest: i64,
        sources: Vec<(String, u64)>,
        shared: Vec<String>,
        doc: Arc<SkillDoc>,
    }
    let mut found = vec![];
    for (slug, id) in &versions {
        let Some(doc) = SKILLS.lock().unwrap().get(id).cloned() else {
            continue;
        };
        let cited: Vec<(&str, u64, bool)> = doc
            .names
            .iter()
            .filter_map(|n| pr_of(n))
            .filter(|(r, _, _)| in_repo(r))
            .collect();
        let nums: BTreeSet<(String, u64)> =
            cited.iter().map(|(r, n, _)| (r.to_string(), *n)).collect();
        if nums.is_empty()
            || asked.pr.is_some_and(|p| nums.iter().any(|(_, n)| *n == p))
            || nums.iter().any(|k| !read.contains_key(k))
        {
            continue;
        }
        let shared: BTreeSet<&str> = nums
            .iter()
            .flat_map(|k| read[k].0.files.iter().map(String::as_str))
            .filter(|f| files.contains(f))
            .collect();
        if shared.is_empty() {
            continue;
        }
        let score = shared.iter().map(|f| weight(f)).sum::<f64>()
            + 0.5 * f64::from(nums.iter().any(|k| fixes(&read[k].0.title)))
            + 0.5 * f64::from(cited.iter().any(|(_, _, thread)| *thread));
        found.push(Candidate {
            slug: slug.clone(),
            score: (score * 10_000.0).round() / 10_000.0,
            newest: nums.iter().map(|k| read[k].1).max().unwrap_or_default(),
            sources: nums.into_iter().collect(),
            shared: shared.into_iter().map(String::from).collect(),
            doc,
        });
    }
    found.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.newest.cmp(&a.newest))
            .then(a.slug.cmp(&b.slug))
    });
    found.truncate(asked.top);
    if found.is_empty() {
        return Ok((vec![], vec![]));
    }

    let name = |(r, n): &(String, u64)| match asked.repo {
        Some(_) => format!("#{n}"),
        None => format!("{r}#{n}"),
    };
    let blocks: Vec<String> = found
        .iter()
        .map(|c| {
            let from: Vec<String> = c
                .sources
                .iter()
                .map(|k| format!("{} {}", name(k), read[k].0.title))
                .collect();
            let shares: Vec<&str> = c
                .shared
                .iter()
                .take(SHARED_SHOWN)
                .map(String::as_str)
                .collect();
            format!(
                "=== skill {} (from {}; shares {}) ===\n{}",
                c.slug,
                from.join("; "),
                shares.join(", "),
                chars(&c.doc.text, SKILL_CHARS)
            )
        })
        .collect();
    let shown: Vec<&str> = asked
        .files
        .iter()
        .take(PROMPT_FILES)
        .map(String::as_str)
        .collect();
    let change = match asked.pr {
        Some(n) => format!("PR #{n}: {}", asked.title),
        None => format!("PR: {}", asked.title),
    };
    let prompt = format!(
        "You are preparing review checks for a pull request, from the team's memory of earlier pull requests that \
         changed the same files.\n\n{change}\nChanged files: {}\n\nBelow are up to {} skills compiled from earlier \
         merged PRs that changed some of these files. Each was written as a procedure for doing a task. Pull out \
         what a reviewer of THIS PR should check.\n\nWrite 2 to 8 bullets. Each bullet:\n- names the changed file \
         it applies to (and the function, endpoint, config key or test, when the skill names one);\n- states one \
         invariant, contract, known pitfall or past breakage the skill records for that code, and what in a change \
         like this one could violate it, including tests, callers or config outside this PR that assume the old \
         behavior;\n- ends with its source PR, as (#1234).\nUse only what the skills say. No generic advice (style, \
         add tests, handle errors) unless a skill ties it to this code. Leave out skills with nothing checkable for \
         these files. If nothing applies, reply NONE.\n\n{}\n\nReply with the bullets only, each starting with \
         \"- \".",
        shown.join(", "),
        found.len(),
        blocks.join("\n\n")
    );
    let reply = condense(cx, &prompt).await?;
    Ok((
        bullets(&reply),
        found
            .into_iter()
            .map(|c| Found {
                key: format!("{}{}", crate::advice::SKILL, c.slug),
                score: c.score,
                shared: c.shared,
                sources: c.sources.iter().map(|(r, n)| format!("{r}#{n}")).collect(),
            })
            .collect(),
    ))
}

/// The newest version of each issue `keys` names (`linear:<workspace>/<KEY>`) that `who` may read, in the order
/// asked, each once.
async fn issues(
    session: &Session,
    store: &Store,
    who: &Principal,
    keys: &[String],
) -> Result<Vec<Issue>> {
    if keys.is_empty() {
        return Ok(vec![]);
    }
    let want: BTreeSet<String> = keys.iter().map(|k| k.to_ascii_uppercase()).collect();
    let key_of = |s: &str| {
        s.strip_prefix("linear:")
            .and_then(|r| r.rsplit_once('/'))
            .map(|(_, k)| k.to_ascii_uppercase())
    };
    let found = newest_by_source(session, store, who, |s| {
        key_of(s).is_some_and(|k| want.contains(&k))
    })
    .await?;
    let mut newest: HashMap<String, (&String, &String, i64)> = HashMap::new();
    for (source, (id, at)) in &found {
        let k = key_of(source).unwrap_or_default();
        if newest.get(&k).is_none_or(|(_, _, t)| at > t) {
            newest.insert(k, (source, id, *at));
        }
    }
    let mut seen = BTreeSet::new();
    let wanted: Vec<_> = keys
        .iter()
        .filter_map(|k| {
            let k = k.to_ascii_uppercase();
            let found = newest.get(&k)?;
            seen.insert(k).then_some(found)
        })
        .collect();
    // The bodies at once, in the order asked.
    let bodies = futures::future::try_join_all(
        wanted
            .iter()
            .map(|(_, id, _)| session.episode_body(store, who, id)),
    )
    .await?;
    Ok(wanted
        .into_iter()
        .zip(bodies)
        .filter_map(|((source, _, at), body)| {
            Some(Issue {
                key: source.rsplit_once('/').map_or("", |(_, k)| k).to_string(),
                source: source.to_string(),
                occurred_at: crate::audit::rfc3339(*at),
                text: String::from_utf8_lossy(&body?).into_owned(),
            })
        })
        .collect())
}
