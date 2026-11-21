//! Уведомления координатора — порт `app/api/v1/notifications.py`,
//! `app/services/notification_service.py` и `app/core/security.py` (пользователь из `X-User-Id`).

use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, StatusCode};
use dashboard_domain::cursors::parse_cursor;
use dashboard_domain::projection::notification_view;
use dashboard_domain::pycompat;
use dashboard_store::read_repo;
use serde_json::{Value, json};

use crate::AppState;
use crate::dashboard::invalid_cursor;
use crate::error::{ApiResult, AppError};
use crate::params::{self, Params};
use crate::response::PyJson;

pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;

/// `USER_ID_PATTERN`: `^[A-Za-z0-9_.@\-]{1,64}$`.
fn valid_user_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'@' | b'-'))
}

/// `get_current_user`. Точка расширения: проверка токена корпоративного входа (OIDC).
/// Как зависимость FastAPI, вызывается раньше проверки параметров пути и query.
#[allow(clippy::result_large_err)] // AppError — тело ответа; путь ошибки редкий
pub fn current_user(state: &AppState, headers: &HeaderMap) -> ApiResult<String> {
    let header = params::header(headers, "x-user-id").unwrap_or_default();
    let stripped = pycompat::strip(&header);
    let user = if stripped.is_empty() {
        state.settings.default_user_id.as_str()
    } else {
        stripped
    };
    if !valid_user_id(user) {
        return Err(AppError::validation("Некорректный заголовок X-User-Id").with_code("INVALID_USER"));
    }
    Ok(user.to_owned())
}

pub async fn list_notifications(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> ApiResult<PyJson> {
    let user = current_user(&state, &headers)?;
    let mut p = Params::new(&uri);
    let cursor = p.query_str("cursor", None);
    let limit = p.query_int("limit", 1, 200, DEFAULT_LIMIT);
    p.finish()?;

    let limit = limit.clamp(1, MAX_LIMIT);
    let before_id = parse_cursor(cursor.as_deref(), "cursor").map_err(invalid_cursor)?;
    let mut tx = state.pool.begin().await?;
    let rows = read_repo::notifications_page(&mut tx, &user, before_id, limit + 1).await?;
    let has_more = rows.len() as i64 > limit;
    let items: Vec<_> = rows.iter().take(limit as usize).collect();
    let next = match items.last() {
        Some(last) if has_more => Value::String(last.id.to_string()),
        _ => Value::Null,
    };
    let unread = read_repo::unread_count(&mut tx, &user).await?;
    tx.rollback().await?;
    Ok(PyJson(json!({
        "items": items.into_iter().map(notification_view).collect::<Vec<_>>(),
        "nextCursor": next,
        "unreadCount": unread,
    })))
}

/// `PUT /notifications/{id}/read` — 204, идемпотентно.
///
/// Отличие от Python, оставленное намеренно: id вне `bigint` Python передаёт в asyncpg и
/// отвечает 500; здесь такого уведомления просто нет — 404.
pub async fn mark_read(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    let user = current_user(&state, &headers)?;
    let mut p = Params::new(&uri);
    let segment = uri.path().split('/').nth(4).unwrap_or("");
    let id = p.path_int("notification_id", &params::decode_path_segment(segment), 1);
    p.finish()?;

    let not_found = || AppError::not_found("Уведомление не найдено").with_code("NOTIFICATION_NOT_FOUND");
    let id = id.and_then(|v| i64::try_from(v).ok()).ok_or_else(not_found)?;
    let mut tx = state.pool.begin().await?;
    if !read_repo::notification_exists(&mut tx, id).await? {
        return Err(not_found());
    }
    read_repo::mark_read(&mut tx, id, &user, (state.clock)()).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::valid_user_id;

    #[test]
    fn user_id_pattern() {
        assert!(valid_user_id("alice"));
        assert!(valid_user_id("a.b@c-d_1"));
        assert!(!valid_user_id("bad user!"));
        assert!(!valid_user_id(""));
        assert!(!valid_user_id(&"a".repeat(65)));
        assert!(!valid_user_id("юзер"));
    }
}
