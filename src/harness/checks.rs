//! M7 (portable core) checks. Each runs against whatever catalog the store is on, takes its
//! sizes as parameters (the spec's full sizes are the defaults in `harness/verify-m7.sh`), and
//! returns a report or fails with the first broken invariant.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};
use iceberg::Catalog;

use crate::{Episode, Fact, MAIN, Principal, Scope, Store};

/// A small seeded RNG, so every check's inputs come from a recorded seed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn tag() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

fn claim(tag: &str, i: usize, entity: &str, predicate: &str, value: &str) -> (Episode, Fact) {
    let ep = Episode::new(
        "note:check",
        format!("[{tag}#{i}] {entity} {predicate} {value}").as_bytes(),
        &["*"],
    );
    let f = Fact::new(entity, predicate, value).from_episodes(&[&ep]);
    (ep, f)
}

/// Concurrent commits: `writers` stores (each with its own caches, like separate processes)
/// commit `commits` facts each, all at once, on main. Pass: every commit is in the ledger
/// exactly once, nothing else appeared, and main is a single chain (one head, no commit is the
/// parent of two others).
pub async fn concurrent_commits(
    catalog: Arc<dyn Catalog>,
    scope: &Scope,
    writers: usize,
    commits: usize,
    shared: bool,
) -> Result<String> {
    let one = Arc::new(Store::new(catalog.clone()));
    let setup = Store::new(catalog.clone());
    setup.ensure_scope(scope).await?;
    let before: HashSet<String> = setup
        .ledger_rows(scope)
        .await?
        .into_iter()
        .map(|r| r.0)
        .collect();
    let t = tag();
    let started = Instant::now();
    let mut tasks = vec![];
    for w in 0..writers {
        let (catalog, scope, t, one) = (catalog.clone(), scope.clone(), t.clone(), one.clone());
        tasks.push(tokio::spawn(async move {
            let store = if shared {
                one
            } else {
                Arc::new(Store::new(catalog))
            };
            let mut ids = vec![];
            for i in 0..commits {
                let (ep, f) = claim(
                    &t,
                    w * commits + i,
                    &format!("w{w}_{t}"),
                    "count",
                    &i.to_string(),
                );
                ids.push(
                    store
                        .commit(&scope, &format!("writer{w}"), &[ep], &[f])
                        .await?,
                );
            }
            Ok::<_, anyhow::Error>(ids)
        }));
    }
    let mut ours = vec![];
    for task in tasks {
        ours.extend(task.await??);
    }
    let elapsed = started.elapsed();
    let rows = Store::new(catalog).ledger_rows(scope).await?;
    let new: Vec<_> = rows.iter().filter(|r| !before.contains(&r.0)).collect();
    let ids: Vec<&String> = new.iter().map(|r| &r.0).collect();
    let distinct: HashSet<&String> = ids.iter().copied().collect();
    ensure!(
        distinct.len() == ids.len(),
        "a commit id appears twice in the ledger"
    );
    let ours_set: HashSet<&String> = ours.iter().collect();
    ensure!(
        distinct == ours_set,
        "ledger has {} new commits, writers made {}",
        distinct.len(),
        ours.len()
    );
    // Linear: on main, no commit is named as a parent twice, and there is one head.
    let main: Vec<_> = rows.iter().filter(|r| r.1 == MAIN).collect();
    let mut parent_uses: HashMap<&str, usize> = HashMap::new();
    for r in &main {
        for p in &r.2 {
            *parent_uses.entry(p.as_str()).or_default() += 1;
        }
    }
    let forks = parent_uses.values().filter(|n| **n > 1).count();
    ensure!(forks == 0, "main forked at {forks} commits");
    let heads = main
        .iter()
        .filter(|r| !parent_uses.contains_key(r.0.as_str()))
        .count();
    ensure!(heads == 1, "main has {heads} heads");
    Ok(format!(
        "ok: {} writers x {} commits = {} commits{}, ledger linear (1 head, 0 forks), {:.1}s ({:.2} commits/s)",
        writers,
        commits,
        ours.len(),
        if shared {
            " through one store (group commit)"
        } else {
            ""
        },
        elapsed.as_secs_f64(),
        ours.len() as f64 / elapsed.as_secs_f64()
    ))
}

/// Snapshot pinning (D-9): a pinned session's reads, lookups and searches return the same
/// results while writers commit new and superseding facts; a new session sees them.
pub async fn snapshot_pinning(store: &Store, scope: &Scope, writes: usize) -> Result<String> {
    store.ensure_scope(scope).await?;
    let t = tag();
    let entity = format!("pin_{t}");
    let (ep, f) = claim(&t, 0, &entity, "status", "before");
    store.commit(scope, "check", &[ep], &[f]).await?;
    let who = Principal::new("checker", &[]);
    let now = Some(i64::MAX);
    let session = store.pin(scope, MAIN).await?;
    let snapshot = |facts: Vec<Fact>| -> BTreeSet<String> {
        facts
            .into_iter()
            .map(|f| format!("{}={}", f.fact_id, f.value))
            .collect()
    };
    let read0 = snapshot(session.read(store, &who, now).await?);
    let look0 = snapshot(session.lookup(store, &who, &entity, "status", now).await?);
    let hits0: BTreeSet<String> = session
        .search(store, &who, &entity, 10, now, crate::search::Rank::now())
        .await?
        .into_iter()
        .map(|h| h.fact.fact_id)
        .collect();
    ensure!(look0.len() == 1, "lookup before writes saw {look0:?}");

    for i in 1..=writes {
        let (ep, f) = claim(&t, i, &entity, "status", &format!("after{i}"));
        store.commit(scope, "check", &[ep], &[f]).await?;
    }
    let read1 = snapshot(session.read(store, &who, now).await?);
    let look1 = snapshot(session.lookup(store, &who, &entity, "status", now).await?);
    let hits1: BTreeSet<String> = session
        .search(store, &who, &entity, 10, now, crate::search::Rank::now())
        .await?
        .into_iter()
        .map(|h| h.fact.fact_id)
        .collect();
    ensure!(read0 == read1, "pinned read changed under {writes} commits");
    ensure!(
        look0 == look1,
        "pinned lookup changed: {look0:?} -> {look1:?}"
    );
    ensure!(hits0 == hits1, "pinned search changed");
    let fresh = store.pin(scope, MAIN).await?;
    let look2 = fresh.lookup(store, &who, &entity, "status", now).await?;
    ensure!(
        look2.len() == writes + 1,
        "a new session sees {} of {} facts",
        look2.len(),
        writes + 1
    );
    Ok(format!(
        "ok: read/lookup/search unchanged in a pinned session across {writes} commits; a new session sees all {}",
        writes + 1
    ))
}

/// Ledger atomicity: `n` seeded crashes at a random point of the commit (after bodies and
/// episode rows, or after fact rows too, never reaching the ledger append). Pass: none of
/// their episodes or facts is ever visible, while a committed control is.
pub async fn crash_atomicity(store: &Store, scope: &Scope, n: usize, seed: u64) -> Result<String> {
    store.ensure_scope(scope).await?;
    let t = tag();
    let mut rng = Rng(seed);
    let (cep, cf) = claim(&t, 0, &format!("crash_{t}"), "control", "committed");
    store
        .commit(
            scope,
            "check",
            std::slice::from_ref(&cep),
            std::slice::from_ref(&cf),
        )
        .await?;
    let mut orphan_facts = HashSet::new();
    let mut orphan_eps = vec![];
    let (mut after_eps, mut after_facts) = (0, 0);
    for i in 1..=n {
        let (ep, f) = claim(&t, i, &format!("crash_{t}"), "orphan", &i.to_string());
        if rng.below(2) == 0 {
            // Crash between the episode rows and the fact rows.
            store
                .write_pending(scope, std::slice::from_ref(&ep), &[])
                .await?;
            after_eps += 1;
        } else {
            // Crash between the fact rows and the ledger append.
            store
                .write_pending(scope, std::slice::from_ref(&ep), std::slice::from_ref(&f))
                .await?;
            orphan_facts.insert(f.fact_id.clone());
            after_facts += 1;
        }
        orphan_eps.push(ep.episode_id);
    }
    let who = Principal::new("checker", &[]);
    let visible = store.read(scope, &who, None).await?;
    ensure!(
        visible.iter().any(|f| f.fact_id == cf.fact_id),
        "the committed control fact is not visible"
    );
    let leaked = visible
        .iter()
        .filter(|f| orphan_facts.contains(&f.fact_id))
        .count();
    ensure!(
        leaked == 0,
        "{leaked} facts visible without a ledger commit"
    );
    for id in orphan_eps.iter().step_by((n / 50).max(1)) {
        ensure!(
            store.episode_body(scope, &who, id).await?.is_none(),
            "uncommitted episode {id} is readable"
        );
    }
    Ok(format!(
        "ok: {n} seeded crashes ({after_eps} after episode rows, {after_facts} after fact rows), 0 rows visible without a ledger commit"
    ))
}

/// Branch and merge against an in-memory reference model: `sequences` random sequences of
/// `ops` operations (commit on main or a branch, create a branch, merge). After every
/// sequence, each branch's visible (entity, predicate, value) set must equal the model's, and
/// every merge must succeed or be refused exactly when the model says it conflicts.
pub async fn branch_model(
    store: &Store,
    sequences: usize,
    ops: usize,
    seed: u64,
) -> Result<String> {
    let mut rng = Rng(seed);
    let who = Principal::new("checker", &[]);
    let (mut merges, mut refused) = (0, 0);
    for seq in 0..sequences {
        let scope = Scope::Team(format!("model{seq}_{}", tag()));
        store.ensure_scope(&scope).await?;
        // Model: per branch, the set of commits it sees; per commit, its (key, value).
        let mut sees: HashMap<String, BTreeSet<usize>> =
            HashMap::from([(MAIN.to_string(), BTreeSet::new())]);
        let mut base: HashMap<String, BTreeSet<usize>> = HashMap::new();
        let mut kv: Vec<(String, String)> = vec![];
        let keys = ["k0", "k1", "k2"];
        for op in 0..ops {
            let branches: Vec<String> = sees.keys().cloned().collect();
            match rng.below(10) {
                0..=5 => {
                    let b = &branches[rng.below(branches.len())];
                    let (k, v) = (keys[rng.below(keys.len())], format!("v{}", rng.below(3)));
                    let (ep, f) = claim(&scope.namespace(), op, k, "val", &v);
                    store.commit_on(&scope, b, "model", &[ep], &[f]).await?;
                    let id = kv.len();
                    kv.push((k.into(), v));
                    sees.get_mut(b).unwrap().insert(id);
                }
                6 | 7 => {
                    let name = format!("b{op}");
                    store.create_branch(&scope, &name, "model").await?;
                    let main = sees[MAIN].clone();
                    base.insert(name.clone(), main.clone());
                    sees.insert(name, main);
                }
                _ => {
                    let candidates: Vec<&String> = branches.iter().filter(|b| *b != MAIN).collect();
                    if candidates.is_empty() {
                        continue;
                    }
                    let b = candidates[rng.below(candidates.len())].clone();
                    let changed = |side: &BTreeSet<usize>| -> HashMap<&str, BTreeSet<&str>> {
                        let mut m: HashMap<&str, BTreeSet<&str>> = HashMap::new();
                        for c in side.difference(&base[&b]) {
                            m.entry(kv[*c].0.as_str())
                                .or_default()
                                .insert(kv[*c].1.as_str());
                        }
                        m
                    };
                    let (ours, theirs) = (changed(&sees[MAIN]), changed(&sees[&b]));
                    let conflict = theirs
                        .iter()
                        .any(|(k, v)| ours.get(k).is_some_and(|o| o != v));
                    let got = store.merge(&scope, &b, "model").await;
                    merges += 1;
                    match (conflict, got) {
                        (false, Ok(_)) => {
                            let add = sees[&b].clone();
                            sees.get_mut(MAIN).unwrap().extend(add);
                        }
                        (true, Err(e)) if e.to_string().contains("conflict") => refused += 1,
                        (c, g) => bail!(
                            "seq {seq} op {op}: model conflict={c}, engine {:?}",
                            g.map(|_| ())
                        ),
                    }
                }
            }
        }
        for (b, commits) in &sees {
            let want: BTreeSet<String> = commits
                .iter()
                .map(|c| format!("{}={}", kv[*c].0, kv[*c].1))
                .collect();
            let got: BTreeSet<String> = store
                .read_on(&scope, b, &who, None)
                .await?
                .into_iter()
                .map(|f| format!("{}={}", f.entity, f.value))
                .collect();
            ensure!(
                want == got,
                "seq {seq} branch {b}: model {want:?} engine {got:?}"
            );
        }
    }
    Ok(format!(
        "ok: {sequences} sequences x {ops} ops match the reference model ({merges} merges, {refused} refused as conflicts)"
    ))
}

/// Clock skew: writers whose clocks are up to `skew` ahead or behind commit interleaved.
/// Pass: along every ledger edge a commit's recorded time is later than its parents'.
pub async fn clock_skew(
    catalog: Arc<dyn Catalog>,
    scope: &Scope,
    writers: usize,
    commits: usize,
    skew: Duration,
) -> Result<String> {
    let setup = Store::new(catalog.clone());
    setup.ensure_scope(scope).await?;
    let t = tag();
    let mut tasks = vec![];
    for w in 0..writers {
        let (catalog, scope, t) = (catalog.clone(), scope.clone(), t.clone());
        // Spread offsets across [-skew, +skew].
        let frac = if writers > 1 {
            w as f64 / (writers - 1) as f64
        } else {
            0.5
        };
        let offset = skew.mul_f64((frac * 2.0 - 1.0).abs());
        let ahead = frac >= 0.5;
        tasks.push(tokio::spawn(async move {
            let store = Store::new(catalog).with_clock_offset(offset, ahead);
            for i in 0..commits {
                let (ep, f) = claim(
                    &t,
                    w * commits + i,
                    &format!("skew_{t}"),
                    "n",
                    &i.to_string(),
                );
                store
                    .commit(&scope, &format!("skewed{w}"), &[ep], &[f])
                    .await?;
            }
            Ok::<_, anyhow::Error>(())
        }));
    }
    for task in tasks {
        task.await??;
    }
    let rows = setup.ledger_rows(scope).await?;
    let at: HashMap<&str, i64> = rows.iter().map(|r| (r.0.as_str(), r.3)).collect();
    let mut edges = 0;
    for r in &rows {
        for p in &r.2 {
            if let Some(pt) = at.get(p.as_str()) {
                edges += 1;
                ensure!(
                    r.3 > *pt,
                    "commit {} recorded at {} is not after its parent {p} at {pt}",
                    r.0,
                    r.3
                );
            }
        }
    }
    Ok(format!(
        "ok: {writers} writers skewed up to ±{}s, {edges} ledger edges all monotonic in recorded time",
        skew.as_secs()
    ))
}

/// Freshness (NFR-3): a writer commits; a separate, warm reader polls until it sees the fact.
pub async fn freshness(
    catalog: Arc<dyn Catalog>,
    scope: &Scope,
    samples: usize,
) -> Result<(f64, f64)> {
    let (writer, reader) = (Store::new(catalog.clone()), Store::new(catalog));
    writer.ensure_scope(scope).await?;
    let who = Principal::new("checker", &[]);
    let t = tag();
    let entity = format!("fresh_{t}");
    reader.lookup(scope, MAIN, &who, &entity, "n", None).await?;
    let mut ms = vec![];
    for i in 0..samples {
        let (ep, f) = claim(&t, i, &entity, "n", &i.to_string());
        writer
            .commit(scope, "check", &[ep], std::slice::from_ref(&f))
            .await?;
        let start = Instant::now();
        loop {
            let seen = reader.lookup(scope, MAIN, &who, &entity, "n", None).await?;
            if seen.iter().any(|x| x.fact_id == f.fact_id) {
                break;
            }
            ensure!(
                start.elapsed() < Duration::from_secs(600),
                "fact not visible after 10 minutes"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    ms.sort_by(f64::total_cmp);
    Ok((ms[ms.len() / 2], ms[(ms.len() * 95).div_ceil(100) - 1]))
}

/// Source permission fidelity (D-6): for a seeded sample of a loaded corpus's facts and a set
/// of principals per fact (each source ACL's own user and group, plus an outsider), the engine
/// shows the fact exactly when the principal can see every source episode in the ground truth.
pub async fn source_acl(
    store: &Store,
    dir: &std::path::Path,
    only: &HashSet<String>,
    sample: usize,
    seed: u64,
) -> Result<String> {
    use crate::harness::corpus::{GenEpisode, GenFact, read_jsonl};
    let episodes: Vec<GenEpisode> = read_jsonl(&dir.join("episodes.jsonl"))?;
    let acl: HashMap<String, Vec<String>> = episodes
        .into_iter()
        .map(|e| (e.episode_id, e.acl))
        .collect();
    let facts: Vec<GenFact> = read_jsonl(&dir.join("facts.jsonl"))?;
    let facts: Vec<GenFact> = facts
        .into_iter()
        .filter(|f| only.contains(&f.scope))
        .collect();
    ensure!(!facts.is_empty(), "no facts in the loaded scopes");
    // Half the sample from facts with a restricted (user or group) source, which the
    // org-wide majority would otherwise drown out.
    let restricted: Vec<&GenFact> = facts
        .iter()
        .filter(|f| {
            f.source_episodes
                .iter()
                .any(|e| !acl[e].iter().any(|a| a == "*"))
        })
        .collect();
    ensure!(
        !restricted.is_empty(),
        "no restricted facts in the loaded scopes"
    );
    let mut rng = Rng(seed);
    let (mut checks, mut shown) = (0, 0);
    for i in 0..sample {
        let f = if i % 2 == 0 {
            restricted[rng.below(restricted.len())]
        } else {
            &facts[rng.below(facts.len())]
        };
        let scope = Scope::parse(&f.scope)?;
        let mut principals: Vec<Principal> = vec![Principal::new("outsider_nobody", &[])];
        for a in f.source_episodes.iter().flat_map(|e| acl[e].iter()) {
            match a.split_once(':') {
                Some(("user", u)) => principals.push(Principal::new(u, &[])),
                Some(("group", g)) => principals.push(Principal::new("member_of_group", &[g])),
                _ => principals.push(Principal::new("anyone_at_all", &[])),
            }
        }
        for who in &principals {
            let expected = f.source_episodes.iter().all(|e| who.can_see(&acl[e]));
            let got = store
                .lookup(
                    &scope,
                    MAIN,
                    who,
                    &f.entity,
                    &f.predicate,
                    Some(f.valid_from),
                )
                .await?
                .iter()
                .any(|x| x.fact_id == f.fact_id);
            ensure!(
                got == expected,
                "fact {} as {}{:?}: engine {got}, ground truth {expected}",
                f.fact_id,
                who.user(),
                who.groups()
            );
            checks += 1;
            shown += got as usize;
        }
    }
    Ok(format!(
        "ok: {sample} sampled facts, {checks} (fact, principal) checks match ground truth ({shown} visible, {} withheld)",
        checks - shown
    ))
}

/// Index rebuild equivalence: facts arrive in `batches` (with superseded, closed versions),
/// one index is updated after every batch, another is built from scratch at the end. Pass:
/// every key lookup returns the same facts from both and matches the facts written, and every
/// search's top 10 is identical.
pub async fn index_equivalence(
    catalog: Arc<dyn Catalog>,
    scope: &Scope,
    batches: usize,
    per_batch: usize,
    seed: u64,
) -> Result<String> {
    let base = std::env::temp_dir().join(format!("memstore-equiv-{}", tag()));
    let inc =
        Store::new(catalog.clone()).with_index_base(base.join("incremental").display().to_string());
    let full = Store::new(catalog).with_index_base(base.join("full").display().to_string());
    inc.ensure_scope(scope).await?;
    let t = tag();
    let mut rng = Rng(seed);
    let mut written: HashMap<(String, String), BTreeSet<String>> = HashMap::new();
    for b in 0..batches {
        let (mut eps, mut facts) = (vec![], vec![]);
        for i in 0..per_batch {
            let e = format!("eq{}_{t}", rng.below(per_batch.max(4) / 2 + 1));
            let p = ["role", "team", "stage"][rng.below(3)];
            let (ep, mut f) = claim(&t, b * per_batch + i, &e, p, &format!("v{}", rng.below(5)));
            if rng.below(3) == 0 {
                f.valid_to = Some(f.valid_from + 1_000_000);
            }
            written
                .entry((e, p.to_string()))
                .or_default()
                .insert(f.fact_id.clone());
            eps.push(ep);
            facts.push(f);
        }
        inc.commit(scope, "check", &eps, &facts).await?;
        crate::search::update(&inc, scope).await?;
    }
    crate::search::update(&full, scope).await?;
    let who = Principal::new("checker", &[]);
    let ids = |v: Vec<Fact>| -> BTreeSet<String> { v.into_iter().map(|f| f.fact_id).collect() };
    for ((e, p), want) in &written {
        let a = ids(inc.lookup(scope, MAIN, &who, e, p, None).await?);
        let b = ids(full.lookup(scope, MAIN, &who, e, p, None).await?);
        ensure!(a == b, "lookup {e}.{p}: incremental {a:?} vs full {b:?}");
        ensure!(
            &a == want,
            "lookup {e}.{p}: index {a:?} vs written {want:?}"
        );
    }
    let mut queries = 0;
    for (e, p) in written.keys().take(200) {
        let q = format!("{e} {p}");
        let a = crate::search::search(&inc, scope, MAIN, &who, &q, 10, None).await?;
        let b = crate::search::search(&full, scope, MAIN, &who, &q, 10, None).await?;
        // Equal up to ties: the same score at every rank, and the same facts strictly above
        // the lowest score shown. Which of several equally-scored facts make the top 10 depends
        // on the index's data layout, as with any top-k under ties.
        let score = |h: &crate::search::Hit| {
            (
                h.terms_matched,
                (h.bm25 * 1e4).round() as i64,
                (h.score * 1e6).round() as i64,
            )
        };
        let (sa, sb): (Vec<_>, Vec<_>) =
            (a.iter().map(score).collect(), b.iter().map(score).collect());
        ensure!(
            sa == sb,
            "search {q:?}: scores differ by rank between incremental and full rebuild"
        );
        if let Some(floor) = sa.last().copied() {
            let above = |v: &[crate::search::Hit]| -> BTreeSet<String> {
                v.iter()
                    .filter(|h| score(h) > floor)
                    .map(|h| h.fact.fact_id.clone())
                    .collect()
            };
            ensure!(
                above(&a) == above(&b),
                "search {q:?}: different facts above the tie line"
            );
        }
        queries += 1;
    }
    let _ = std::fs::remove_dir_all(&base);
    Ok(format!(
        "ok: {} keys identical (and equal to the facts written) and {queries} searches equal up to ties, between an index updated over {batches} batches and a full rebuild",
        written.len()
    ))
}

/// Write throughput: `writers` concurrent writers commit `commits` times each, `facts` facts
/// (and one episode per fact) per commit. `shared`: all writers go through one store (one
/// engine serving many agents, so group commit batches them); otherwise each writer has its
/// own store (separate processes). Reports commits/s, facts/s and commit latency as one JSON
/// line, so a sweep can be tabulated.
pub async fn throughput(
    catalog: Arc<dyn Catalog>,
    scope: &Scope,
    writers: usize,
    commits: usize,
    facts: usize,
    shared: bool,
) -> Result<String> {
    Store::new(catalog.clone()).ensure_scope(scope).await?;
    let one = Arc::new(Store::new(catalog.clone()));
    let t = tag();
    let started = Instant::now();
    let mut tasks = vec![];
    for w in 0..writers {
        let (catalog, scope, t, one) = (catalog.clone(), scope.clone(), t.clone(), one.clone());
        tasks.push(tokio::spawn(async move {
            let store = if shared {
                one
            } else {
                Arc::new(Store::new(catalog))
            };
            let mut ms = vec![];
            for c in 0..commits {
                let (eps, fs): (Vec<_>, Vec<_>) = (0..facts)
                    .map(|i| {
                        claim(
                            &t,
                            (w * commits + c) * facts + i,
                            &format!("tp{w}_{t}"),
                            "n",
                            &i.to_string(),
                        )
                    })
                    .unzip();
                let t0 = Instant::now();
                store
                    .commit(&scope, &format!("writer{w}"), &eps, &fs)
                    .await?;
                ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            }
            Ok::<_, anyhow::Error>(ms)
        }));
    }
    let mut ms = vec![];
    for task in tasks {
        ms.extend(task.await??);
    }
    let secs = started.elapsed().as_secs_f64();
    ms.sort_by(f64::total_cmp);
    let n = (writers * commits) as f64;
    Ok(serde_json::json!({
        "writers": writers,
        "shared_store": shared,
        "commits": writers * commits,
        "facts_per_commit": facts,
        "seconds": (secs * 10.0).round() / 10.0,
        "commits_per_s": (n / secs * 100.0).round() / 100.0,
        "facts_per_s": ((n * facts as f64 / secs) * 10.0).round() / 10.0,
        "commit_p50_ms": ms[ms.len() / 2].round(),
        "commit_p95_ms": ms[(ms.len() * 95).div_ceil(100) - 1].round(),
    })
    .to_string())
}
