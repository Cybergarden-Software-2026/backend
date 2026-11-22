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
use tower::ServiceExt;

const PAIRINGS: &str = "/api/v1/mobile/pairings";
const DEVICES: &str = "/api/v1/mobile/devices";
const CURRENT: &str = "/api/v1/mobile/devices/current";
const TOKEN: &str = "/api/v1/mobile/devices/current/token";
const ENABLED: &[(&str, &str)] = &[
    ("COORDINATOR_ACCOUNTS_ENABLED", "true"),
    ("MOBILE_PUSH_ENABLED", "true"),
    ("FIREBASE_PROJECT_ID", "test-project"),
];

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    data: Option<Value>,
) -> Response {
    let mut req = Request::builder().method(method).uri(path);
    for &(name, value) in headers {
        req = req.header(name, value);
    }
    let body = match data {
        Some(value) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    response
}

async fn read(response: Response, status: StatusCode) -> Value {
    let actual = response.status();
    let data = response.into_body().collect().await.unwrap().to_bytes();
    let value = if data.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&data).unwrap()
    };
    assert_eq!(actual, status, "unexpected HTTP status: {value}");
    value
}

async fn owner(env: &Env, app: &Router, login: &str) -> String {
    let accounts = create_accounts(
        env.pool(),
        &[AccountSpec {
            login,
            display_name: login,
        }],
    )
    .await
    .unwrap();
    let res = call(
        app,
        "POST",
        "/api/v1/auth/coordinator/session",
        &[],
        Some(json!({"key":accounts[0].key, "remember":false})),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    res.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

async fn pairing(app: &Router, cookie: &str) -> String {
    let body = read(
        call(app, "POST", PAIRINGS, &[("cookie", cookie)], None).await,
        StatusCode::CREATED,
    )
    .await;
    body["code"].as_str().unwrap().to_owned()
}

async fn claim(app: &Router, code: &str, token: &str) -> Response {
    call(
        app,
        "POST",
        DEVICES,
        &[],
        Some(json!({"platform":"android", "code":code, "fcmToken":token})),
    )
    .await
}

#[tokio::test]
async fn mobile_routes_are_closed_without_individual_accounts_and_explicit_enablement() {
    for extra in [vec![("COORDINATOR_ACCOUNTS_ENABLED", "true")], vec![]] {
        let settings = Settings::from_vars(
            extra
                .into_iter()
                .chain([("DATABASE_URL", "postgresql://localhost/test")]),
        )
        .unwrap();
        let pool = dashboard_store::connect_lazy(&settings).unwrap();
        let app = router(AppState::new(Arc::new(settings), pool));
        for (method, path) in [
            ("POST", PAIRINGS),
            ("GET", DEVICES),
            ("POST", DEVICES),
            ("PUT", TOKEN),
            ("DELETE", CURRENT),
        ] {
            let body = read(
                call(&app, method, path, &[], None).await,
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
            assert_eq!(body["code"], "MOBILE_PUSH_DISABLED");
        }
    }
}

#[tokio::test]
async fn pairing_replacement_expiry_atomic_claim_and_token_conflict_rollback() {
    let Some(env) = Env::with_vars(ENABLED).await else {
        return;
    };
    let app = router(AppState::with_clock(
        env.settings.clone(),
        env.pool().clone(),
        env.clock.as_clock(),
    ));
    let cookie = owner(&env, &app, "alice").await;
    let blocked = call(
        &app,
        "POST",
        PAIRINGS,
        &[("cookie", &cookie), ("origin", "https://another.example")],
        None,
    )
    .await;
    assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    let first = pairing(&app, &cookie).await;
    let expiring = pairing(&app, &cookie).await;
    assert_ne!(first, expiring);
    assert_eq!(
        read(claim(&app, &first, "fcm-1").await, StatusCode::UNAUTHORIZED).await["code"],
        "MOBILE_PAIRING_INVALID"
    );
    env.clock.advance(300.0);
    assert_eq!(
        claim(&app, &expiring, "fcm-2").await.status(),
        StatusCode::UNAUTHORIZED
    );

    let code = pairing(&app, &cookie).await;
    assert_eq!(
        call(&app, "POST", DEVICES, &[], None).await.status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let invalid = call(
        &app,
        "POST",
        DEVICES,
        &[],
        Some(json!({"platform":"android", "code":code, "fcmToken":" token"})),
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    let (one, two) = tokio::join!(
        claim(&app, &code, "fcm-registered"),
        claim(&app, &code, "fcm-registered")
    );
    assert!(matches!(
        (one.status(), two.status()),
        (StatusCode::CREATED, StatusCode::UNAUTHORIZED) | (StatusCode::UNAUTHORIZED, StatusCode::CREATED)
    ));

    let fresh = pairing(&app, &cookie).await;
    assert_eq!(
        read(claim(&app, &fresh, "fcm-registered").await, StatusCode::CONFLICT).await["code"],
        "FCM_TOKEN_IN_USE"
    );
    let new_device = read(
        call(
            &app,
            "POST",
            DEVICES,
            &[],
            Some(json!({"platform":"web", "code":fresh, "fcmToken":"fcm-web", "fcmTargetType":"fid"})),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(
        env.string("SELECT fcm_target_type FROM mobile_devices WHERE fcm_token = 'fcm-web'")
            .await
            .as_deref(),
        Some("fid"),
    );
    let bearer = format!("Bearer {}", new_device["deviceToken"].as_str().unwrap());
    assert_eq!(
        call(
            &app,
            "PUT",
            TOKEN,
            &[("authorization", &bearer)],
            Some(json!({"fcmToken":"fcm-registered"}))
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(&app, "DELETE", CURRENT, &[("authorization", &bearer)], None)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, "DELETE", CURRENT, &[("authorization", &bearer)], None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    env.finish().await;
}

#[tokio::test]
async fn mobile_credentials_are_device_scoped_and_owner_controls_revocation() {
    let Some(env) = Env::with_vars(ENABLED).await else {
        return;
    };
    let app = router(AppState::with_clock(
        env.settings.clone(),
        env.pool().clone(),
        env.clock.as_clock(),
    ));
    let alice = owner(&env, &app, "alice").await;
    let bob = owner(&env, &app, "bob").await;
    let code = pairing(&app, &alice).await;
    let device = read(claim(&app, &code, "fcm-alice").await, StatusCode::CREATED).await;
    let id = device["deviceId"].as_str().unwrap();
    let bearer = format!("Bearer {}", device["deviceToken"].as_str().unwrap());

    let items = read(
        call(
            &app,
            "GET",
            DEVICES,
            &[("cookie", &alice), ("x-user-id", "bob")],
            None,
        )
        .await,
        StatusCode::OK,
    )
    .await;
    let item = &items["items"][0];
    assert_eq!(item["deviceId"], id);
    assert_eq!(item["platform"], "android");
    assert_eq!(item["pushEnabled"], true);
    assert!(item["createdAt"].is_string());
    assert_eq!(item.as_object().unwrap().len(), 4);
    let items = read(
        call(
            &app,
            "GET",
            DEVICES,
            &[("cookie", &bob), ("x-user-id", "alice")],
            None,
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(items["items"], json!([]));

    for (method, path) in [("GET", DEVICES), ("POST", PAIRINGS), ("GET", "/api/v1/snapshot")] {
        assert_eq!(
            call(&app, method, path, &[("authorization", &bearer)], None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call(&app, "DELETE", CURRENT, &[("cookie", &alice)], None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let path = format!("{DEVICES}/{id}");
    assert_eq!(
        read(
            call(&app, "DELETE", &path, &[("cookie", &bob)], None).await,
            StatusCode::NOT_FOUND
        )
        .await["code"],
        "MOBILE_DEVICE_NOT_FOUND"
    );
    for _ in 0..2 {
        assert_eq!(
            call(
                &app,
                "PUT",
                TOKEN,
                &[("authorization", &bearer)],
                Some(json!({"fcmToken":"fcm-rotated", "fcmTargetType":"fid"}))
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
    }
    assert_eq!(
        env.string("SELECT fcm_target_type FROM mobile_devices")
            .await
            .as_deref(),
        Some("fid")
    );
    assert_eq!(
        env.string("SELECT fcm_token FROM mobile_devices")
            .await
            .as_deref(),
        Some("fcm-rotated")
    );
    assert_eq!(
        call(&app, "DELETE", &path, &[("cookie", &alice)], None)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &app,
            "PUT",
            TOKEN,
            &[("authorization", &bearer)],
            Some(json!({"fcmToken":"fcm-rotated"}))
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );

    let fresh = pairing(&app, &alice).await;
    let replacement = read(claim(&app, &fresh, "fcm-alice").await, StatusCode::CREATED).await;
    assert_eq!(
        env.string("SELECT fcm_target_type FROM mobile_devices")
            .await
            .as_deref(),
        Some("token")
    );
    let new_bearer = format!("Bearer {}", replacement["deviceToken"].as_str().unwrap());
    let pending = pairing(&app, &alice).await;
    let (disabled, raced_claim) = tokio::join!(
        disable_account(env.pool(), "alice"),
        claim(&app, &pending, "racing-target"),
    );
    assert!(disabled.unwrap());
    assert!(matches!(
        raced_claim.status(),
        StatusCode::CREATED | StatusCode::UNAUTHORIZED
    ));
    assert_eq!(
        env.i64("SELECT count(*) FROM mobile_devices WHERE account_login = 'alice'")
            .await,
        0
    );
    assert_eq!(
        env.i64("SELECT count(*) FROM mobile_pairings WHERE account_login = 'alice'")
            .await,
        0
    );
    let bob_code = pairing(&app, &bob).await;
    assert_eq!(
        claim(&app, &bob_code, "fcm-alice").await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        claim(&app, &pending, "fcm-disabled").await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "DELETE", CURRENT, &[("authorization", &new_bearer)], None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    env.finish().await;
}
