//! JSON-ответ, записанный как у `JSONResponse` Starlette:
//! `json.dumps(content, ensure_ascii=False, separators=(",", ":"))`, `application/json`.
//! Тела совпадают с Python-версией байт в байт (в том числе запись чисел с плавающей точкой).

use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use dashboard_domain::pycompat;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct PyJson(pub Value);

impl IntoResponse for PyJson {
    fn into_response(self) -> Response {
        (
            [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
            pycompat::dumps(&self.0),
        )
            .into_response()
    }
}
