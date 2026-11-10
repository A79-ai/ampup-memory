//! Per-scope compile settings (`compile_batch`, `compile_kind`), on real compiles against a stand-in model that
//! records every prompt, answers each C2S round with one skill per source and each AIP call with a graph.
//!
//! A scope that sets neither compiles as it did before they existed: the same prompt, under the same compiler id.
//! The id is pinned here as a deployed library is compiled under it (`compile:<sources hash>:<model>@<prompt hash>`):
//! a change to it compiles every such library's history again, so it changes only on purpose.
//! A team that sets both compiles in its own batches with the error-report line, under an id of its own; a team
//! that sets neither takes its org's batch size, which changes no id. Each setting is refused out of range and
//! unset by its empty value.

use std::sync::{Arc, Mutex};

use memstore::compile::{self, Kind, Options};
use memstore::people::directory::{self, Actor, Change, Directory, Settings};
use memstore::{Episode, Scope};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The id an org's skills carry: its sources (`github:acme/app#`, `linear:acme/`), its models, and the prompts'
/// hash before compile settings existed.
const PINNED: &str = "compile:2fb52ee4:qwen/qwen3.7-plus+aip=anthropic/claude-haiku-5.5@c9c188c4";
/// What `compile_kind: errors` adds to the prompt.
const ERRORS_LINE: &str = "the sources are error reports";

/// A model API (OpenAI-compatible) that records each prompt and answers a C2S round with one skill per source
/// handle (its slug from the source's name) and an AIP call with a valid graph.
async fn model(seen: Arc<Mutex<Vec<String>>>) -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = vec![];
                let mut chunk = [0u8; 8192];
                let (start, len) = loop {
                    let n = conn.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                        break (i + 4, len);
                    }
                };
                while buf.len() < start + len {
                    let n = conn.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let req: Value = serde_json::from_slice(&buf[start..start + len]).unwrap();
                let prompt = req["messages"][0]["content"].as_str().unwrap().to_string();
                seen.lock().unwrap().push(prompt.clone());
                let reply = if prompt.starts_with("Compile this skill") {
                    json!({"purpose": "fix it", "trigger_when": ["it breaks"], "do_not_use_when": [],
                           "steps": [{"name": "fix-it", "description": "fix it", "inputs": [], "outputs": []}]})
                } else {
                    // Each source is "[SRCn]" then "(<its source name>)".
                    let lines: Vec<&str> = prompt.lines().collect();
                    let skills: Vec<Value> = lines
                        .windows(2)
                        .filter_map(|w| {
                            let handle = w[0].strip_prefix('[')?.strip_suffix(']')?;
                            let name = w[1].strip_prefix('(')?.strip_suffix(')')?;
                            let tag: String = name
                                .chars()
                                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                                .collect();
                            let slug = format!("fix-{}", tag.trim_matches('-').to_lowercase());
                            Some(json!({"slug": slug, "title": slug, "when": format!("{name} breaks"),
                                        "steps": ["fix it"], "if": [], "sources": [handle]}))
                        })
                        .collect();
                    json!({"skills": skills})
                }
                .to_string();
                let body = json!({
                    "choices": [{"message": {"content": reply}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": prompt.len() / 4, "completion_tokens": reply.len() / 4}
                })
                .to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                conn.write_all(head.as_bytes()).await.unwrap();
                conn.write_all(body.as_bytes()).await.unwrap();
                conn.shutdown().await.ok();
            });
        }
    });
    url
}

/// `n` sources named `<prefix><k>`, a day apart.
fn sources(prefix: &str, n: usize) -> Vec<Episode> {
    (1..=n)
        .map(|k| {
            let mut e = Episode::new(
                &format!("{prefix}{k}"),
                format!("{prefix}{k}: something broke and was fixed").as_bytes(),
                &["*"],
            );
            e.occurred_at = 1_790_000_000_000_000 + k as i64 * 86_400_000_000;
            e
        })
        .collect()
}

/// The C2S prompts sent so far, one per round.
fn rounds(seen: &Mutex<Vec<String>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|p| p.starts_with("You maintain the skill library"))
        .cloned()
        .collect()
}

#[tokio::test]
async fn compile_settings_change_only_the_scopes_that_set_them() {
    if !crate::own_process(
        "compile_settings::compile_settings_change_only_the_scopes_that_set_them",
    ) {
        return;
    }
    let seen = Arc::new(Mutex::new(vec![]));
    let url = model(seen.clone()).await;
    // SAFETY: this test runs alone in its process (`own_process`).
    unsafe {
        std::env::set_var("MEMSTORE_MODEL_CACHE", "off");
        std::env::set_var("MEMSTORE_COMPILE", "openai");
        std::env::set_var("MEMSTORE_COMPILE_URL", &url);
        std::env::set_var("MEMSTORE_COMPILE_MODEL", "qwen/qwen3.7-plus");
        std::env::remove_var("MEMSTORE_COMPILE_BATCH");
        std::env::remove_var("MEMSTORE_AIP_MODEL");
    }
    let cx = memstore::extract::Extractor::compile_from_env()
        .unwrap()
        .unwrap()
        .with_aip_model("anthropic/claude-haiku-5.5");
    let dir = tempfile::tempdir().unwrap();
    let store = crate::local_store(dir.path()).await;
    let operator = Actor {
        user: "ops".into(),
        superuser: true,
        ..Actor::default()
    };
    let admin = Actor {
        user: "alice".into(),
        ..Actor::default()
    };
    let set = |team: Option<&str>, settings: Settings| Change::SetSettings {
        org: "acme".into(),
        team: team.map(String::from),
        settings,
    };
    for c in [
        Change::CreateOrg {
            org: "acme".into(),
            admin: Some("alice".into()),
            auth0_org: None,
        },
        Change::CreateTeam {
            org: "acme".into(),
            team: "eng".into(),
        },
        Change::CreateTeam {
            org: "acme".into(),
            team: "ops".into(),
        },
    ] {
        directory::apply(&store, &operator, &c).await.unwrap();
    }
    let (acme, eng, ops) = (
        Scope::org("acme").unwrap(),
        Scope::team("acme", "eng").unwrap(),
        Scope::team("acme", "ops").unwrap(),
    );
    let org_sources = vec!["github:acme/app#".to_string(), "linear:acme/".to_string()];

    // Org acme sets nothing yet: its options are the default, its id the pinned one.
    let d = Directory::load(&store).await.unwrap();
    assert_eq!(compile::options_for(Some(&d), &acme), Options::default());
    assert_eq!(compile::options_for(None, &acme), Options::default());
    assert_eq!(compile::compiler_id(&org_sources, &cx), PINNED);
    assert_eq!(
        compile::compiler_id_with(&org_sources, &cx, &Options::default()),
        PINNED
    );

    // The org's admin sets the org's batch; eng sets its own and the error-report kind; ops sets nothing.
    for (team, s) in [
        (
            None,
            Settings {
                compile_batch: Some(4),
                ..Settings::default()
            },
        ),
        (
            Some("eng"),
            Settings {
                compile_batch: Some(2),
                compile_kind: Some("errors".into()),
                ..Settings::default()
            },
        ),
    ] {
        directory::apply(&store, &admin, &set(team, s))
            .await
            .unwrap();
    }
    let d = Directory::load(&store).await.unwrap();
    let org_opts = Options {
        batch: Some(4),
        kind: None,
    };
    let eng_opts = Options {
        batch: Some(2),
        kind: Some(Kind::Errors),
    };
    assert_eq!(compile::options_for(Some(&d), &acme), org_opts);
    assert_eq!(compile::options_for(Some(&d), &ops), org_opts);
    assert_eq!(compile::options_for(Some(&d), &eng), eng_opts);
    // A batch size changes no id; a kind does.
    assert_eq!(
        compile::compiler_id_with(&org_sources, &cx, &org_opts),
        PINNED
    );
    let sentry = vec!["sentry:".to_string()];
    let eng_id = compile::compiler_id_with(&sentry, &cx, &eng_opts);
    assert_ne!(eng_id, compile::compiler_id(&sentry, &cx));
    assert_ne!(&eng_id[eng_id.len() - 8..], &PINNED[PINNED.len() - 8..]);

    // The org compiles its 5 PRs and tickets in batches of 4, with the prompt as it was, under the pinned id.
    let mut org_eps = sources("github:acme/app#", 3);
    org_eps.extend(sources("linear:acme/ENG-", 2));
    store.ensure_scope(&acme).await.unwrap();
    store.commit(&acme, "test", &org_eps, &[]).await.unwrap();
    let r = compile::run_with(
        &store,
        &acme,
        memstore::MAIN,
        &org_sources,
        &cx,
        &compile::options_for(Some(&d), &acme),
        100,
    )
    .await
    .unwrap();
    assert_eq!((r.episodes, r.skills, r.failed), (5, 5, 0), "{r:?}");
    let org_rounds = rounds(&seen);
    assert_eq!(org_rounds.len(), 2, "5 sources in batches of 4");
    assert!(org_rounds.iter().all(|p| !p.contains(ERRORS_LINE)));
    let read = store.extracted(&acme, PINNED).await.unwrap();
    assert!(
        org_eps.iter().all(|e| read.contains(&e.episode_id)),
        "the org's sources are recorded under the pinned id"
    );

    // Eng compiles its 5 Sentry issues in batches of 2, each round's prompt with the error-report line, under its
    // own id; nothing of eng's is recorded under the id an unset scope would have.
    let eng_eps = sources("sentry:https://acme.sentry.io/issues/", 5);
    store.ensure_scope(&eng).await.unwrap();
    store.commit(&eng, "test", &eng_eps, &[]).await.unwrap();
    let r = compile::run_with(
        &store,
        &eng,
        memstore::MAIN,
        &sentry,
        &cx,
        &compile::options_for(Some(&d), &eng),
        100,
    )
    .await
    .unwrap();
    assert_eq!((r.episodes, r.skills, r.failed), (5, 5, 0), "{r:?}");
    let eng_rounds = rounds(&seen)[org_rounds.len()..].to_vec();
    assert!(eng_rounds.iter().all(|p| p.contains("(sentry:")));
    assert_eq!(eng_rounds.len(), 3, "5 sources in batches of 2");
    for p in &eng_rounds {
        let when = p.find("- `when`:").unwrap();
        let errors = p.find(ERRORS_LINE).expect("the error-report line");
        assert!(errors > when && p[when..errors].lines().count() == 2, "{p}");
    }
    let read = store.extracted(&eng, &eng_id).await.unwrap();
    assert!(eng_eps.iter().all(|e| read.contains(&e.episode_id)));
    assert!(
        store
            .extracted(&eng, &compile::compiler_id(&sentry, &cx))
            .await
            .unwrap()
            .is_empty()
    );
    // But for the error-report line, eng's instructions are the ones every other scope compiles with.
    let rules = |p: &str| {
        let from = p.find("Update the library from the new sources").unwrap();
        let to = p.find("Reply with JSON only").unwrap();
        p[from..to].to_string()
    };
    let (plain, errs) = (rules(&org_rounds[0]), rules(&eng_rounds[0]));
    let line = errs.lines().find(|l| l.contains(ERRORS_LINE)).unwrap();
    assert_eq!(errs.replacen(&format!("{line}\n"), "", 1), plain);

    // Out of range, refused; the empty values unset them, and eng is back to the org's batch and no kind.
    for bad in [
        Settings {
            compile_kind: Some("warnings".into()),
            ..Settings::default()
        },
        Settings {
            compile_batch: Some(101),
            ..Settings::default()
        },
    ] {
        let e = directory::apply(&store, &admin, &set(Some("eng"), bad))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("compile_"), "{e:#}");
    }
    let unset = Settings {
        compile_batch: Some(0),
        compile_kind: Some(String::new()),
        ..Settings::default()
    };
    directory::apply(&store, &admin, &set(Some("eng"), unset))
        .await
        .unwrap();
    let d = Directory::load(&store).await.unwrap();
    assert_eq!(compile::options_for(Some(&d), &eng), org_opts);
}
