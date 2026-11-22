//! Sign in a mobile installation with a one-time QR code from a personal coordinator account.

use axum::extract::{DefaultBodyLimit, Path, Request, State, rejection::JsonRejection};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::coordinator_accounts::Coordinator;
use crate::{ApiResult, AppError, AppState, coordinator_session};

pub(crate) fn routes(state: AppState) -> Router<AppState> {
    let account_routes = Router::new()
        .route("/mobile/pairings", post(issue_pairing))
        .route("/mobile/devices", get(list_devices))
        .route("/mobile/devices/{device_id}", delete(revoke_device))
        .route_layer(from_fn_with_state(state.clone(), coordinator_session::protect));

    let account_data = Router::new()
        .route("/mobile/incidents", get(crate::mobile_incidents::list))
        .route(
            "/mobile/incidents/{sys_id}/coordination-claim",
            put(crate::coordination_claims::claim).delete(crate::coordination_claims::release),
        )
        .route(
            "/mobile/incidents/{sys_id}/team-contact",
            post(crate::team_contact::record),
        )
        .route(
            "/mobile/notifications",
            get(crate::notifications::list_notifications),
        )
        .route(
            "/mobile/notifications/{notification_id}/read",
            put(crate::notifications::mark_read),
        )
        .route_layer(from_fn_with_state(state.clone(), authenticate_session));

    let sessions = account_routes
        .merge(account_data)
        .route("/mobile/session", post(login).get(current_session).delete(logout))
        .route("/mobile/devices/current", delete(unregister))
        .route_layer(from_fn_with_state(state.clone(), require_accounts));

    let push = Router::new()
        .route("/mobile/devices", post(register))
        .route("/mobile/devices/current/token", put(refresh_token))
        .route_layer(from_fn_with_state(state, require_push));

    sessions
        .merge(push)
        .layer(DefaultBodyLimit::max(8192))
        .route_layer(from_fn(coordinator_session::auth_headers))
}

async fn require_accounts(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> ApiResult<Response> {
    if !state.settings.coordinator_accounts_enabled {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "MOBILE_ACCOUNTS_DISABLED",
            "Для мобильного входа нужны личные учётные записи.",
        ));
    }
    Ok(next.run(request).await)
}

async fn require_push(State(state): State<AppState>, request: Request, next: Next) -> ApiResult<Response> {
    if !state.settings.mobile_push_enabled || !state.settings.coordinator_accounts_enabled {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "MOBILE_PUSH_DISABLED",
            "Мобильные уведомления отключены.",
        ));
    }
    Ok(next.run(request).await)
}

const PAIRING_PREFIX: &str = "ucp_";
const DEVICE_PREFIX: &str = "ucd_";
const SESSION_PREFIX: &str = "ucm_";

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

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token)
}

fn device_hash(headers: &HeaderMap) -> ApiResult<[u8; 32]> {
    bearer(headers)
        .and_then(|token| secret_hash(token, SESSION_PREFIX).or_else(|| secret_hash(token, DEVICE_PREFIX)))
        .ok_or_else(unauthorized_device)
}

fn unauthorized_session() -> AppError {
    AppError::new(
        StatusCode::UNAUTHORIZED,
        "MOBILE_SESSION_UNAUTHORIZED",
        "Требуется вход по QR-коду.",
    )
}

fn session_hash(headers: &HeaderMap) -> ApiResult<[u8; 32]> {
    bearer(headers)
        .and_then(|token| secret_hash(token, SESSION_PREFIX))
        .ok_or_else(unauthorized_session)
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

pub(crate) fn json_error(err: JsonRejection) -> AppError {
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

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Platform {
    #[default]
    Android,
    Web,
}

impl Platform {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Android => "android",
            Self::Web => "web",
        }
    }
}

// The caller commits only after successfully registering or signing in the installation.
async fn claim_pairing(
    state: &AppState,
    code: &str,
) -> ApiResult<(Transaction<'static, Postgres>, Coordinator)> {
    let code_hash = secret_hash(code, PAIRING_PREFIX).ok_or_else(invalid_pairing)?;
    let mut tx = state.pool.begin().await?;
    // Take the account lock before touching its pairing or devices, as account disablement does.
    let owner: (String, String) = sqlx::query_as(
        "SELECT a.login, a.display_name FROM coordinator_accounts a JOIN mobile_pairings p ON p.account_login = a.login
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
    .bind(&owner.0)
    .bind(code_hash.as_slice())
    .bind((state.clock)())
    .execute(&mut *tx)
    .await?;
    if consumed.rows_affected() == 0 {
        return Err(invalid_pairing());
    }
    Ok((tx, owner.into()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Login {
    code: String,
    device_id: String,
    #[serde(default)]
    platform: Platform,
}

async fn login(
    State(state): State<AppState>,
    body: Result<Json<Login>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let Json(input) = body.map_err(json_error)?;
    if !(1..=128).contains(&input.device_id.len()) || !input.device_id.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(AppError::validation(
            "deviceId должен содержать от 1 до 128 печатных ASCII-символов без пробелов.",
        ));
    }
    let (mut tx, owner) = claim_pairing(&state, &input.code).await?;
    let token = generate_secret(SESSION_PREFIX)?;
    let hash = Sha256::digest(token.as_bytes());
    // Only the account identified by the QR can rotate this installation's bearer.
    let result = sqlx::query(
        "INSERT INTO mobile_devices (client_device_id, account_login, platform, secret_hash)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (client_device_id) DO UPDATE SET secret_hash = EXCLUDED.secret_hash
         WHERE mobile_devices.account_login = EXCLUDED.account_login",
    )
    .bind(&input.device_id)
    .bind(&owner.login)
    .bind(input.platform.as_str())
    .bind(&hash[..])
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::conflict("Устройство уже привязано к другому аккаунту.")
            .with_code("MOBILE_DEVICE_IN_USE"));
    }
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "token": token, "tokenType": "Bearer", "deviceId": input.device_id, "user": owner,
        })),
    ))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MobileSession {
    device_id: String,
    user: Coordinator,
}

async fn active_session(state: &AppState, headers: &HeaderMap) -> ApiResult<MobileSession> {
    let hash = session_hash(headers)?;
    let (device_id, login, display_name): (String, String, String) = sqlx::query_as(
        "SELECT d.client_device_id, a.login, a.display_name
         FROM mobile_devices d JOIN coordinator_accounts a ON a.login = d.account_login
         WHERE d.secret_hash = $1 AND d.client_device_id IS NOT NULL AND a.disabled_at IS NULL",
    )
    .bind(hash.as_slice())
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(unauthorized_session)?;
    Ok(MobileSession {
        device_id,
        user: (login, display_name).into(),
    })
}

async fn current_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<MobileSession>> {
    Ok(Json(active_session(&state, &headers).await?))
}

async fn authenticate_session(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> ApiResult<Response> {
    let session = active_session(&state, request.headers()).await?;
    request.extensions_mut().insert(session.user);
    Ok(next.run(request).await)
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
    let (mut tx, owner) = claim_pairing(&state, &input.code).await?;
    let token = generate_secret(DEVICE_PREFIX)?;
    let hash = Sha256::digest(token.as_bytes());
    let id = Uuid::new_v4();
    let result = sqlx::query(
        "INSERT INTO mobile_devices(id, account_login, platform, fcm_token, secret_hash, fcm_target_type)
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (fcm_token) DO NOTHING",
    )
    .bind(id)
    .bind(owner.login)
    .bind(input.platform.as_str())
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

async fn remove_registration(state: &AppState, hash: &[u8; 32]) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM mobile_devices d USING coordinator_accounts a
         WHERE d.secret_hash = $1 AND a.login = d.account_login AND a.disabled_at IS NULL",
    )
    .bind(hash.as_slice())
    .execute(&state.pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<StatusCode> {
    if !remove_registration(&state, &session_hash(&headers)?).await? {
        return Err(unauthorized_session());
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn unregister(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<StatusCode> {
    if !remove_registration(&state, &device_hash(&headers)?).await? {
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
