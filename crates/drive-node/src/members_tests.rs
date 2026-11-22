//! `/orgs/{org}/members` against a stand-in issuer that speaks the auth
//! server's organization API (drive#50).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};

/// The issuer: `tm` is in `church` (with two others), `ts` in `elsewhere`.
async fn issuer(asked: Arc<AtomicUsize>) -> String {
    let who = |h: &HeaderMap| {
        h.get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .trim_start_matches("Bearer ")
            .to_string()
    };
    let list = move |h: HeaderMap| async move {
        match who(&h).as_str() {
            "tm" => Ok(axum::Json(serde_json::json!([
                { "organization": { "id": "9f1c", "slug": "church", "name": "Church" },
                  "membership": { "role": "member" } }
            ]))),
            "ts" => Ok(axum::Json(serde_json::json!([
                { "organization": { "id": "77aa", "slug": "elsewhere" },
                  "membership": { "role": "owner" } }
            ]))),
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    };
    let members = move |h: HeaderMap, body: String| {
        let asked = asked.clone();
        async move {
            asked.fetch_add(1, Ordering::SeqCst);
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            if who(&h) != "tm" || v["organization_id"] != "9f1c" {
                return Err(StatusCode::NOT_FOUND);
            }
            let m = |id: &str, email: &str, name: serde_json::Value, role: &str| {
                serde_json::json!({
                    "member": { "id": format!("m-{id}"), "organization_id": "9f1c", "user_id": id, "role": role },
                    "user": { "id": id, "email": email, "name": name, "image": null }
                })
            };
            Ok(axum::Json(serde_json::json!([
                m("u2", "zoe@example.com", "Zoe".into(), "member"),
                m("u1", "ann@example.com", "Ann".into(), "owner"),
                m(
                    "u3",
                    "no.name@example.com",
                    serde_json::Value::Null,
                    "admin"
                ),
            ])))
        }
    };
    let app = Router::new()
        .route("/auth/organization/list", post(list))
        .route("/auth/organization/members", post(members))
        .route(
            "/oauth2/userinfo",
            get(|| async { axum::Json(serde_json::json!({ "email": "someone@example.com" })) }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn an_orgs_members_from_the_issuer_for_its_members_only() {
    let asked = Arc::new(AtomicUsize::new(0));
    let issuer = issuer(asked.clone()).await;
    let data = tempfile::tempdir().unwrap();
    let opts = crate::Options {
        issuer: Some(issuer),
        no_iroh: true,
        no_backfill: true,
        ..Default::default()
    };
    let app = crate::router(data.path(), Vec::new(), opts).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let http = reqwest::Client::new();
    let ask = |org: &str, token: Option<&str>| {
        let mut r = http.get(format!("{node}/orgs/{org}/members"));
        if let Some(t) = token {
            r = r.bearer_auth(t);
        }
        r.send()
    };

    let r = ask("church", Some("tm")).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let v: serde_json::Value = r.json().await.unwrap();
    let members: Vec<drive_proto::OrgMember> =
        serde_json::from_value(v["members"].clone()).unwrap();
    // By name (else email), with the issuer's account ids and roles.
    let rows: Vec<(&str, &str, &str, &str)> = members
        .iter()
        .map(|m| {
            (
                m.account.as_str(),
                m.email.as_str(),
                m.name.as_str(),
                m.role.as_str(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("u1", "ann@example.com", "Ann", "owner"),
            ("u3", "no.name@example.com", "", "admin"),
            ("u2", "zoe@example.com", "Zoe", "member"),
        ]
    );
    // Kept a minute: the issuer is not asked again.
    assert_eq!(
        ask("church", Some("tm")).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(asked.load(Ordering::SeqCst), 1);

    // Not their org, not signed in, a token the issuer refuses.
    assert_eq!(
        ask("church", Some("ts")).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        ask("elsewhere", Some("tm")).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        ask("church", None).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        ask("church", Some("junk")).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn without_an_issuer_nobody_is_listed() {
    let data = tempfile::tempdir().unwrap();
    let opts = crate::Options {
        no_iroh: true,
        no_backfill: true,
        ..Default::default()
    };
    let app = crate::router(data.path(), Vec::new(), opts).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let v: serde_json::Value = reqwest::get(format!("{node}/orgs/home/members"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["members"], serde_json::json!([]));
}
