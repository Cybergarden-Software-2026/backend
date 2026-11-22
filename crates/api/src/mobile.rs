//! One-time pairing of a notification-only device to a personal coordinator account.

use axum::extract::{DefaultBodyLimit, Path, Request, State, rejection::JsonRejection};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::coordinator_accounts::Coordinator;
use crate::{ApiResult, AppError, AppState, coordinator_session};

pub(crate) fn routes(state: AppState) -> Router<AppState> {
    let account_routes = Router::new()
        .route("/mobile/pairings", post(issue_pairing))
        .route("/mobile/devices", get(list_devices))
        .route("/mobile/devices/{device_id}", delete(revoke_device))
        .route_layer(from_fn_with_state(state.clone(), coordinator_session::protect));

    account_routes
        .merge(
            Router::new()
                .route("/mobile/devices", post(register))
                .route("/mobile/devices/current/token", put(refresh_token))
                .route("/mobile/devices/current", delete(unregister)),
        )
        .layer(DefaultBodyLimit::max(8192))
        .route_layer(from_fn_with_state(state, require_mobile_accounts))
        .route_layer(from_fn(coordinator_session::auth_headers))
}

async fn require_mobile_accounts(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if state.settings.mobile_push_enabled && state.settings.coordinator_accounts_enabled {
        next.run(request).await
    } else {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "MOBILE_PUSH_DISABLED",
            "Мобильные уведомления отключены.",
        )
        .into_response()
    }
}

const PAIRING_PREFIX: &str = "ucp_";
const DEVICE_PREFIX: &str = "ucd_";

fn generate_secret(prefix: &str) -> ApiResult<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| AppError::internal())?;
    Ok(format!("{prefix}{}", hex::encode(bytes)))
}

fn secret_hash(value: &str, prefix: &str) -> Option<[u8; 32]> {
    let digits = value.strip_prefix(prefix)?;
    (digits.len() == 64 && digits.bytes().all(|c| c.is_ascii_hexdigit()))
        .then(|| Sha256::digest(value.as_bytes()).into())
}

fn invalid_pairing() -> AppError {
    AppError::new(
        StatusCode::UNAUTHORIZED,
        "MOBILE_PAIRING_INVALID",
        "Код подключения недействителен. Откройте новый QR-код.",
    )
}

fn unauthorized_device() -> AppError {
    AppError::new(
        StatusCode::UNAUTHORIZED,
        "MOBILE_DEVICE_UNAUTHORIZED",
        "Устройство не авторизовано.",
    )
}

fn device_hash(headers: &HeaderMap) -> ApiResult<[u8; 32]> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .and_then(|(_, token)| secret_hash(token, DEVICE_PREFIX))
        .ok_or_else(unauthorized_device)
}

fn token_in_use() -> AppError {
    AppError::conflict("Этот FCM-токен уже зарегистрирован.").with_code("FCM_TOKEN_IN_USE")
}

fn valid_fcm_token(token: &str) -> ApiResult<()> {
    if (1..=2048).contains(&token.len()) && token.bytes().all(|c| c.is_ascii_graphic()) {
        Ok(())
    } else {
        Err(AppError::validation("Укажите корректный FCM-токен."))
    }
}

fn json_error(err: JsonRejection) -> AppError {
    match err.status() {
        StatusCode::UNSUPPORTED_MEDIA_TYPE => AppError::new(
            err.status(),
            "UNSUPPORTED_MEDIA_TYPE",
            "Передайте запрос с Content-Type: application/json.",
        ),
        StatusCode::PAYLOAD_TOO_LARGE => AppError::http(err.status()).with_code("PAYLOAD_TOO_LARGE"),
        _ => AppError::validation("Некорректный формат запроса."),
    }
}

async fn issue_pairing(
    State(state): State<AppState>,
    Extension(account): Extension<Coordinator>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let code = generate_secret(PAIRING_PREFIX)?;
    let hash = Sha256::digest(code.as_bytes());
    let expires_at = (state.clock)() + TimeDelta::minutes(5);
    let result = sqlx::query(
        "INSERT INTO mobile_pairings(account_login, code_hash, expires_at)
         SELECT login, $2, $3 FROM coordinator_accounts WHERE login = $1 AND disabled_at IS NULL FOR SHARE
         ON CONFLICT (account_login) DO UPDATE
           SET code_hash = EXCLUDED.code_hash, expires_at = EXCLUDED.expires_at",
    )
    .bind(account.login)
    .bind(&hash[..])
    .bind(expires_at)
    .execute(&state.pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "COORDINATOR_UNAUTHORIZED",
            "Аккаунт отключён.",
        ));
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({"code":code, "expiresAt":expires_at})),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Platform {
    Android,
    Web,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FcmTargetType {
    #[default]
    Token,
    Fid,
}

impl FcmTargetType {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::Fid => "fid",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Registration {
    code: String,
    platform: Platform,
    fcm_token: String,
    #[serde(default)]
    fcm_target_type: FcmTargetType,
}

async fn register(
    State(state): State<AppState>,
    body: Result<Json<Registration>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let Json(input) = body.map_err(json_error)?;
    valid_fcm_token(&input.fcm_token)?;
    let code_hash = secret_hash(&input.code, PAIRING_PREFIX).ok_or_else(invalid_pairing)?;
    let mut tx = state.pool.begin().await?;
    // Take the account lock before touching its pairing or devices, as account disablement does.
    let login: String = sqlx::query_scalar(
        "SELECT a.login FROM coordinator_accounts a JOIN mobile_pairings p ON p.account_login = a.login
         WHERE a.disabled_at IS NULL AND p.code_hash = $1 AND p.expires_at > $2 FOR SHARE OF a",
    )
    .bind(code_hash.as_slice())
    .bind((state.clock)())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(invalid_pairing)?;
    // A concurrent claim or replacement can win after the lookup. Consume the exact code once.
    let consumed = sqlx::query(
        "DELETE FROM mobile_pairings WHERE account_login = $1 AND code_hash = $2 AND expires_at > $3",
    )
    .bind(&login)
    .bind(code_hash.as_slice())
    .bind((state.clock)())
    .execute(&mut *tx)
    .await?;
    if consumed.rows_affected() == 0 {
        return Err(invalid_pairing());
    }
    let token = generate_secret(DEVICE_PREFIX)?;
    let hash = Sha256::digest(token.as_bytes());
    let id = Uuid::new_v4();
    let platform = match input.platform {
        Platform::Android => "android",
        Platform::Web => "web",
    };
    let result = sqlx::query(
        "INSERT INTO mobile_devices(id, account_login, platform, fcm_token, secret_hash, fcm_target_type)
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (fcm_token) DO NOTHING",
    )
    .bind(id)
    .bind(login)
    .bind(platform)
    .bind(input.fcm_token)
    .bind(&hash[..])
    .bind(input.fcm_target_type.as_str())
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(token_in_use());
    }
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"deviceId":id.to_string(), "deviceToken":token})),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TokenUpdate {
    fcm_token: String,
    #[serde(default)]
    fcm_target_type: FcmTargetType,
}

async fn refresh_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<TokenUpdate>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let hash = device_hash(&headers)?;
    let Json(input) = body.map_err(json_error)?;
    valid_fcm_token(&input.fcm_token)?;
    let result = sqlx::query(
        "UPDATE mobile_devices d SET fcm_token = $2, fcm_target_type = $3 FROM coordinator_accounts a
         WHERE d.secret_hash = $1 AND a.login = d.account_login AND a.disabled_at IS NULL",
    )
    .bind(hash.as_slice())
    .bind(input.fcm_token)
    .bind(input.fcm_target_type.as_str())
    .execute(&state.pool)
    .await
    .map_err(|err| {
        if err.as_database_error().is_some_and(|e| e.is_unique_violation()) {
            token_in_use()
        } else {
            AppError::from(err)
        }
    })?;
    if result.rows_affected() == 0 {
        return Err(unauthorized_device());
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn unregister(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let hash = device_hash(&headers)?;
    let result = sqlx::query(
        "DELETE FROM mobile_devices d USING coordinator_accounts a
         WHERE d.secret_hash = $1 AND a.login = d.account_login AND a.disabled_at IS NULL",
    )
    .bind(hash.as_slice())
    .execute(&state.pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(unauthorized_device());
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list_devices(
    State(state): State<AppState>,
    Extension(account): Extension<Coordinator>,
) -> ApiResult<Json<Value>> {
    let devices = sqlx::query_as::<_, (Uuid, String, DateTime<Utc>, bool)>(
        "SELECT id, platform, created_at, fcm_token IS NOT NULL FROM mobile_devices
         WHERE account_login = $1 ORDER BY created_at DESC, id",
    )
    .bind(account.login)
    .fetch_all(&state.pool)
    .await?;
    let items: Vec<_> = devices.into_iter().map(|(id, platform, created, active)| {
        json!({"deviceId":id.to_string(), "platform":platform, "createdAt":created, "pushEnabled":active})
    }).collect();
    Ok(Json(json!({"items":items})))
}

async fn revoke_device(
    State(state): State<AppState>,
    Extension(account): Extension<Coordinator>,
    Path(raw_id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = Uuid::parse_str(&raw_id)
        .map_err(|_| AppError::validation("Некорректный идентификатор устройства."))?;
    let result = sqlx::query("DELETE FROM mobile_devices WHERE id = $1 AND account_login = $2")
        .bind(id)
        .bind(account.login)
        .execute(&state.pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::not_found("Устройство не найдено.").with_code("MOBILE_DEVICE_NOT_FOUND"));
    }
    Ok(StatusCode::NO_CONTENT)
}
