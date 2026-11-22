//! HTTP-слой без БД: пул указывает на заведомо недоступный адрес.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dashboard_api::{AppState, router};
use dashboard_config::Settings;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app(extra: &[(&str, &str)]) -> axum::Router {
    let mut vars = vec![("DATABASE_URL", "postgresql+asyncpg://u:p@127.0.0.1:1/none")];
    vars.extend_from_slice(extra);
    let settings = Settings::from_vars(vars).unwrap();
    let pool = dashboard_store::connect_lazy(&settings).unwrap();
    router(AppState::new(Arc::new(settings), pool))
}

async fn call(app: axum::Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Value) {
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, headers, body)
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn web_contract_is_documented() {
    let (status, _, spec) = call(app(&[]), get("/api/v1/openapi.json")).await;
    assert_eq!(status, StatusCode::OK);
    let paths = &spec["paths"];
    let queue = &paths["/api/v1/incidents"]["get"];
    let params = queue["parameters"].as_array().unwrap();
    for name in [
        "page", "pageSize", "group", "priority", "status", "sla", "limit", "cursor",
    ] {
        assert!(params.iter().any(|param| param["name"] == name), "{name}");
    }
    assert!(paths["/api/v1/incidents/{key}/analysis"]["get"].is_object());
    assert!(spec["components"]["schemas"]["QueuePage"]["properties"]["totalPages"].is_object());
    assert!(spec["components"]["schemas"]["IncidentAnalysis"]["properties"]["candidates"].is_object());
}

#[tokio::test]
async fn health_is_ok_without_database() {
    let (status, headers, body) = call(app(&[]), get("/health")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"status": "ok"}));
    // Сгенерированный request-id: uuid4().hex — 32 шестнадцатеричных символа.
    let id = headers["x-request-id"].to_str().unwrap();
    assert_eq!(id.len(), 32);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
}

#[tokio::test]
async fn ready_is_503_when_database_is_down() {
    let (status, _, body) = call(app(&[]), get("/health/ready")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, json!({"status": "unavailable", "database": false}));
    // Порядок ключей как у Python: status, database.
    assert_eq!(
        serde_json::to_string(&body).unwrap(),
        r#"{"status":"unavailable","database":false}"#
    );
}

#[tokio::test]
async fn unknown_route_uses_error_format() {
    let req = Request::get("/nope")
        .header("X-Request-ID", "req-42")
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = call(app(&[]), req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers["x-request-id"], "req-42");
    assert_eq!(
        serde_json::to_string(&body).unwrap(),
        r#"{"error":"Not Found","code":"NOT_FOUND","message":"Not Found","requestId":"req-42"}"#
    );
}

#[tokio::test]
async fn wrong_method_is_405_with_error_format() {
    let req = Request::post("/health").body(Body::empty()).unwrap();
    let (status, headers, body) = call(app(&[]), req).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["code"], "METHOD_NOT_ALLOWED");
    assert_eq!(body["error"], "Method Not Allowed");
    assert_eq!(body["requestId"], headers["x-request-id"].to_str().unwrap());
}

#[tokio::test]
async fn too_long_request_id_is_replaced() {
    let long = "x".repeat(129);
    let req = Request::get("/health")
        .header("X-Request-ID", &long)
        .body(Body::empty())
        .unwrap();
    let (_, headers, _) = call(app(&[]), req).await;
    assert_eq!(headers["x-request-id"].len(), 32);
    let ok = "y".repeat(128);
    let req = Request::get("/health")
        .header("X-Request-ID", &ok)
        .body(Body::empty())
        .unwrap();
    let (_, headers, _) = call(app(&[]), req).await;
    assert_eq!(headers["x-request-id"], ok.as_str());
}

#[tokio::test]
async fn cors_only_when_configured() {
    let preflight = || {
        Request::builder()
            .method("OPTIONS")
            .uri("/health")
            .header("Origin", "http://localhost:5173")
            .header("Access-Control-Request-Method", "GET")
            .header("Access-Control-Request-Headers", "x-custom")
            .body(Body::empty())
            .unwrap()
    };
    let (_, headers, _) = call(app(&[]), preflight()).await;
    assert!(headers.get("access-control-allow-origin").is_none());

    let cors = app(&[("CORS_ORIGINS", r#"["http://localhost:5173"]"#)]);
    let (status, headers, _) = call(cors.clone(), preflight()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["access-control-allow-origin"], "http://localhost:5173");
    assert_eq!(headers["access-control-allow-headers"], "x-custom");
    assert_eq!(headers["access-control-allow-credentials"], "true");

    let req = Request::get("/health")
        .header("Origin", "http://localhost:5173")
        .body(Body::empty())
        .unwrap();
    let (_, headers, _) = call(cors, req).await;
    assert_eq!(headers["access-control-expose-headers"], "x-request-id");
}

#[tokio::test]
async fn app_error_body_has_details() {
    use axum::response::IntoResponse;
    let err = dashboard_api::AppError::not_found("Инцидент не найден")
        .with_code("INCIDENT_NOT_FOUND")
        .with_detail("sysId", "abc");
    let res = err.into_response();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        std::str::from_utf8(&bytes).unwrap(),
        r#"{"error":"Инцидент не найден","code":"INCIDENT_NOT_FOUND","message":"Инцидент не найден","requestId":"-","sysId":"abc"}"#
    );
}

fn json_request(method: &str, uri: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn demo_routes_are_hidden_unless_enabled() {
    let req = json_request("POST", "/api/v1/demo", &json!({"action":"mass"}));
    let (status, _, body) = call(app(&[]), req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "NOT_FOUND");
    let req = json_request("PUT", "/api/v1/incidents", &json!([]));
    let (status, _, body) = call(app(&[]), req).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["code"], "METHOD_NOT_ALLOWED");
}

#[tokio::test]
async fn shift_statistics_rejects_invalid_ranges_without_querying_the_database() {
    for uri in [
        "/api/v1/statistics/shift",
        "/api/v1/statistics/shift?from=2026-11-20T12:00:00&to=2026-11-20T12:01:00Z",
        "/api/v1/statistics/shift?from=2026-11-20T12:01:00Z&to=2026-11-20T12:00:00Z",
    ] {
        let (status, _, body) = call(app(&[]), get(uri)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(body["code"], "VALIDATION_ERROR");
    }
}

#[tokio::test]
async fn demo_routes_are_documented() {
    let (_, _, spec) = call(app(&[]), get("/api/v1/openapi.json")).await;
    assert!(spec["paths"]["/api/v1/demo"]["post"].is_object());
    assert!(spec["paths"]["/api/v1/incidents"]["put"].is_object());
    assert!(spec["components"]["schemas"]["DemoRequest"].is_object());
}

#[tokio::test]
async fn mobile_contract_distinguishes_session_and_push_bearers() {
    let (_, _, spec) = call(app(&[]), get("/api/v1/openapi.json")).await;
    let owner = json!([{"CoordinatorSession":[]}]);
    let device = json!([{"MobileSessionBearer":[]},{"MobileDeviceBearer":[]}]);
    let session = json!([{"MobileSessionBearer":[]}]);
    let qr = json!([]);
    for (method, path, auth, success) in [
        ("post", "pairings", &owner, "201"),
        ("post", "session", &qr, "201"),
        ("get", "session", &session, "200"),
        ("delete", "session", &session, "204"),
        ("get", "notifications", &session, "200"),
        ("put", "notifications/{notification_id}/read", &session, "204"),
        ("get", "devices", &owner, "200"),
        ("post", "devices", &qr, "201"),
        ("put", "devices/current/token", &device, "204"),
        ("delete", "devices/current", &device, "204"),
        ("delete", "devices/{device_id}", &owner, "204"),
    ] {
        let operation = &spec["paths"][format!("/api/v1/mobile/{path}")][method];
        assert_eq!(&operation["security"], auth, "{method} {path}");
        assert!(operation["responses"][success].is_object());
    }
    let schemas = &spec["components"]["schemas"];
    assert_eq!(schemas["MobileLogin"]["required"], json!(["code", "deviceId"]));
    assert_eq!(
        schemas["MobileLogin"]["properties"]["platform"]["default"],
        "android"
    );
    assert_eq!(
        schemas["MobileLoginResult"]["required"],
        json!(["token", "tokenType", "deviceId", "user"])
    );
    assert_eq!(schemas["MobileSession"]["required"], json!(["deviceId", "user"]));
    let registration = &schemas["MobileRegistration"];
    assert_eq!(registration["required"], json!(["code", "platform", "fcmToken"]));
    let target = &schemas["MobileFcmTargetType"];
    assert_eq!(target["enum"], json!(["token", "fid"]));
    assert_eq!(target["default"], "token");
    for input in ["MobileRegistration", "MobileTokenUpdate"] {
        assert_eq!(
            schemas[input]["properties"]["fcmTargetType"]["$ref"],
            "#/components/schemas/MobileFcmTargetType"
        );
    }
}
