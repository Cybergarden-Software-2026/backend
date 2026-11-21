//! `/health` (живость процесса) и `/health/ready` (готовность: доступна ли БД).

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::json;

use crate::AppState;
use crate::response::PyJson;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/health/ready", get(ready))
}

async fn health() -> PyJson {
    PyJson(json!({ "status": "ok" }))
}

async fn ready(State(state): State<AppState>) -> (StatusCode, PyJson) {
    let db_ok = dashboard_store::check_database(&state.pool).await;
    let status = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        PyJson(json!({ "status": if db_ok { "ok" } else { "unavailable" }, "database": db_ok })),
    )
}
