//! A real server and persisted local catalog: ordinary writes cannot bypass an entity owner,
//! and an authenticated owner publishes only the immutable revision they reviewed.

use crate::isolation::{ADMIN_KEY, CLIENT_ID, admin, id_token, post, start_idp};
use memstore::auth::{self, OidcConfig, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{Episode, Fact, Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;
use std::slice::from_ref;
use std::time::{Duration, Instant};

struct Env {
    _dir: tempfile::TempDir,
    store: Store,
    scope: Scope,
    url: String,
    alice: String,
    bob: String,
    carol: String,
    foreign: String,
}

impl Env {
    async fn start() -> Self {
        Self::open(tempfile::tempdir().unwrap(), true).await
    }

    async fn open(dir: tempfile::TempDir, initialize: bool) -> Self {
        let catalog = platform::local_dir(dir.path()).await.unwrap();
        let store = Store::new(catalog.clone())
            .with_index_base(dir.path().join("index").display().to_string());
        store.ensure_scope(&Scope::Org).await.unwrap();
        let issuer = start_idp().await;
        let keys = json!([{"id":"platform","sha256":auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes())),"user":"svc:platform","admin":true}]);
        let auth = TokenAuth::new(Some(&keys.to_string()), None)
            .unwrap()
            .allow_local_issuers()
            .with_oidc(
                OidcConfig::discover(&issuer, vec![CLIENT_ID.into()])
                    .await
                    .unwrap(),
            );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_with(
            port,
            Scope::Org,
            vec![],
            Options {
                listener: Some(listener),
                catalog: Some(catalog),
                index_base: Some(store.index_base().to_string()),
                token_auth: Some(auth),
                ..Default::default()
            },
        ));
        let env = Self {
            _dir: dir,
            store,
            scope: Scope::org("acme").unwrap(),
            url: format!("http://127.0.0.1:{port}"),
            alice: id_token(&issuer, "alice@acme.com"),
            bob: id_token(&issuer, "bob@acme.com"),
            carol: id_token(&issuer, "carol@acme.com"),
            foreign: id_token(&issuer, "eve@other.com"),
        };
        let start = Instant::now();
        loop {
            if reqwest::get(format!("{}/health", env.url)).await.is_ok() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(30));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if initialize {
            for change in [
                json!({"op":"create_org","org":"acme","admin":"alice@acme.com"}),
                json!({"op":"invite","org":"acme","members":[{"user":"bob@acme.com"},{"user":"carol@acme.com"}]}),
                json!({"op":"create_org","org":"other","admin":"eve@other.com"}),
                json!({"op":"set_schema","org":"acme","schema":{"name":"policies","types":{"policy":{"predicates":{"refund_days":{"type":"string"},"applies_to":{"type":"ref","to":["policy"]}}}}}}),
            ] {
                admin(&env.url, change).await;
            }
        }
        env
    }

    async fn call(&self, token: &str, mut req: Value) -> (u16, Value) {
        if req["org"].is_null() {
            req["org"] = "acme".into();
        }
        let (status, body) = post(&format!("{}/admin", self.url), token, &req).await;
        (
            status,
            serde_json::from_str(&body).unwrap_or(json!({"raw":body})),
        )
    }

    async fn ok(&self, token: &str, req: Value) -> Value {
        let (status, body) = self.call(token, req.clone()).await;
        assert_eq!(status, 200, "{req}: {body}");
        body["result"].clone()
    }

    async fn seed(&self, entity: &str, days: &str) -> (Episode, Fact) {
        let ep = Episode::sourced(
            &format!("policy:{entity}:{days}"),
            format!("{entity} refund days {days}").as_bytes(),
            &["group:acme.all"],
        );
        let f = Fact::new(entity, "refund_days", days).from_episodes(&[&ep]);
        self.store
            .commit(&self.scope, "seed", from_ref(&ep), from_ref(&f))
            .await
            .unwrap();
        (ep, f)
    }

    async fn draft(&self, branch: &str, entity: &str, days: &str) -> (Episode, Fact) {
        self.store
            .create_branch(&self.scope, branch, "agent")
            .await
            .unwrap();
        let ep = Episode::sourced(
            &format!("policy:{branch}"),
            format!("{entity} refund days {days}").as_bytes(),
            &["group:acme.all"],
        );
        let f = Fact::new(entity, "refund_days", days).from_episodes(&[&ep]);
        self.store
            .commit_on(&self.scope, branch, "agent", from_ref(&ep), from_ref(&f))
            .await
            .unwrap();
        (ep, f)
    }

    async fn proposed(&self, branch: &str) -> Value {
        self.ok(&self.bob, json!({"op":"propose_branch","branch":branch}))
            .await
    }

    async fn decide(&self, token: &str, proposal: &Value, decision: &str) -> (u16, Value) {
        self.call(token,json!({"op":"review_proposal","proposal":proposal["proposal"]["id"],"digest":proposal["proposal"]["digest"],"decision":decision})).await
    }

    async fn reads(&self, wanted: &str, forbidden: &str) {
        let start = Instant::now();
        loop {
            let (status, body) = post(
                &format!("{}/query?org=acme", self.url),
                &self.bob,
                &json!({"data":[[0,{"find":"facts","limit":500}]]}),
            )
            .await;
            assert_eq!(status, 200, "{body}");
            if body.contains(wanted) && !body.contains(forbidden) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "wanted {wanted}, not {forbidden}: {body}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_owner_publishes_the_reviewed_revision() {
    let e = Env::start().await;
    let entity = "policy:refund-window";
    let (_, old) = e.seed(entity, "thirty").await;
    let owner = e
        .ok(
            &e.alice,
            json!({"op":"set_entity_owner","entity":entity,"owner":"bob@acme.com"}),
        )
        .await;
    assert_eq!(owner["user"], "bob@acme.com");
    let (ep, new) = e.draft("refund-update", entity, "fourteen").await;
    // Both the ordinary grouped writer and the separate merge path must refuse an owner actor string.
    let denied = e
        .store
        .commit(&e.scope, "bob@acme.com", from_ref(&ep), from_ref(&new))
        .await
        .unwrap_err();
    assert!(
        denied.to_string().contains("owner approval required"),
        "{denied:#}"
    );
    assert!(
        e.store
            .merge(&e.scope, "refund-update", "memstore:session-merge")
            .await
            .is_err()
    );
    e.reads(&old.fact_id, &new.fact_id).await;
    let p = e.proposed("refund-update").await;
    assert_eq!(p["status"], "pending_review");
    assert_eq!(
        e.proposed("refund-update").await["proposal"]["id"],
        p["proposal"]["id"]
    );
    // Neither a member who isn't its owner nor another org's admin decides it (an admin of its org may: below).
    for token in [&e.carol, &e.foreign] {
        let (status, body) = e.decide(token, &p, "approve").await;
        assert_eq!(status, 403, "{body}");
    }
    // The inbox shows the proposal to a member who may read its evidence, with what it changes and the
    // value main holds now; the graph marks the owned entity.
    let inbox = e.ok(&e.carol, json!({"op":"list_proposals"})).await;
    let row = &inbox["proposals"][0];
    assert_eq!(row["id"], p["proposal"]["id"]);
    assert_eq!(row["status"], "pending_review");
    assert_eq!(row["entities"], json!([entity]));
    assert_eq!(row["required_owners"], json!(["bob@acme.com"]));
    assert!(row["created_at"].as_i64().unwrap() > 0, "{row}");
    assert_eq!(p["proposal"]["changes"][0]["before"], "thirty");
    assert_eq!(p["proposal"]["changes"][0]["value"], "fourteen");
    assert_eq!(p["evidence_details"][0]["readable"], true);
    assert_eq!(
        e.ok(
            &e.carol,
            json!({"op":"list_proposals","entity":"policy:other"})
        )
        .await["proposals"],
        json!([])
    );
    let graph = e.ok(&e.carol, json!({"op":"entity_graph"})).await;
    let node = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == entity)
        .unwrap();
    assert_eq!(node["owner"], "bob@acme.com");
    assert_eq!(node["pending"], json!([p["proposal"]["id"]]));
    let (status, body) = e.decide(&e.bob, &p, "approve").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["status"], "published");
    assert!(
        body["result"]["published_at"].as_i64().unwrap() > 0,
        "{body}"
    );
    assert_eq!(body["result"]["decisions"][0]["user"], "bob@acme.com");
    assert_eq!(body["result"]["decisions"][0]["approve"], true);
    assert!(body["result"]["decisions"][0]["at"].as_i64().unwrap() > 0);
    e.reads(&new.fact_id, &old.fact_id).await;
    // Retrying after a lost response is idempotent.
    let before = e.store.branch_heads(&e.scope).await.unwrap();
    assert_eq!(e.decide(&e.bob, &p, "approve").await.0, 200);
    assert_eq!(e.store.branch_heads(&e.scope).await.unwrap(), before);
    // A fresh Store (cold caches) reads ownership and decisions from the same persisted catalog.
    let reopened = Store::new(e.store.catalog()).with_index_base(e.store.index_base().to_string());
    assert_eq!(
        reopened.entity_owners(&e.scope).await.unwrap()[0].user,
        "bob@acme.com"
    );
    let review = reopened
        .proposal(
            &e.scope,
            p["proposal"]["id"].as_str().unwrap(),
            &memstore::Principal::new("bob@acme.com", &["acme.all"]),
        )
        .await
        .unwrap();
    assert_eq!(review.status, "published");
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_rejections_and_transfers_cannot_reuse_approval() {
    let e = Env::start().await;
    let entity = "policy:refund-window";
    e.seed(entity, "thirty").await;
    let owner = e
        .ok(
            &e.alice,
            json!({"op":"set_entity_owner","entity":entity,"owner":"bob@acme.com"}),
        )
        .await;
    let (ep, _) = e.draft("edit", entity, "fourteen").await;
    let p = e.proposed("edit").await;
    let changed = Fact::new(entity, "refund_days", "seven").from_episodes(&[&ep]);
    e.store
        .commit_on(&e.scope, "edit", "agent", &[], &[changed])
        .await
        .unwrap();
    assert_eq!(e.decide(&e.bob, &p, "approve").await.0, 403);
    let stale = e
        .ok(
            &e.carol,
            json!({"op":"get_proposal","proposal":p["proposal"]["id"]}),
        )
        .await;
    assert_eq!(stale["status"], "stale");
    assert_eq!(stale["stale_reason"], "draft_changed");
    let p = e.proposed("edit").await;
    let (status, body) = e
        .call(
            &e.bob,
            json!({"op":"review_proposal","proposal":p["proposal"]["id"],"digest":p["proposal"]["digest"],"decision":"reject","note":"Seven is the pilot's number, not policy."}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["status"], "rejected");
    assert_eq!(
        body["result"]["decisions"][0]["note"],
        "Seven is the pilot's number, not policy."
    );
    assert_eq!(
        e.ok(&e.carol, json!({"op":"list_proposals"})).await["proposals"][0]["rejected_by"],
        "bob@acme.com"
    );
    assert_eq!(e.decide(&e.bob, &p, "approve").await.0, 403);
    assert!(
        e.store
            .merge(&e.scope, "edit", "bob@acme.com")
            .await
            .is_err()
    );
    assert_eq!(e.proposed("edit").await["status"], "rejected");
    e.draft("transfer", entity, "twenty").await;
    let transfer = e.proposed("transfer").await;
    let change = json!({"op":"set_entity_owner","entity":entity,"owner":"carol@acme.com","revision":owner["revision"]});
    assert_eq!(e.call(&e.alice, change.clone()).await.0, 403);
    e.ok(&e.bob, change.clone()).await;
    assert_eq!(e.decide(&e.bob, &transfer, "approve").await.0, 403);
    assert_eq!(e.call(&e.bob, change).await.0, 403);
    let current = e.proposed("transfer").await;
    assert_eq!(e.decide(&e.carol, &current, "approve").await.0, 200);
    // Removing the owner: not a member who is neither owner nor admin, not with a stale revision; the
    // admin may, and afterwards the entity's changes publish without review again.
    let owners = e.ok(&e.alice, json!({"op":"entity_owners"})).await;
    let revision = owners[0]["revision"].clone();
    assert!(owners[0]["since"].as_i64().unwrap() > 0, "{owners}");
    let remove = json!({"op":"remove_entity_owner","entity":entity,"revision":revision});
    assert_eq!(e.call(&e.bob, remove.clone()).await.0, 403);
    assert_eq!(
        e.call(
            &e.alice,
            json!({"op":"remove_entity_owner","entity":entity,"revision":"stale"})
        )
        .await
        .0,
        403
    );
    e.ok(&e.alice, remove).await;
    assert_eq!(
        e.ok(&e.alice, json!({"op":"entity_owners"})).await,
        json!([])
    );
    let (ep, free) = e.draft("unowned", entity, "ninety").await;
    e.store
        .commit(&e.scope, "agent", from_ref(&ep), from_ref(&free))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn protected_side_writes_and_multi_owner_changes_are_gated() {
    let e = Env::start().await;
    let (old, old_fact) = e.seed("policy:refund-window", "thirty").await;
    e.seed("policy:returns", "sixty").await;
    for (entity, owner) in [
        ("policy:refund-window", "bob@acme.com"),
        ("policy:returns", "carol@acme.com"),
    ] {
        e.ok(
            &e.alice,
            json!({"op":"set_entity_owner","entity":entity,"owner":owner}),
        )
        .await;
    }
    let close = [(old_fact.fact_id.as_str(), memstore::now_micros(), &old)];
    // The exact signature is the normal closure API, not a governance-specific test hook.
    assert!(
        e.store
            .close_facts(&e.scope, memstore::MAIN, "agent", &close)
            .await
            .is_err()
    );
    let schema = e
        .store
        .schema(&e.scope, memstore::MAIN)
        .await
        .unwrap()
        .unwrap();
    assert!(
        e.store
            .set_schema(&e.scope, memstore::MAIN, "bob@acme.com", &schema, false)
            .await
            .is_err()
    );
    // A shared document waits for the owners of what it touches (an owned entity it is about), not for every owner
    // in the scope: advice that names no owned entity lands.
    let note = Episode::new("advice:learn", b"refund advice", &["group:acme.all"])
        .version_of("advice/refunds")
        .with_about(&["policy:refund-window"]);
    assert!(
        e.store
            .commit(&e.scope, "advice:learn", &[note], &[])
            .await
            .is_err()
    );
    let unrelated = Episode::new("advice:learn", b"review advice", &["group:acme.all"])
        .version_of("advice/code-review");
    e.store
        .commit(&e.scope, "advice:learn", &[unrelated], &[])
        .await
        .unwrap();
    // An unrelated fact remains writable in the same scope, but a new incoming relation is governed.
    let (shipping, _) = e.seed("policy:shipping", "five").await;
    let relation = Fact::new("policy:shipping", "applies_to", "policy:refund-window")
        .from_episodes(&[&shipping]);
    let denied = e
        .store
        .commit(&e.scope, "agent", &[], &[relation])
        .await
        .unwrap_err();
    assert!(
        denied.to_string().contains("owner approval required"),
        "{denied:#}"
    );
    let copied = Fact::new("policy:copy", "refund_days", "thirty").from_episodes(&[&old]);
    let denied = e
        .store
        .commit(&e.scope, "agent", &[], &[copied])
        .await
        .unwrap_err();
    assert!(
        denied.to_string().contains("owner approval required"),
        "{denied:#}"
    );
    let denied = e
        .store
        .merge_subjects(&e.scope, "agent", "policy:refund-window", "policy:shipping")
        .await
        .unwrap_err();
    assert!(
        denied.to_string().contains("ownership migration"),
        "{denied:#}"
    );
    let (ep, _) = e.draft("both", "policy:refund-window", "fourteen").await;
    let second = Fact::new("policy:returns", "refund_days", "twenty").from_episodes(&[&ep]);
    e.store
        .commit_on(&e.scope, "both", "agent", &[], &[second])
        .await
        .unwrap();
    let p = e.proposed("both").await;
    assert_eq!(
        p["proposal"]["required_owners"].as_array().unwrap().len(),
        2
    );
    let (status, body) = e.decide(&e.bob, &p, "approve").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["status"], "pending_review");
    assert!(
        e.store
            .merge(&e.scope, "both", "alice@acme.com")
            .await
            .is_err()
    );
    let (status, body) = e.decide(&e.carol, &p, "approve").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["status"], "published");
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_bases_require_a_new_review_and_private_evidence_none_until_shared() {
    let e = Env::start().await;
    let entity = "policy:refund-window";
    let (before, _) = e.seed(entity, "thirty").await;
    e.ok(
        &e.alice,
        json!({"op":"set_entity_owner","entity":entity,"owner":"bob@acme.com"}),
    )
    .await;
    e.draft("stale-base", entity, "fourteen").await;
    let p = e.proposed("stale-base").await;
    // Main moving on elsewhere (a busy main: extraction commits every few seconds) leaves the review current.
    e.seed("policy:shipping", "five").await;
    let still = e
        .ok(
            &e.bob,
            json!({"op":"get_proposal","proposal":p["proposal"]["id"]}),
        )
        .await;
    assert_eq!(still["status"], "pending_review", "{still}");
    // Changing who reads what the owner reviewed as "before" voids it.
    e.store
        .change_acl(
            &e.scope,
            "source",
            &memstore::AclTarget::Episode(before.episode_id.clone()),
            &["user:bob@acme.com".into(), "user:alice@acme.com".into()],
        )
        .await
        .unwrap();
    assert_eq!(e.decide(&e.bob, &p, "approve").await.0, 403);
    let fresh = e.proposed("stale-base").await;
    assert_ne!(fresh["proposal"]["digest"], p["proposal"]["digest"]);
    assert_eq!(e.decide(&e.bob, &fresh, "approve").await.0, 200);

    e.store
        .create_branch(&e.scope, "private", "agent")
        .await
        .unwrap();
    let private = Episode::new(
        "private:note",
        b"private refund policy",
        &["user:carol@acme.com"],
    );
    let private_id = private.episode_id.clone();
    let fact = Fact::new(entity, "refund_days", "xprivate-value").from_episodes(&[&private]);
    e.store
        .commit_on(&e.scope, "private", "agent", &[private], &[fact])
        .await
        .unwrap();
    // Only Carol reads it, so it changes nothing Bob or anyone else reads: nothing for him to review (he couldn't
    // read its evidence), and it merges as it is.
    let (status, body) = e
        .call(&e.bob, json!({"op":"propose_branch","branch":"private"}))
        .await;
    assert_ne!(status, 200, "{body}");
    assert!(!body.to_string().contains("xprivate-value"), "{body}");
    e.store.merge(&e.scope, "private", "agent").await.unwrap();
    let read = |token: String| {
        let url = format!("{}/browse", e.url);
        async move {
            post(
                &url,
                &token,
                &json!({"org":"acme","op":"ls","path":["entities","policy","refund-window"]}),
            )
            .await
            .1
        }
    };
    // The server's reads catch up with a commit made beside it within a few seconds.
    let start = Instant::now();
    let mut hers = read(e.carol.clone()).await;
    while !hers.contains("xprivate-value") && start.elapsed() < Duration::from_secs(20) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        hers = read(e.carol.clone()).await;
    }
    assert!(hers.contains("xprivate-value"), "{hers}");
    assert!(!read(e.bob.clone()).await.contains("xprivate-value"));
    let theirs = e.ok(&e.bob, json!({"op":"list_proposals"})).await;
    assert!(!theirs.to_string().contains("xprivate-value"), "{theirs}");
    // Adding an entry no one holds lets no one else read it, so it stays immediate, and the note stays Carol's.
    e.store
        .change_acl(
            &e.scope,
            "source",
            &memstore::AclTarget::Episode(private_id.clone()),
            &["user:carol@acme.com".into(), memstore::FORGOTTEN.into()],
        )
        .await
        .unwrap();
    // Letting everyone read the note would publish that fact to them: it waits for Bob, refused here.
    let widen = e
        .store
        .change_acl(
            &e.scope,
            "source",
            &memstore::AclTarget::Episode(private_id.clone()),
            &["group:acme.all".into()],
        )
        .await
        .unwrap_err();
    assert!(
        memstore::governance::OwnerApprovalRequired::caused(&widen),
        "{widen:#}"
    );
    assert!(!read(e.bob.clone()).await.contains("xprivate-value"));
    // Handing it to another reader (a record reassigned at its source) would show Bob a fact no owner approved
    // (here Bob is the owner, but he didn't write it): his read waits, while Carol loses it at once.
    let handed = e
        .store
        .change_acl(
            &e.scope,
            "source",
            &memstore::AclTarget::Episode(private_id.clone()),
            &["user:bob@acme.com".into()],
        )
        .await
        .unwrap_err();
    assert!(
        memstore::governance::OwnerApprovalRequired::caused(&handed),
        "{handed:#}"
    );
    let start = Instant::now();
    while read(e.carol.clone()).await.contains("xprivate-value") {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "Carol still reads it once handed on"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(!read(e.bob.clone()).await.contains("xprivate-value"));
    // Forgetting it stays immediate.
    e.store
        .change_acl(
            &e.scope,
            "source",
            &memstore::AclTarget::Episode(private_id),
            &[memstore::FORGOTTEN.into()],
        )
        .await
        .unwrap();
    let start = Instant::now();
    while read(e.carol.clone()).await.contains("xprivate-value") {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "still read after it was forgotten"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Team membership is required even when the caller administers its organization.
    admin(
        &e.url,
        json!({"op":"create_team","org":"acme","team":"support"}),
    )
    .await;
    assert_eq!(
        e.call(&e.alice, json!({"op":"entity_owners","team":"support"}))
            .await
            .0,
        403
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn document_replacement_needs_review_but_source_revocation_does_not_wait() {
    let e = Env::start().await;
    let doc = Episode::sourced(
        "policy:handbook",
        b"refund window thirty days",
        &["group:acme.all"],
    )
    .version_of("policies/refunds");
    let fact = Fact::new("policy:refund-window", "refund_days", "thirty").from_episodes(&[&doc]);
    e.store
        .commit(&e.scope, "seed", from_ref(&doc), from_ref(&fact))
        .await
        .unwrap();
    e.ok(
        &e.alice,
        json!({"op":"set_entity_owner","entity":"policy:refund-window","owner":"bob@acme.com"}),
    )
    .await;
    e.draft("pending", "policy:refund-window", "fourteen").await;
    let proposal = e.proposed("pending").await;
    let mut removed_identity = doc.clone();
    removed_identity.document = None;
    assert!(
        e.store
            .commit(&e.scope, "agent", &[removed_identity], &[])
            .await
            .is_err()
    );
    assert!(
        e.store
            .supersede(
                &e.scope,
                "agent",
                &[memstore::AclTarget::Episode(doc.episode_id.clone())]
            )
            .await
            .is_err()
    );
    let changed = e
        .store
        .change_acl(
            &e.scope,
            "source",
            &memstore::AclTarget::Episode(doc.episode_id.clone()),
            &["user:carol@acme.com".into()],
        )
        .await
        .unwrap();
    assert_eq!(changed, 1);
    // Permission loss takes effect while the owner proposal is still pending.
    let start = Instant::now();
    loop {
        let (status, body) = post(
            &format!("{}/query?org=acme", e.url),
            &e.bob,
            &json!({"data":[[0,{"find":"facts","limit":500}]]}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        if !body.contains(&fact.fact_id) {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "{body}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(e.decide(&e.bob, &proposal, "approve").await.0, 403);

    // One source change over two episodes: a note only Carol reads, which a fact on the owned policy cites (it
    // landed without Bob: no one else reads it), and a thread everyone reads. Moving the source to two readers
    // would let Alice read Carol's note: that waits for Bob, and must not hold back Bob losing the thread.
    let note = Episode::sourced(
        "channel:x:1",
        b"carol's refund note",
        &["user:carol@acme.com"],
    );
    let noted =
        Fact::new("policy:refund-window", "refund_days", "xcarol-note").from_episodes(&[&note]);
    e.store
        .commit(&e.scope, "agent", from_ref(&note), from_ref(&noted))
        .await
        .unwrap();
    let thread = Episode::sourced("channel:x:2", b"shipping five days", &["group:acme.all"]);
    let said = Fact::new("policy:shipping", "refund_days", "xthread").from_episodes(&[&thread]);
    e.store
        .commit(&e.scope, "agent", from_ref(&thread), from_ref(&said))
        .await
        .unwrap();
    let moved = e
        .store
        .change_acl(
            &e.scope,
            "source",
            &memstore::AclTarget::SourcePrefix("channel:x:".into()),
            &["user:alice@acme.com".into(), "user:carol@acme.com".into()],
        )
        .await
        .unwrap_err();
    assert!(
        memstore::governance::OwnerApprovalRequired::caused(&moved),
        "{moved:#}"
    );
    let reads = |token: String| {
        let url = format!("{}/query?org=acme", e.url);
        async move {
            post(
                &url,
                &token,
                &json!({"data":[[0,{"find":"facts","limit":500}]]}),
            )
            .await
            .1
        }
    };
    let start = Instant::now();
    while reads(e.bob.clone()).await.contains("xthread") {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "Bob still reads the thread"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(!reads(e.alice.clone()).await.contains("xcarol-note"));
}

#[test]
fn a_server_restart_preserves_pending_review_and_partial_approval() {
    let runtime = || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    };
    let first = runtime();
    let (dir, proposal) = first.block_on(async {
        let e = Env::start().await;
        for (entity, owner) in [
            ("policy:refund-window", "bob@acme.com"),
            ("policy:returns", "carol@acme.com"),
        ] {
            e.seed(entity, "thirty").await;
            e.ok(
                &e.alice,
                json!({"op":"set_entity_owner","entity":entity,"owner":owner}),
            )
            .await;
        }
        let (ep, _) = e.draft("restart", "policy:refund-window", "fourteen").await;
        let second = Fact::new("policy:returns", "refund_days", "twenty").from_episodes(&[&ep]);
        e.store
            .commit_on(&e.scope, "restart", "agent", &[], &[second])
            .await
            .unwrap();
        let p = e.proposed("restart").await;
        let (status, body) = e.decide(&e.bob, &p, "approve").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["result"]["status"], "pending_review");
        (e._dir, p)
    });
    // Drop the server, all background tasks, the catalog and its directory lock.
    drop(first);
    let second = runtime();
    second.block_on(async {
        let e = Env::open(dir, false).await;
        let saved = e
            .ok(
                &e.carol,
                json!({"op":"get_proposal","proposal":proposal["proposal"]["id"]}),
            )
            .await;
        assert_eq!(saved["status"], "pending_review");
        assert_eq!(saved["approvals"], json!(["bob@acme.com"]));
        let (status, body) = e.decide(&e.carol, &saved, "approve").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["result"]["status"], "published");
        let mut denied = Fact::new("policy:refund-window", "refund_days", "seven");
        denied.source_episodes = saved["proposal"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let error = e
            .store
            .commit(&e.scope, "bob@acme.com", &[], &[denied])
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("owner approval required"),
            "{error:#}"
        );
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn retiring_a_version_waits_only_for_the_owners_of_the_facts_it_supports() {
    let e = Env::start().await;
    let policy = Episode::sourced(
        "doc:refund-policy",
        b"refund window thirty days",
        &["group:acme.all"],
    );
    let fact = Fact::new("policy:refund-window", "refund_days", "thirty").from_episodes(&[&policy]);
    e.store
        .commit(&e.scope, "seed", from_ref(&policy), from_ref(&fact))
        .await
        .unwrap();
    e.ok(
        &e.alice,
        json!({"op":"set_entity_owner","entity":"policy:refund-window","owner":"bob@acme.com"}),
    )
    .await;
    let key = format!("msk_{}feed", uuid::Uuid::new_v4().simple());
    admin(
        &e.url,
        json!({"op":"create_key","org":"acme","name":"feed","role":"ingest","key":key}),
    )
    .await;
    let replace = async |source: &str, text: &str| -> Value {
        let body = json!({"episodes":[{"source":source,"occurred_at":1_760_000_000_000_000i64,
            "acl":["group:acme.all"],"body":text,"replaces_source":true}]});
        let (status, body) = post(&format!("{}/ingest", e.url), &key, &body).await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    };

    // A record no owned fact cites: its next version retires it, though the org has an owner elsewhere.
    replace("linear:acme/ENG-1", "ENG-1 is open").await;
    let answer = replace("linear:acme/ENG-1", "ENG-1 is done").await;
    assert_eq!(answer["retired"], 1, "{answer}");
    assert!(
        answer.get("supersede_held_for_owners").is_none(),
        "{answer}"
    );

    // A record the owned policy's fact cites: its next version lands, and retiring the old one waits for Bob, so
    // the fact is still read.
    let answer = replace(&policy.source, "refund window fourteen days").await;
    assert_eq!(answer["committed"], 1, "{answer}");
    assert_eq!(answer["retired"], 0, "{answer}");
    assert_eq!(
        answer["supersede_held_for_owners"],
        json!([policy.source]),
        "{answer}"
    );
    e.reads(&fact.fact_id, "no such fact").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_org_admin_decides_for_owners_and_owners_settle_conflicts() {
    let e = Env::start().await;
    let entity = "policy:refund-window";
    let (_, old) = e.seed(entity, "thirty").await;
    e.ok(
        &e.alice,
        json!({"op":"set_entity_owner","entity":entity,"owner":"bob@acme.com"}),
    )
    .await;
    // An org admin approves what waits for its owner: it publishes, and the decision names them as an admin.
    let (_, drafted) = e.draft("admin-approves", entity, "fourteen").await;
    let p = e.proposed("admin-approves").await;
    for token in [&e.carol, &e.foreign] {
        assert_eq!(e.decide(token, &p, "approve").await.0, 403);
    }
    let (status, body) = e.decide(&e.alice, &p, "approve").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["status"], "published", "{body}");
    assert_eq!(body["result"]["decisions"][0]["user"], "alice@acme.com");
    assert_eq!(body["result"]["decisions"][0]["admin"], true);
    e.reads(&drafted.fact_id, &old.fact_id).await;

    // Only an admin adds a fact or checks for conflicts; a fact they add on an owned thing publishes at once.
    let add = json!({"op":"add_fact","entity":entity,"predicate":"refund_days","value":"sixty","source":"finance-sheet"});
    for token in [&e.bob, &e.carol, &e.foreign] {
        assert_eq!(e.call(token, add.clone()).await.0, 403);
        assert_eq!(e.call(token, json!({"op":"check_conflicts"})).await.0, 403);
    }
    let added = e.ok(&e.alice, add).await;
    assert_eq!(added["review"]["status"], "published", "{added}");
    let sixty = added["fact_id"].as_str().unwrap().to_string();
    e.reads(&sixty, "\"no such fact\"").await;
    // Three open values of one attribute from three sources: each pair is recorded.
    let checked = e.ok(&e.alice, json!({"op":"check_conflicts"})).await;
    assert_eq!(checked["recorded"], 3, "{checked}");
    assert_eq!(
        checked["conflicts"][2]["facts"],
        json!([drafted.fact_id, sixty])
    );
    // Checked again, nothing new is recorded.
    assert_eq!(
        e.ok(&e.alice, json!({"op":"check_conflicts"})).await["recorded"],
        0
    );

    // A member sees the conflict but may not settle it; its owner may, and nobody of another org sees it.
    let listed = e.ok(&e.carol, json!({"op":"list_conflicts"})).await;
    assert_eq!(listed["conflicts"].as_array().unwrap().len(), 3, "{listed}");
    let c = listed["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| {
            c["facts"][1]["fact_id"] == sixty.as_str() && c["facts"][0]["value"] == "fourteen"
        })
        .unwrap();
    assert_eq!(c["entity"], entity, "{listed}");
    assert_eq!(c["owner"], "bob@acme.com");
    assert_eq!(c["can_resolve"], false);
    let mine = e
        .ok(&e.bob, json!({"op":"list_conflicts","mine":true}))
        .await;
    assert_eq!(mine["conflicts"][0]["can_resolve"], true, "{mine}");
    assert_eq!(
        e.call(&e.foreign, json!({"op":"list_conflicts"})).await.0,
        403
    );
    let settle =
        json!({"op":"resolve_conflict","facts":[drafted.fact_id, sixty],"value":"forty-five"});
    for token in [&e.carol, &e.foreign] {
        assert_eq!(e.call(token, settle.clone()).await.0, 403);
    }
    let settled = e.ok(&e.bob, settle).await;
    assert_eq!(settled["status"], "published", "{settled}");
    let added = settled["added"].as_str().unwrap().to_string();
    // The attribute is settled: both sides end, and so does the older value still open beside them.
    assert_eq!(
        settled["ended"],
        json!([drafted.fact_id, sixty, old.fact_id])
    );
    e.reads(&added, &sixty).await;
    e.reads(&added, &drafted.fact_id).await;
    assert_eq!(
        e.ok(&e.carol, json!({"op":"list_conflicts"})).await["conflicts"],
        json!([])
    );

    // An admin settles one by keeping a side: the other ends.
    let late = e
        .ok(
            &e.alice,
            json!({"op":"add_fact","entity":entity,"predicate":"refund_days","value":"ninety"}),
        )
        .await["fact_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        e.ok(&e.alice, json!({"op":"check_conflicts"})).await["recorded"],
        1
    );
    let kept = e
        .ok(
            &e.alice,
            json!({"op":"resolve_conflict","facts":[added, late],"keep":added}),
        )
        .await;
    assert_eq!(kept["status"], "published", "{kept}");
    // What it keeps is older than what ends: it is stated again, so reads serve it, and its first statement ends.
    assert_eq!(kept["ended"], json!([late, added]));
    let again = kept["added"].as_str().unwrap();
    e.reads(again, &late).await;
}
