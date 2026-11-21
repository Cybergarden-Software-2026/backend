#[path = "../../importer/tests/common/mod.rs"]
mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use common::Env;
use dashboard_api::coordinator_accounts::{AccountSpec, create_accounts, disable_account};
use dashboard_api::{AppState, router};
use dashboard_config::Settings;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const SESSION: &str = "/api/v1/auth/coordinator/session";
const TEST_KEY: &str = "temporary-coordinator-test-key-fbe9";
const FRONTEND: &str = "https://frontend.example.test";
const ACCOUNTS: &[(&str, &str)] = &[
    ("COORDINATOR_ACCOUNTS_ENABLED", "true"),
    ("CORS_ORIGINS", r#"["https://frontend.example.test"]"#),
    ("SSE_HEARTBEAT_SECONDS", "0.05"),
];

fn app(env: &Env) -> Router {
    router(AppState::with_clock(
        env.settings.clone(),
        env.pool().clone(),
        env.clock.as_clock(),
    ))
}

async fn request(app: &Router, method: &str, path: &str, headers: &[(&str, &str)], body: Body) -> Response {
    let mut req = Request::builder().method(method).uri(path);
    for &(name, value) in headers {
        req = req.header(name, value);
    }
    app.clone().oneshot(req.body(body).unwrap()).await.unwrap()
}

async fn status(app: &Router, cookie: Option<&str>) -> Value {
    let headers: Vec<_> = cookie.into_iter().map(|v| ("Cookie", v)).collect();
    let response = request(app, "GET", SESSION, &headers, Body::empty()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

async fn login(app: &Router, key: &str, remember: bool, origin: &str) -> Response {
    sign_in(app, json!({"key":key,"remember":remember}), origin).await
}

async fn sign_in(app: &Router, data: Value, origin: &str) -> Response {
    request(
        app,
        "POST",
        SESSION,
        &[
            ("Content-Type", "application/json"),
            ("Origin", origin),
            ("Host", "api:8080"),
        ],
        Body::from(data.to_string()),
    )
    .await
}

async fn json_response(response: Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

fn cookie(response: &Response) -> (&str, &str) {
    assert_eq!(response.status(), StatusCode::OK);
    let set = response.headers()[header::SET_COOKIE].to_str().unwrap();
    let (cookie, attrs) = set.split_once(';').unwrap();
    assert!(
        cookie.starts_with("__Host-underctrl-coordinator="),
        "unexpected cookie name"
    );
    assert!(attrs.starts_with(" Path=/; Secure; HttpOnly; SameSite=Strict"));
    assert!(!attrs.contains("Domain="));
    (cookie, attrs)
}

#[tokio::test]
async fn auth_is_optional_but_all_private_routes_require_it_when_configured() {
    let Some(env) = Env::with_vars(&[("COORDINATOR_ACCESS_KEY", TEST_KEY)]).await else {
        return;
    };
    let protected = app(&env);
    for (method, path) in [
        ("GET", "/api/v1/snapshot?view=web"),
        ("GET", "/api/v1/dashboard"),
        ("GET", "/api/v1/incidents"),
        ("GET", "/api/v1/incidents/123"),
        ("GET", "/api/v1/incidents/123/events"),
        ("GET", "/api/v1/incidents/123/analysis"),
        ("GET", "/api/v1/events"),
        ("GET", "/api/v1/events?since=0"),
        ("GET", "/api/v1/events/stream"),
        ("GET", "/api/v1/teams"),
        ("GET", "/api/v1/statistics/shift"),
        ("POST", "/api/v1/incidents/123/assignment"),
        ("GET", "/api/v1/problems/clusters"),
        ("PATCH", "/api/v1/problems/clusters"),
        ("GET", "/api/v1/notifications"),
        ("PUT", "/api/v1/notifications/123/read"),
        ("POST", "/api/v1/demo"),
        ("PUT", "/api/v1/incidents"),
    ] {
        let response = request(&protected, method, path, &[], Body::empty()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    for path in ["/health", "/health/ready", "/api/docs", "/api/v1/openapi.json"] {
        assert_eq!(
            request(&protected, "GET", path, &[], Body::empty())
                .await
                .status(),
            StatusCode::OK,
            "{path}"
        );
    }
    assert_eq!(
        status(&protected, None).await,
        json!({"required":true,"authenticated":false,"mode":"shared-key"})
    );

    let disabled = router(AppState::with_clock(
        Arc::new(Settings::from_vars([("DATABASE_URL", env.db.url.as_str())]).unwrap()),
        env.pool().clone(),
        env.clock.as_clock(),
    ));
    let expected = json!({"required":false,"authenticated":false,"mode":"none"});
    assert_eq!(status(&disabled, None).await, expected);
    assert_eq!(
        request(&disabled, "GET", "/api/v1/snapshot", &[], Body::empty())
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let inactive = login(&disabled, TEST_KEY, false, FRONTEND).await;
    assert_eq!(inactive.status(), StatusCode::OK);
    assert!(inactive.headers().get(header::SET_COOKIE).is_none());
    let body: Value =
        serde_json::from_slice(&inactive.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body, expected);
    env.finish().await;
}

#[tokio::test]
async fn login_checks_origin_and_key_and_expires_or_rotates_sessions() {
    let Some(mut env) = Env::with_vars(&[
        ("COORDINATOR_ACCESS_KEY", TEST_KEY),
        ("CORS_ORIGINS", r#"["https://frontend.example.test"]"#),
    ])
    .await
    else {
        return;
    };
    env.write_json(&common::records());
    assert!(env.tick().await.accepted());
    let protected = app(&env);
    let invalid = login(&protected, "wrong key", false, FRONTEND).await;
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
    assert!(invalid.headers().get(header::SET_COOKIE).is_none());
    let blocked = login(&protected, TEST_KEY, false, "https://untrusted.example.test").await;
    assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    assert!(
        blocked
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none()
    );

    let initial = login(&protected, TEST_KEY, false, FRONTEND).await;
    assert_eq!(initial.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], FRONTEND);
    assert_eq!(
        initial.headers()[header::ACCESS_CONTROL_ALLOW_CREDENTIALS],
        "true"
    );
    let (first, attrs) = cookie(&initial);
    assert!(!attrs.contains("Max-Age"));
    assert_eq!(
        status(&protected, Some(first)).await,
        json!({"required":true,"authenticated":true,"mode":"shared-key"})
    );
    let response = request(
        &protected,
        "GET",
        "/api/v1/snapshot?view=web",
        &[("Cookie", first)],
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let cross_site = request(
        &protected,
        "DELETE",
        SESSION,
        &[("Cookie", first), ("Origin", "https://untrusted.example.test")],
        Body::empty(),
    )
    .await;
    assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);
    assert_eq!(status(&protected, Some(first)).await["authenticated"], true);

    env.clock.advance(12.0 * 60.0 * 60.0);
    assert_eq!(status(&protected, Some(first)).await["authenticated"], false);
    let persistent = login(&protected, TEST_KEY, true, FRONTEND).await;
    let (remembered, attrs) = cookie(&persistent);
    assert!(attrs.contains("Max-Age=604800"));
    let restarted = app(&env);
    assert_eq!(status(&restarted, Some(remembered)).await["authenticated"], true);
    let rotated = router(AppState::with_clock(
        Arc::new(
            Settings::from_vars([
                ("DATABASE_URL", env.db.url.as_str()),
                (
                    "COORDINATOR_ACCESS_KEY",
                    "new-temporary-coordinator-test-key-47b1",
                ),
            ])
            .unwrap(),
        ),
        env.pool().clone(),
        env.clock.as_clock(),
    ));
    assert_eq!(status(&rotated, Some(remembered)).await["authenticated"], false);
    env.clock.advance(7.0 * 24.0 * 60.0 * 60.0);
    assert_eq!(status(&protected, Some(remembered)).await["authenticated"], false);
    env.finish().await;
}

#[tokio::test]
async fn same_origin_proxy_login_and_logout_protect_existing_event_stream() {
    let Some(env) = Env::with_vars(&[
        ("COORDINATOR_ACCESS_KEY", TEST_KEY),
        ("SSE_HEARTBEAT_SECONDS", "0.05"),
    ])
    .await
    else {
        return;
    };
    let protected = app(&env);
    let login = request(
        &protected,
        "POST",
        SESSION,
        &[
            ("Host", "api:8080"),
            ("Origin", FRONTEND),
            ("Sec-Fetch-Site", "same-origin"),
            ("Content-Type", "application/json"),
        ],
        Body::from(json!({"key":TEST_KEY,"remember":true}).to_string()),
    )
    .await;
    let (cookie, _) = cookie(&login);
    let stream = request(
        &protected,
        "GET",
        "/api/v1/events/stream",
        &[("Cookie", cookie)],
        Body::empty(),
    )
    .await;
    assert_eq!(stream.status(), StatusCode::OK);
    assert_eq!(stream.headers()[header::CACHE_CONTROL], "no-store, no-transform");
    let mut body = stream.into_body();
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(first.into_data().unwrap().starts_with(b"retry:"));
    let logout = request(
        &protected,
        "DELETE",
        SESSION,
        &[
            ("Cookie", cookie),
            ("Origin", FRONTEND),
            ("Sec-Fetch-Site", "same-origin"),
        ],
        Body::empty(),
    )
    .await;
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    assert!(
        logout.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(status(&protected, Some(cookie)).await["authenticated"], false);
    tokio::time::timeout(std::time::Duration::from_secs(2), body.collect())
        .await
        .unwrap()
        .unwrap();
    let reconnect = request(
        &protected,
        "GET",
        "/api/v1/events/stream",
        &[("Cookie", cookie)],
        Body::empty(),
    )
    .await;
    assert_eq!(reconnect.status(), StatusCode::UNAUTHORIZED);
    env.finish().await;
}

#[tokio::test]
async fn accounts_mode_rejects_legacy_credentials_even_before_provisioning() {
    let vars = [
        ("COORDINATOR_ACCESS_KEY", TEST_KEY),
        ("COORDINATOR_ACCOUNTS_ENABLED", "true"),
    ];
    let Some(env) = Env::with_vars(&vars).await else {
        return;
    };
    let accounts = app(&env);
    let legacy = router(AppState::with_clock(
        Arc::new(
            Settings::from_vars([
                ("DATABASE_URL", env.db.url.as_str()),
                ("COORDINATOR_ACCESS_KEY", TEST_KEY),
            ])
            .unwrap(),
        ),
        env.pool().clone(),
        env.clock.as_clock(),
    ));
    let old = request(
        &legacy,
        "POST",
        SESSION,
        &[("Content-Type", "application/json")],
        Body::from(json!({"key":TEST_KEY,"remember":true}).to_string()),
    )
    .await;
    let (old_cookie, _) = cookie(&old);
    let unauthenticated = json!({"required":true,"authenticated":false,"mode":"accounts"});
    assert_eq!(status(&accounts, None).await, unauthenticated);
    assert_eq!(status(&accounts, Some(old_cookie)).await, unauthenticated);
    let response = request(
        &accounts,
        "GET",
        "/api/v1/snapshot",
        &[("Cookie", old_cookie)],
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let shared_login = request(
        &accounts,
        "POST",
        SESSION,
        &[("Content-Type", "application/json")],
        Body::from(json!({"key":TEST_KEY,"remember":true}).to_string()),
    )
    .await;
    assert_eq!(shared_login.status(), StatusCode::UNAUTHORIZED);
    assert!(shared_login.headers().get(header::SET_COOKIE).is_none());
    assert!(
        !dashboard_api::coordinator_accounts::has_enabled_account(env.pool())
            .await
            .unwrap()
    );
    env.finish().await;
}

#[tokio::test]
async fn per_account_key_and_password_are_private_and_revocable() {
    let Some(env) = Env::with_vars(ACCOUNTS).await else {
        return;
    };
    let specs = [
        AccountSpec {
            login: "coordinator-01",
            display_name: "Первый координатор",
        },
        AccountSpec {
            login: "coordinator-02",
            display_name: "Второй координатор",
        },
        AccountSpec {
            login: "coordinator-03",
            display_name: "Третий координатор",
        },
    ];
    let issued = create_accounts(env.pool(), &specs).await.unwrap();
    assert_eq!(issued.len(), 3);
    assert!(create_accounts(env.pool(), &specs).await.unwrap().is_empty());
    for (index, creds) in issued.iter().enumerate() {
        let (password_hash, key_hash): (String, Vec<u8>) =
            sqlx::query_as("SELECT password_hash, key_hash FROM coordinator_accounts WHERE login = $1")
                .bind(&creds.login)
                .fetch_one(env.pool())
                .await
                .unwrap();
        assert!(password_hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(!password_hash.contains(&creds.password));
        assert_eq!(key_hash.as_slice(), &Sha256::digest(creds.key.as_bytes())[..]);
        assert!(
            !issued[..index]
                .iter()
                .any(|other| other.key == creds.key || other.password == creds.password)
        );
    }

    let protected = app(&env);
    let first = &issued[0];
    let password_auth = sign_in(
        &protected,
        json!({"login":" COORDINATOR-01 ","password":first.password,"remember":false}),
        FRONTEND,
    )
    .await;
    let (password_cookie, attributes) = cookie(&password_auth);
    assert!(!attributes.contains("Max-Age"));
    let key_auth = login(&protected, &first.key, true, FRONTEND).await;
    let (key_cookie, attributes) = cookie(&key_auth);
    assert!(attributes.contains("Max-Age=604800"));
    assert_ne!(password_cookie, key_cookie);
    let third_auth = login(&protected, &issued[2].key, false, FRONTEND).await;
    let (third_cookie, _) = cookie(&third_auth);
    let first_profile = json!({"required":true,"authenticated":true,"mode":"accounts","coordinator":{
        "login":first.login,"displayName":first.display_name,
    }});
    assert_eq!(status(&protected, Some(password_cookie)).await, first_profile);
    assert_eq!(status(&app(&env), Some(key_cookie)).await, first_profile);
    assert_eq!(
        status(&protected, Some(third_cookie)).await["coordinator"]["login"],
        issued[2].login
    );
    let mixed = sign_in(
        &protected,
        json!({"login":first.login,"password":first.password,"key":first.key,"remember":false}),
        FRONTEND,
    )
    .await;
    assert_eq!(mixed.status(), StatusCode::BAD_REQUEST);

    let check_error = async |name: &str| {
        let result = sign_in(
            &protected,
            json!({"login":name,"password":"wrong-password","remember":false}),
            FRONTEND,
        )
        .await;
        assert_eq!(result.status(), StatusCode::UNAUTHORIZED);
        assert!(result.headers().get(header::SET_COOKIE).is_none());
        json_response(result).await["message"].clone()
    };
    assert_eq!(
        check_error(first.login.as_str()).await,
        check_error("nonexistent-account").await
    );

    let stream = request(
        &protected,
        "GET",
        "/api/v1/events/stream",
        &[("Cookie", password_cookie)],
        Body::empty(),
    )
    .await;
    assert_eq!(stream.status(), StatusCode::OK);
    let mut body = stream.into_body();
    let timeout = std::time::Duration::from_secs(2);
    let frame = tokio::time::timeout(timeout, body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(frame.into_data().unwrap().starts_with(b"retry:"));

    assert!(disable_account(env.pool(), &first.login).await.unwrap());
    assert!(!disable_account(env.pool(), &first.login).await.unwrap());
    assert_eq!(
        status(&protected, Some(password_cookie)).await["authenticated"],
        false
    );
    assert_eq!(status(&protected, Some(key_cookie)).await["authenticated"], false);
    assert_eq!(
        status(&protected, Some(third_cookie)).await["authenticated"],
        true
    );
    tokio::time::timeout(timeout, body.collect())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        login(&protected, &first.key, false, FRONTEND).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let disabled_login = sign_in(
        &protected,
        json!({"login":first.login,"password":first.password,"remember":false}),
        FRONTEND,
    )
    .await;
    assert_eq!(disabled_login.status(), StatusCode::UNAUTHORIZED);
    env.finish().await;
}

#[tokio::test]
async fn assignment_and_notification_state_take_authenticated_identity() {
    let Some(mut env) = Env::with_vars(ACCOUNTS).await else {
        return;
    };
    let mut source = common::records();
    source.truncate(2);
    source[0].insert("stateEnum".into(), json!("Assigned"));
    source[0].insert("stateId".into(), json!(0));
    source[0].insert("assignedGroupName".into(), json!("Previous"));
    source[1].insert("assignedGroupName".into(), json!("New team"));
    env.write_json(&source);
    assert!(env.tick().await.accepted());
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO notifications (type, title, dedup_key) VALUES ('source.error', 'test', 'account-read-test') RETURNING id",
    ).fetch_one(env.pool()).await.unwrap();
    let profiles = [
        AccountSpec {
            login: "operator-a",
            display_name: "Operator A",
        },
        AccountSpec {
            login: "operator-b",
            display_name: "Operator B",
        },
    ];
    let issued = create_accounts(env.pool(), &profiles).await.unwrap();
    let protected = app(&env);
    let first = login(&protected, &issued[0].key, false, FRONTEND).await;
    let second = login(&protected, &issued[1].key, false, FRONTEND).await;
    let (actor, _) = cookie(&first);
    let (other, _) = cookie(&second);

    let sys_id = source[0]["sysId"].as_str().unwrap();
    let assigned = request(
        &protected,
        "POST",
        &format!("/api/v1/incidents/{sys_id}/assignment"),
        &[
            ("Cookie", actor),
            ("Content-Type", "application/json"),
            ("X-User-Id", "operator-b"),
        ],
        Body::from(json!({"team":"New team","expectedRevision":1}).to_string()),
    )
    .await;
    assert_eq!(assigned.status(), StatusCode::OK);
    let logged: String =
        sqlx::query_scalar("SELECT payload->>'actor' FROM dashboard_events WHERE reason = 'assignment'")
            .fetch_one(env.pool())
            .await
            .unwrap();
    assert_eq!(logged, "operator-a");

    let marked = request(
        &protected,
        "PUT",
        &format!("/api/v1/notifications/{id}/read"),
        &[("Cookie", actor), ("X-User-Id", "operator-b")],
        Body::empty(),
    )
    .await;
    assert_eq!(marked.status(), StatusCode::NO_CONTENT);
    let mine = request(
        &protected,
        "GET",
        "/api/v1/notifications",
        &[("Cookie", actor), ("X-User-Id", "ignored")],
        Body::empty(),
    )
    .await;
    let theirs = request(
        &protected,
        "GET",
        "/api/v1/notifications",
        &[("Cookie", other), ("X-User-Id", "operator-a")],
        Body::empty(),
    )
    .await;
    for (res, read) in [(mine, true), (theirs, false)] {
        let values = json_response(res).await;
        let entry = values["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == id)
            .unwrap();
        assert_eq!(entry["isRead"], read);
    }
    env.finish().await;
}
