//! Единый JSON-формат ошибок — порт `app/core/errors.py` и `app/core/exceptions.py`:
//! `{"error": "...", "code": "INCIDENT_NOT_FOUND", "message": "...", "requestId": "..."}`
//! (+ дополнительные поля `details`, например `errors` у `VALIDATION_ERROR`).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

use crate::request_id;
use crate::response::PyJson;

#[derive(Debug, Clone)]
pub struct AppError {
    pub status: StatusCode,
    pub code: Cow,
    pub message: String,
    /// Дополнительные поля верхнего уровня тела ответа.
    pub details: Map<String, Value>,
}

type Cow = std::borrow::Cow<'static, str>;

impl AppError {
    pub fn new(status: StatusCode, code: impl Into<Cow>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            details: Map::new(),
        }
    }

    /// 400 `BAD_REQUEST` (как `AppError` по умолчанию).
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "BAD_REQUEST", message)
    }

    /// 400 `VALIDATION_ERROR`.
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "VALIDATION_ERROR", message)
    }

    /// 404 `NOT_FOUND` (или свой код, например `INCIDENT_NOT_FOUND`).
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "NOT_FOUND", message)
    }

    /// 409 `CONFLICT`.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "CONFLICT", message)
    }

    /// 503 `SOURCE_NOT_READY`.
    pub fn source_not_ready(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "SOURCE_NOT_READY", message)
    }

    /// 500 `INTERNAL_ERROR` — текст как у обработчика необработанных исключений Python.
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "Внутренняя ошибка сервера",
        )
    }

    pub fn with_code(mut self, code: impl Into<Cow>) -> Self {
        self.code = code.into();
        self
    }

    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }

    /// Ошибка уровня HTTP (Starlette `HTTPException`): код по статусу, текст — стандартная фраза.
    pub fn http(status: StatusCode) -> Self {
        let code = match status.as_u16() {
            400 => "BAD_REQUEST",
            401 => "UNAUTHORIZED",
            403 => "FORBIDDEN",
            404 => "NOT_FOUND",
            405 => "METHOD_NOT_ALLOWED",
            429 => "TOO_MANY_REQUESTS",
            _ => "HTTP_ERROR",
        };
        Self::new(status, code, status.canonical_reason().unwrap_or("Error"))
    }

    /// Тело ответа; `requestId` берётся из текущего запроса (`-`, если вне запроса).
    pub fn body(&self) -> Value {
        let mut body = Map::new();
        body.insert("error".into(), self.message.clone().into());
        body.insert("code".into(), self.code.to_string().into());
        body.insert("message".into(), self.message.clone().into());
        body.insert("requestId".into(), request_id::current().into());
        for (k, v) in &self.details {
            body.insert(k.clone(), v.clone());
        }
        Value::Object(body)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.status.as_u16(), self.code, self.message)
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, PyJson(self.body())).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!(error = %err, request_id = %request_id::current(), "database error");
        Self::internal()
    }
}

pub type ApiResult<T> = Result<T, AppError>;
