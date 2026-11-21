//! HTTP API дашборда (axum) — порт `app/main.py`.
//!
//! * R1: `/health`, `/health/ready`, `X-Request-ID`, CORS, единый формат ошибок 404/405/500;
//! * R5: REST `/api/v1` (API.md и ТЗ) — [`dashboard`], [`notifications`], проверка параметров с
//!   текстами FastAPI/pydantic ([`params`]), OpenAPI и страницы документации ([`docs`]),
//!   307 на адрес без завершающего `/` (как `redirect_slashes` Starlette);
//! * демо-режим фронтенда (`POST /demo`, `PUT /incidents`, только при `DEMO_API_ENABLED`) — [`demo`];
//! * R6: SSE (`/events/stream`, `/events?since=…`) — [`sse`]: broadcaster и режимы
//!   `snapshot`/`notify`.

mod analysis;
pub mod assignments;
pub mod dashboard;
pub mod demo;
pub mod docs;
pub mod error;
pub mod health;
pub mod notifications;
pub mod params;
pub mod request_id;
pub mod response;
pub mod sse;
mod web_queue;

use std::any::Any;
use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use chrono::{DateTime, Utc};
use dashboard_config::Settings;
use sqlx::PgPool;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{AllowHeaders, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

pub use error::{ApiResult, AppError};

/// Источник текущего времени (`get_clock`; в тестах подменяется).
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Текущее время с точностью до микросекунд (как `datetime.now(UTC)` и PostgreSQL).
pub fn system_clock() -> Clock {
    Arc::new(|| {
        let now = Utc::now();
        now - chrono::TimeDelta::nanoseconds(i64::from(now.timestamp_subsec_nanos() % 1000))
    })
}

#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
    pub pool: PgPool,
    pub clock: Clock,
    /// Один broadcaster SSE на процесс; создаётся при первом подключении к потоку
    /// (`get_broadcaster`). Общий для всех копий состояния.
    broadcaster: Arc<OnceLock<Arc<sse::Broadcaster>>>,
}

impl AppState {
    pub fn new(settings: Arc<Settings>, pool: PgPool) -> Self {
        Self::with_clock(settings, pool, system_clock())
    }

    pub fn with_clock(settings: Arc<Settings>, pool: PgPool, clock: Clock) -> Self {
        Self {
            settings,
            pool,
            clock,
            broadcaster: Arc::default(),
        }
    }

    /// Копия без общего broadcaster — для самого broadcaster (без цикла ссылок).
    pub(crate) fn detached(&self) -> Self {
        Self::with_clock(
            Arc::clone(&self.settings),
            self.pool.clone(),
            Arc::clone(&self.clock),
        )
    }

    /// `get_broadcaster`: создаёт broadcaster при первом вызове и запускает опрос журнала.
    pub fn broadcaster(&self) -> Arc<sse::Broadcaster> {
        let b = self.broadcaster.get_or_init(|| {
            sse::Broadcaster::new(
                Arc::new(sse::DbJournal::new(self)),
                self.settings.sse_poll_interval,
            )
        });
        b.start();
        Arc::clone(b)
    }

    /// Остановка сервера: опрос журнала прекращается, открытые потоки SSE закрываются.
    pub fn shutdown_streams(&self) {
        if let Some(b) = self.broadcaster.get() {
            b.shutdown();
        }
    }
}

/// Маршруты `/api/v1` (`app/api/v1/router.py`).
fn api_v1() -> Router<AppState> {
    Router::new()
        .route("/snapshot", get(dashboard::snapshot))
        .route("/teams", get(assignments::teams))
        .route("/incidents/{key}/assignment", post(assignments::assign))
        .route("/dashboard", get(dashboard::dashboard))
        .route(
            "/incidents",
            get(dashboard::list_incidents)
                .put(demo::replace_incidents)
                .layer(DefaultBodyLimit::max(
                    usize::try_from(dashboard_importer::reader::DEFAULT_MAX_BYTES).unwrap_or(usize::MAX),
                )),
        )
        .route("/demo", post(demo::simulate))
        .route("/incidents/{key}", get(dashboard::get_incident))
        .route("/incidents/{key}/events", get(dashboard::get_incident_events))
        .route("/incidents/{key}/analysis", get(analysis::get))
        .route("/events", get(dashboard::list_events))
        .route("/events/stream", get(dashboard::stream_events))
        .route("/notifications", get(notifications::list_notifications))
        .route(
            "/notifications/{notification_id}/read",
            put(notifications::mark_read),
        )
        .route("/openapi.json", get(docs::openapi))
}

pub fn router(state: AppState) -> Router {
    let cors = cors_layer(&state.settings);
    let mut app = Router::new()
        .merge(health::routes())
        .nest("/api/v1", api_v1())
        .merge(docs::routes())
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
        .layer(CatchPanicLayer::custom(on_panic))
        // Строка доступа на каждый запрос (как access-лог uvicorn).
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        );
    if let Some(cors) = cors {
        app = app.layer(cors);
    }
    // Внешний слой: request-id виден и в ответах CORS/паники, и в логах.
    app.layer(axum::middleware::from_fn(request_id::middleware))
}

/// CORS включается, только если задан `CORS_ORIGINS` (как в Python-версии).
fn cors_layer(settings: &Settings) -> Option<CorsLayer> {
    if settings.cors_origins.is_empty() {
        return None;
    }
    let origins: Vec<HeaderValue> = settings
        .cors_origins
        .iter()
        .filter_map(|o| match HeaderValue::from_str(o) {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!(origin = %o, "ignoring invalid CORS origin");
                None
            }
        })
        .collect();
    Some(
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([Method::GET, Method::POST, Method::PUT, Method::OPTIONS])
            // allow_headers=["*"] в Starlette = отражение запрошенных заголовков.
            .allow_headers(AllowHeaders::mirror_request())
            .expose_headers([HeaderName::from_static(request_id::REQUEST_ID_HEADER)]),
    )
}

/// Шаблоны путей Python-версии (для 307 на адрес без `/`); `*` — один сегмент.
const ROUTE_PATTERNS: &[&str] = &[
    "/health",
    "/health/ready",
    "/api/v1/snapshot",
    "/api/v1/teams",
    "/api/v1/incidents/*/assignment",
    "/api/v1/dashboard",
    "/api/v1/incidents",
    "/api/v1/demo",
    "/api/v1/incidents/*",
    "/api/v1/incidents/*/events",
    "/api/v1/incidents/*/analysis",
    "/api/v1/events",
    "/api/v1/events/stream",
    "/api/v1/notifications",
    "/api/v1/notifications/*/read",
    "/api/v1/openapi.json",
    "/api/docs",
    "/docs/oauth2-redirect",
    "/redoc",
];

fn matches_route(path: &str) -> bool {
    ROUTE_PATTERNS.iter().any(|pattern| {
        let (mut a, mut b) = (pattern.split('/'), path.split('/'));
        loop {
            match (a.next(), b.next()) {
                (None, None) => return true,
                (Some("*"), Some(seg)) if !seg.is_empty() => {}
                (Some(p), Some(seg)) if p == seg => {}
                _ => return false,
            }
        }
    })
}

/// 404 или, как `redirect_slashes` Starlette, 307 на тот же адрес без завершающего `/`.
async fn not_found(req: Request) -> Response {
    let path = req.uri().path();
    if path.len() > 1 && path.ends_with('/') && matches_route(&path[..path.len() - 1]) {
        let host = req
            .headers()
            .get(header::HOST)
            .map(|h| params::latin1(h.as_bytes()))
            .unwrap_or_else(|| "localhost".to_owned());
        let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
        let location = format!("http://{host}{}{query}", &path[..path.len() - 1]);
        if let Ok(value) = HeaderValue::from_str(&location) {
            return (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, value)]).into_response();
        }
    }
    AppError::http(StatusCode::NOT_FOUND).into_response()
}

async fn method_not_allowed() -> AppError {
    AppError::http(StatusCode::METHOD_NOT_ALLOWED)
}

fn on_panic(err: Box<dyn Any + Send + 'static>) -> Response {
    let detail = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| err.downcast_ref::<&str>().copied())
        .unwrap_or("panic");
    tracing::error!(request_id = %request_id::current(), panic = detail, "Unhandled error");
    AppError::internal().into_response()
}

/// Запуск сервера с плавной остановкой по сигналу `shutdown`.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let streams = state.clone();
    let shutdown = async move {
        shutdown.await;
        // Бесконечные ответы SSE иначе держали бы плавную остановку.
        streams.shutdown_streams();
    };
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}
