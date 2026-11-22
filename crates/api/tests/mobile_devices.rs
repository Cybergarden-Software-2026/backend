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
const SESSION: &str = "/api/v1/mobile/session";
const INBOX: &str = "/api/v1/mobile/notifications";
const INCIDENTS: &str = "/api/v1/mobile/incidents";
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

async fn login_device(app: &Router, code: &str, device_id: &str) -> Response {
    call(
        app,
        "POST",
        SESSION,
        &[],
        Some(json!({"code":code, "deviceId":device_id})),
    )
    .await
}

#[tokio::test]
async fn login_requires_personal_accounts_and_fcm_registration_requires_push() {
    for accounts in ["true", "false"] {
        let settings = Settings::from_vars([
            ("COORDINATOR_ACCOUNTS_ENABLED", accounts),
            ("DATABASE_URL", "postgresql://localhost/test"),
        ])
        .unwrap();
        let pool = dashboard_store::connect_lazy(&settings).unwrap();
        let app = router(AppState::new(Arc::new(settings), pool));
        for (method, path) in [("POST", DEVICES), ("PUT", TOKEN)] {
            let body = read(
                call(&app, method, path, &[], None).await,
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
            assert_eq!(body["code"], "MOBILE_PUSH_DISABLED");
        }
        if accounts == "false" {
            for (method, path) in [
                ("POST", PAIRINGS),
                ("GET", DEVICES),
                ("DELETE", CURRENT),
                ("POST", SESSION),
                ("GET", SESSION),
                ("DELETE", SESSION),
                ("GET", INBOX),
                ("GET", INCIDENTS),
            ] {
                let body = read(
                    call(&app, method, path, &[], None).await,
                    StatusCode::SERVICE_UNAVAILABLE,
                )
                .await;
                assert_eq!(body["code"], "MOBILE_ACCOUNTS_DISABLED");
            }
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

    for (method, path) in [
        ("GET", DEVICES),
        ("POST", PAIRINGS),
        ("GET", "/api/v1/snapshot"),
        ("GET", SESSION),
        ("GET", INBOX),
        ("GET", INCIDENTS),
        ("DELETE", SESSION),
    ] {
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

#[tokio::test]
async fn qr_login_without_firebase_authenticates_the_account_and_its_notification_reads() {
    let Some(env) = Env::with_vars(&[("COORDINATOR_ACCOUNTS_ENABLED", "true")]).await else {
        return;
    };
    let app = router(AppState::with_clock(
        env.settings.clone(),
        env.pool().clone(),
        env.clock.as_clock(),
    ));
    let alice = owner(&env, &app, "alice").await;
    let bob = owner(&env, &app, "bob").await;
    sqlx::query("UPDATE coordinator_accounts SET display_name = 'Alice Example' WHERE login = 'alice'")
        .execute(env.pool())
        .await
        .unwrap();
    let code = pairing(&app, &alice).await;
    for device_id in ["", "one two", "тест", &"a".repeat(129)] {
        let body = read(
            login_device(&app, &code, device_id).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!(body["code"], "VALIDATION_ERROR");
    }
    let device_id = "a".repeat(128);
    let session = read(login_device(&app, &code, &device_id).await, StatusCode::CREATED).await;
    let token = session["token"].as_str().unwrap();
    let user = json!({"login":"alice","displayName":"Alice Example"});
    assert!(token.starts_with("ucm_"));
    assert_eq!(
        session,
        json!({"token":token,"tokenType":"Bearer","deviceId":device_id,"user":user})
    );
    let bearer = format!("Bearer {token}");
    let as_alice = [("authorization", bearer.as_str()), ("x-user-id", "bob")];
    let current = read(call(&app, "GET", SESSION, &as_alice, None).await, StatusCode::OK).await;
    assert_eq!(current, json!({"deviceId":device_id,"user":user}));
    assert_eq!(
        env.string("SELECT platform FROM mobile_devices").await.as_deref(),
        Some("android")
    );
    assert_eq!(env.i64("SELECT count(fcm_token) FROM mobile_devices").await, 0);
    let repeated = read(
        login_device(&app, &code, &device_id).await,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    assert_eq!(repeated["code"], "MOBILE_PAIRING_INVALID");

    let notification_id: i64 = sqlx::query_scalar(
        "INSERT INTO notifications(type, title, dedup_key) VALUES ('critical.raised', 'Critical', 'test-mobile') RETURNING id"
    ).fetch_one(env.pool()).await.unwrap();
    let unread = read(call(&app, "GET", INBOX, &as_alice, None).await, StatusCode::OK).await;
    assert_eq!(unread["unreadCount"], 1);
    assert_eq!(unread["items"][0]["id"], notification_id);
    let mark = format!("{INBOX}/{notification_id}/read");
    assert_eq!(
        call(&app, "PUT", &mark, &as_alice, None).await.status(),
        StatusCode::NO_CONTENT
    );
    for (cookie, expected) in [(&alice, true), (&bob, false)] {
        let page = read(
            call(&app, "GET", "/api/v1/notifications", &[("cookie", cookie)], None).await,
            StatusCode::OK,
        )
        .await;
        assert_eq!(page["items"][0]["isRead"], expected);
    }
    assert_eq!(
        call(&app, "GET", DEVICES, &as_alice, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "DELETE", SESSION, &as_alice, None).await.status(),
        StatusCode::NO_CONTENT
    );
    for path in [SESSION, INBOX, INCIDENTS] {
        let unauthorized = read(
            call(&app, "GET", path, &as_alice, None).await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        assert_eq!(unauthorized["code"], "MOBILE_SESSION_UNAUTHORIZED");
    }
    env.finish().await;
}

#[tokio::test]
async fn mobile_incidents_returns_the_complete_current_header_input_without_firebase() {
    let Some(mut env) = Env::with_vars(&[
        ("COORDINATOR_ACCOUNTS_ENABLED", "true"),
        ("SOURCE_NAIVE_TIMEZONE", "Europe/Moscow"),
    ])
    .await
    else {
        return;
    };
    let app = router(AppState::with_clock(
        env.settings.clone(),
        env.pool().clone(),
        env.clock.as_clock(),
    ));
    let cookie = owner(&env, &app, "reader").await;
    for headers in [vec![], vec![("cookie", cookie.as_str())]] {
        let denied = read(
            call(&app, "GET", INCIDENTS, &headers, None).await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        assert_eq!(denied["code"], "MOBILE_SESSION_UNAUTHORIZED");
    }
    let code = pairing(&app, &cookie).await;
    let session = read(
        login_device(&app, &code, "reader-phone").await,
        StatusCode::CREATED,
    )
    .await;
    let bearer = format!("Bearer {}", session["token"].as_str().unwrap());
    let headers = [("authorization", bearer.as_str())];
    let before_import = read(
        call(&app, "GET", INCIDENTS, &headers, None).await,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert_eq!(before_import["code"], "SOURCE_NOT_READY");
    assert!(before_import.get("source").is_none());

    let mut template = common::records().remove(0);
    common::update(
        &mut template,
        &[
            ("header", json!("Network fault")),
            ("stateEnum", json!("Assigned")),
            ("stateId", json!(0)),
            ("priorityId", json!(1)),
            ("assignedGroupName", json!("Team A")),
            ("submitDate", json!("2026-11-20 14:00:00.123456")),
            ("deadline", json!("2026-11-20 15:05:00.987654")),
            (
                "lastAssignmentToTeamDateTime",
                json!("2026-11-20 14:45:00.456789"),
            ),
        ],
    );
    let mut incidents = vec![template; 102];
    for (index, row) in incidents.iter_mut().enumerate() {
        row.insert("id".into(), json!(format!("INC{index:04}")));
        row.insert("sysId".into(), json!(format!("{index:06}")));
    }
    common::update(
        &mut incidents[0],
        &[("stateEnum", json!("Research")), ("priorityId", json!(4))],
    );
    common::update(
        &mut incidents[1],
        &[
            ("assignedGroupName", json!(common::COORD)),
            ("lastAssignmentToTeamDateTime", Value::Null),
        ],
    );
    common::update(
        &mut incidents[3],
        &[("stateEnum", json!("Closed")), ("priorityId", json!(4))],
    );
    common::update(
        &mut incidents[4],
        &[("stateEnum", json!("Resolved")), ("priorityId", json!(4))],
    );
    common::update(
        &mut incidents[5],
        &[
            ("assignedGroupName", Value::Null),
            ("deadline", Value::Null),
            ("header", Value::Null),
        ],
    );
    // JS trims a BOM but preserves NEL. Either character can come from a copied team name.
    for (index, prefix) in [(6, '\u{feff}'), (7, '\u{0085}')] {
        common::update(
            &mut incidents[index],
            &[
                ("assignedGroupName", json!(format!("{prefix}{}", common::COORD))),
                ("deadline", Value::Null),
            ],
        );
    }
    // The former snapshot's only row must disappear; all 101 remaining rows must fit in one response.
    env.write_json(&incidents[101..]);
    assert!(env.tick().await.accepted());
    env.write_json(&incidents[..101]);
    assert!(env.tick().await.accepted());
    let snapshot = read(call(&app, "GET", INCIDENTS, &headers, None).await, StatusCode::OK).await;
    assert_eq!(snapshot["serverTime"], "2026-11-20T12:00:02.000Z");
    assert_eq!(
        snapshot["revision"],
        env.i64("SELECT MAX(id) FROM dashboard_events").await
    );
    let mut rows = snapshot["items"].as_array().unwrap().clone();
    rows.sort_by(|a, b| a["sysId"].as_str().cmp(&b["sysId"].as_str()));
    assert_eq!(rows.len(), 101);
    for (row, expected) in rows.iter().zip(&incidents) {
        assert_eq!(row["sysId"], expected["sysId"]);
    }
    assert_eq!(
        rows[0],
        json!({
            "sysId":"000000", "id":"INC0000", "header":"Network fault", "normalizedState":"Research",
            "priorityId":4, "assignedGroupName":"Team A", "submitDate":"2026-11-20T11:00:00.123Z",
            "deadline":"2026-11-20T12:05:00.987Z", "lastAssignmentToTeamDateTime":"2026-11-20T11:45:00.456Z",
            "headerKind":"critical"
        })
    );
    assert_eq!(rows[1]["headerKind"], "waiting");
    assert!(rows[1]["lastAssignmentToTeamDateTime"].is_null());
    assert_eq!(rows[2]["headerKind"], "deadline");
    assert_eq!(rows[6]["headerKind"], "waiting");
    assert_eq!(rows[6]["assignedGroupName"], common::COORD);
    assert_eq!(rows[7].get("headerKind"), Some(&Value::Null));
    assert_eq!(rows[3]["normalizedState"], "Closed");
    assert_eq!(rows[4]["normalizedState"], "Resolved");
    for row in &rows[3..=5] {
        assert_eq!(row.get("headerKind"), Some(&Value::Null));
    }
    for key in ["header", "deadline", "assignedGroupName"] {
        assert_eq!(rows[5].get(key), Some(&Value::Null));
    }
    env.clock.advance(300.0);
    let later = read(call(&app, "GET", INCIDENTS, &headers, None).await, StatusCode::OK).await;
    assert_eq!(later["revision"], snapshot["revision"]);
    assert_eq!(later["serverTime"], "2026-11-20T12:05:02.000Z");
    let expired = later["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["sysId"] == "000002")
        .unwrap();
    assert_eq!(expired.get("headerKind"), Some(&Value::Null));

    env.write_json(&[]);
    assert!(env.tick().await.accepted());
    let empty = read(call(&app, "GET", INCIDENTS, &headers, None).await, StatusCode::OK).await;
    assert_eq!(empty["items"], json!([]));
    env.finish().await;
}

#[tokio::test]
async fn fresh_qr_rotates_the_same_installation_and_preserves_push_without_cross_account_takeover() {
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
    let device_id = "android:fixture-1";
    let code = pairing(&app, &alice).await;
    let (first, second) = tokio::join!(
        login_device(&app, &code, device_id),
        login_device(&app, &code, device_id)
    );
    let (success, replay) = if first.status() == StatusCode::CREATED {
        (first, second)
    } else {
        (second, first)
    };
    let session = read(success, StatusCode::CREATED).await;
    assert_eq!(
        read(replay, StatusCode::UNAUTHORIZED).await["code"],
        "MOBILE_PAIRING_INVALID"
    );
    let original = format!("Bearer {}", session["token"].as_str().unwrap());
    let as_first = [("authorization", original.as_str())];
    assert_eq!(
        call(
            &app,
            "PUT",
            TOKEN,
            &as_first,
            Some(json!({"fcmToken":"registered","fcmTargetType":"fid"}))
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let alice_devices = read(
        call(&app, "GET", DEVICES, &[("cookie", &alice)], None).await,
        StatusCode::OK,
    )
    .await;
    let revoke = format!(
        "{DEVICES}/{}",
        alice_devices["items"][0]["deviceId"].as_str().unwrap()
    );

    let bob_code = pairing(&app, &bob).await;
    assert_eq!(
        read(
            login_device(&app, &bob_code, device_id).await,
            StatusCode::CONFLICT
        )
        .await["code"],
        "MOBILE_DEVICE_IN_USE"
    );
    let fresh = pairing(&app, &alice).await;
    let rotated = read(login_device(&app, &fresh, device_id).await, StatusCode::CREATED).await;
    let updated = format!("Bearer {}", rotated["token"].as_str().unwrap());
    assert_ne!(updated, original);
    assert_eq!(
        call(&app, "GET", SESSION, &as_first, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "DELETE", CURRENT, &as_first, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(env.i64("SELECT count(*) FROM mobile_devices").await, 1);
    assert_eq!(
        env.string("SELECT fcm_token FROM mobile_devices WHERE fcm_target_type='fid'")
            .await
            .as_deref(),
        Some("registered")
    );
    assert_eq!(
        call(&app, "DELETE", &revoke, &[("cookie", &bob)], None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "DELETE", &revoke, &[("cookie", &alice)], None)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, "GET", SESSION, &[("authorization", &updated)], None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let reassigned = read(
        login_device(&app, &bob_code, device_id).await,
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(reassigned["user"]["login"], "bob");
    assert_eq!(env.i64("SELECT count(fcm_token) FROM mobile_devices").await, 0);
    let bob_token = format!("Bearer {}", reassigned["token"].as_str().unwrap());
    assert!(disable_account(env.pool(), "bob").await.unwrap());
    assert_eq!(
        call(&app, "GET", INBOX, &[("authorization", &bob_token)], None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    env.finish().await;
}
