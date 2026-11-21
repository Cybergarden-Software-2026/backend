//! OpenAPI и страницы документации — те же, что генерирует FastAPI Python-версии.
//!
//! `static/` собирает `tests/fixtures/gen_api_parity.py` из настоящего приложения FastAPI:
//! схема маршрутов, параметров и ответов — общий контракт обеих версий, поэтому Rust отдаёт её
//! как есть, подставляя только `info.title` (`APP_NAME`). Страницы Swagger UI / ReDoc — шаблоны
//! FastAPI с заполнителем `{{TITLE}}`.

use std::sync::LazyLock;

use axum::Router;
use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use serde_json::Value;

use crate::AppState;
use crate::response::PyJson;

const OPENAPI: &str = include_str!("../static/openapi.json");
const SWAGGER: &str = include_str!("../static/swagger.html");
const REDOC: &str = include_str!("../static/redoc.html");
const OAUTH2_REDIRECT: &str = include_str!("../static/oauth2-redirect.html");

static SPEC: LazyLock<Value> = LazyLock::new(|| serde_json::from_str(OPENAPI).expect("static/openapi.json"));

/// `/api/docs` (Swagger UI), `/redoc`, `/docs/oauth2-redirect` — как у FastAPI.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/docs", get(swagger))
        .route("/redoc", get(redoc))
        .route("/docs/oauth2-redirect", get(|| async { Html(OAUTH2_REDIRECT) }))
}

/// Схема OpenAPI с заголовком из настроек.
pub fn spec(app_name: &str) -> Value {
    let mut spec = SPEC.clone();
    spec["info"]["title"] = Value::String(app_name.to_owned());
    spec
}

pub async fn openapi(State(state): State<AppState>) -> PyJson {
    PyJson(spec(&state.settings.app_name))
}

async fn swagger(State(state): State<AppState>) -> Html<String> {
    Html(SWAGGER.replace("{{TITLE}}", &state.settings.app_name))
}

async fn redoc(State(state): State<AppState>) -> Html<String> {
    Html(REDOC.replace("{{TITLE}}", &state.settings.app_name))
}
