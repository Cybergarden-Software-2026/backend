//! `X-Request-ID` — порт `RequestIdMiddleware`: входящий заголовок длиной 1..=128 принимается,
//! иначе генерируется `uuid4().hex`; значение возвращается в ответе и попадает в тело ошибок.

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

pub const REQUEST_ID_HEADER: &str = "x-request-id";

tokio::task_local! {
    static REQUEST_ID: String;
}

/// Идентификатор текущего запроса; `-` вне обработки запроса (как `get_request_id` в Python).
pub fn current() -> String {
    REQUEST_ID
        .try_with(Clone::clone)
        .unwrap_or_else(|_| "-".to_owned())
}

/// Идентификатор запроса в extensions — для обработчиков, которым он нужен явно.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

pub async fn middleware(mut req: Request, next: Next) -> Response {
    let incoming = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .map(str::to_owned);
    let id = incoming.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    req.extensions_mut().insert(RequestId(id.clone()));

    let mut response = REQUEST_ID.scope(id.clone(), next.run(req)).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}
