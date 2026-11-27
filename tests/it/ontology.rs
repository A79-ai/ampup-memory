//! An org's ontology (its schema) end to end, as a page that draws it reads it: a real server over HTTP on a local
//! catalog, an org set up with a team. Its admin types the org with the built-in `sales`, and every member, in
//! the team or not, reads each link with how it reads both ways (`label`, `inverse`). The admin then names the
//! links of an ontology of the org's own at the same version; a member can't change it, and a name that is too
//! long or on a non-link is refused.

use crate::isolation::{ADMIN_KEY, CLIENT_ID, admin, id_token, post, start_idp};
use memstore::auth::{self, OidcConfig, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;
use std::time::{Duration, Instant};

/// The ontology a member reads on the org's browse: the schema, with each link's names.
async fn ontology(url: &str, bearer: &str) -> Value {
    let (status, body) = post(
        &format!("{url}/browse?org=acme"),
        bearer,
        &json!({"op": "schema", "history": false}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str::<Value>(&body).unwrap()["schema"].clone()
}

fn link<'a>(schema: &'a Value, ty: &str, predicate: &str) -> &'a Value {
    &schema["types"][ty]["predicates"][predicate]
}

#[tokio::test(flavor = "multi_thread")]
async fn members_read_each_link_both_ways_and_only_admins_name_them() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local_dir(dir.path()).await.unwrap();
    let store =
        Store::new(catalog.clone()).with_index_base(dir.path().join("index").display().to_string());
    store.ensure_scope(&Scope::Org).await.unwrap();
    let issuer = start_idp().await;
    let keys = json!([{"id": "platform", "sha256": auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes())),
                       "user": "svc:platform", "admin": true}]);
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
    let url = format!("http://127.0.0.1:{port}");
    let start = Instant::now();
    while reqwest::get(format!("{url}/health")).await.is_err() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The setup: an org, its admin, a team, a member in the team and one who isn't.
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "create_team", "org": "acme", "team": "sales_team"}),
        json!({"op": "invite", "org": "acme", "members": [
            {"user": "bob@acme.com", "teams": ["sales_team"]},
            {"user": "carol@acme.com"}]}),
    ] {
        admin(&url, change).await;
    }
    let alice = id_token(&issuer, "alice@acme.com");
    let bob = id_token(&issuer, "bob@acme.com");
    let carol = id_token(&issuer, "carol@acme.com");
    let set = |bearer: String, schema: Value| {
        let url = url.clone();
        async move {
            post(
                &format!("{url}/admin"),
                &bearer,
                &json!({"op": "set_schema", "org": "acme", "schema": schema}),
            )
            .await
        }
    };

    // The admin types the org with the built-in sales ontology.
    let (status, body) = set(alice.clone(), json!("sales")).await;
    assert_eq!(status, 200, "{body}");

    // Every member reads each link both ways, the ones whose names say nothing included.
    for bearer in [&bob, &carol, &alice] {
        let s = ontology(&url, bearer).await;
        assert_eq!(s["name"], "sales", "{s}");
        for (ty, p, label, inverse) in [
            ("objection", "about", "raised on", "objections"),
            ("task", "deal", "for", "tasks"),
            ("meeting", "deal", "about", "meetings"),
            ("stakeholder", "deal", "on", "stakeholders"),
            ("commitment", "about", "about", "commitments"),
            ("deal", "owner", "owned by", "owns"),
        ] {
            let l = link(&s, ty, p);
            assert_eq!(
                (l["label"].as_str(), l["inverse"].as_str()),
                (Some(label), Some(inverse)),
                "{ty}.{p}: {l}"
            );
        }
        // Every link is named; nothing that isn't a link is.
        for (ty, def) in s["types"].as_object().unwrap() {
            for (p, pd) in def["predicates"].as_object().into_iter().flatten() {
                let named = pd.get("label").is_some() && pd.get("inverse").is_some();
                assert_eq!(named, pd["type"] == "ref", "{ty}.{p}: {pd}");
            }
        }
    }

    // The org's own type laid on sales, as an org adds its own (a win story here), written without names: none
    // come back for it, while sales' own links keep theirs.
    let mut own: Value = serde_json::from_str(include_str!("../../schemas/sales.json")).unwrap();
    own["types"]["win_story"] = json!({"predicates": {
        "deal": {"type": "ref", "to": ["deal"]},
        "summary": {"type": "text"}}});
    let (status, body) = set(alice.clone(), own.clone()).await;
    assert_eq!(status, 200, "{body}");
    let s = ontology(&url, &bob).await;
    assert_eq!(link(&s, "objection", "about")["label"], "raised on", "{s}");
    let l = link(&s, "win_story", "deal");
    assert!(
        l.get("label").is_none() && l.get("inverse").is_none(),
        "{l}"
    );

    // The admin names its link at the same version, and the members read it.
    let mut named = own.clone();
    named["types"]["win_story"]["predicates"]["deal"]["label"] = json!("won");
    named["types"]["win_story"]["predicates"]["deal"]["inverse"] = json!("win stories");
    // A member who isn't an admin, in the team or not, may not.
    for bearer in [&bob, &carol] {
        let (status, body) = set(bearer.clone(), named.clone()).await;
        assert_eq!(status, 403, "{body}");
    }
    assert!(
        link(&ontology(&url, &carol).await, "win_story", "deal")
            .get("label")
            .is_none()
    );
    let (status, body) = set(alice.clone(), named.clone()).await;
    assert_eq!(status, 200, "{body}");
    for bearer in [&bob, &carol] {
        let s = ontology(&url, bearer).await;
        let l = link(&s, "win_story", "deal");
        assert_eq!(
            (l["label"].as_str(), l["inverse"].as_str()),
            (Some("won"), Some("win stories")),
            "{l}"
        );
        assert_eq!(s["version"], 7, "{s}");
    }

    // A name too long, blank, not a string, or on something that isn't a link is refused, and the names stay.
    for (p, field, value) in [
        ("deal", "label", json!("x".repeat(41))),
        ("deal", "inverse", json!(" ")),
        ("deal", "label", json!(3)),
        ("summary", "label", json!("is")),
    ] {
        let mut bad = named.clone();
        bad["types"]["win_story"]["predicates"][p][field] = value.clone();
        let (status, body) = set(alice.clone(), bad).await;
        assert_eq!(status, 400, "{p}.{field} = {value}: {body}");
    }
    let l = link(&ontology(&url, &bob).await, "win_story", "deal").clone();
    assert_eq!(l["label"], "won", "{l}");
}
