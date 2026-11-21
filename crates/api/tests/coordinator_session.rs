#[path = "../../importer/tests/common/mod.rs"]
mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use common::Env;
use dashboard_api::{AppState, router};
use dashboard_config::Settings;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const SESSION: &str = "/api/v1/auth/coordinator/session";
const TEST_KEY: &str = "temporary-coordinator-test-key-fbe9";
const FRONTEND: &str = "https://frontend.example.test";

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
    request(
        app,
        "POST",
        SESSION,
        &[
            ("Content-Type", "application/json"),
            ("Origin", origin),
            ("Host", "api:8080"),
        ],
        Body::from(json!({"key":key,"remember":remember}).to_string()),
    )
    .await
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
        json!({"required":true,"authenticated":false})
    );

    let disabled = router(AppState::with_clock(
        Arc::new(Settings::from_vars([("DATABASE_URL", env.db.url.as_str())]).unwrap()),
        env.pool().clone(),
        env.clock.as_clock(),
    ));
    let expected = json!({"required":false,"authenticated":false});
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
        json!({"required":true,"authenticated":true})
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
