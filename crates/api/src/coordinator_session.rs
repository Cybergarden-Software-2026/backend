use axum::extract::{Json, Request, State, rejection::JsonRejection};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use dashboard_config::{CoordinatorAccessKey, Settings};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{ApiResult, AppError, AppState, Clock};

const COOKIE_NAME: &str = "__Host-underctrl-coordinator";
const COOKIE_ATTRIBUTES: &str = "Path=/; Secure; HttpOnly; SameSite=Strict";
const SESSION_SECONDS: i64 = 12 * 60 * 60;
const REMEMBER_SECONDS: i64 = 7 * 24 * 60 * 60;

#[derive(Deserialize)]
pub(crate) struct Login {
    key: String,
    remember: bool,
}

#[derive(Serialize)]
struct SessionStatus {
    required: bool,
    authenticated: bool,
}

#[derive(Clone)]
pub(crate) struct ActiveSession {
    pool: sqlx::PgPool,
    clock: Clock,
    hash: [u8; 32],
}

impl ActiveSession {
    pub(crate) async fn is_valid(&self) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM coordinator_sessions WHERE token_hash = $1 AND expires_at > $2)",
        )
        .bind(self.hash.as_slice())
        .bind((self.clock)())
        .fetch_one(&self.pool)
        .await
    }
}

fn token_hash(key: &CoordinatorAccessKey, token: &[u8; 32]) -> [u8; 32] {
    // Fixed-size hashes and random tokens. Rotating the configured key also invalidates issued cookies.
    Sha256::new()
        .chain_update(b"underctrl:coordinator-session:v1\0")
        .chain_update(Sha256::digest(key.as_bytes()))
        .chain_update(token)
        .finalize()
        .into()
}

fn session(headers: &HeaderMap, state: &AppState, key: &CoordinatorAccessKey) -> Option<ActiveSession> {
    let value = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|cookie| cookie.trim().split_once('='))
        .find_map(|(name, value)| (name == COOKIE_NAME).then_some(value))?;
    let mut token = [0; 32];
    hex::decode_to_slice(value, &mut token).ok()?;
    Some(ActiveSession {
        pool: state.pool.clone(),
        clock: state.clock.clone(),
        hash: token_hash(key, &token),
    })
}

pub(crate) fn browser_origin(value: &str) -> Option<Uri> {
    let uri: Uri = value.parse().ok()?;
    (matches!(uri.scheme_str(), Some("http" | "https"))
        && uri
            .authority()
            .is_some_and(|authority| !authority.as_str().contains('@'))
        && uri.path() == "/"
        && uri.query().is_none()
        && !value.ends_with('/'))
    .then_some(uri)
}

fn check_origin(method: &Method, headers: &HeaderMap, settings: &Settings) -> ApiResult<()> {
    if method.is_safe() {
        return Ok(());
    }
    let fetch_site = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok());
    let allowed = match headers.get(header::ORIGIN) {
        // Command-line API clients need not send browser fetch metadata.
        None => !matches!(fetch_site, Some("cross-site" | "same-site")),
        Some(value) => value.to_str().ok().and_then(browser_origin).is_some_and(|uri| {
            fetch_site == Some("same-origin")
                || settings
                    .cors_origins
                    .iter()
                    .any(|origin| origin.as_bytes() == value.as_bytes())
                || headers
                    .get(header::HOST)
                    .and_then(|host| host.to_str().ok())
                    .is_some_and(|host| {
                        uri.authority()
                            .is_some_and(|origin| origin.as_str().eq_ignore_ascii_case(host))
                    })
        }),
    };
    if allowed {
        Ok(())
    } else {
        Err(AppError::new(
            StatusCode::FORBIDDEN,
            "COORDINATOR_ORIGIN_FORBIDDEN",
            "Запрос с этого сайта не разрешён.",
        ))
    }
}

fn unauthorized(message: &'static str) -> AppError {
    AppError::new(StatusCode::UNAUTHORIZED, "COORDINATOR_UNAUTHORIZED", message)
}

fn no_store(mut response: Response) -> Response {
    let value = if response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"text/event-stream"))
    {
        "no-store, no-transform"
    } else {
        "no-store"
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    response
}

pub(crate) async fn auth_headers(request: Request, next: Next) -> Response {
    no_store(next.run(request).await)
}

pub(crate) async fn protect(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let Some(key) = &state.settings.coordinator_access_key else {
        return next.run(request).await;
    };
    let response = async {
        check_origin(request.method(), request.headers(), &state.settings)?;
        let access = session(request.headers(), &state, key)
            .ok_or_else(|| unauthorized("Войдите как координатор."))?;
        if !access.is_valid().await? {
            return Err(unauthorized("Сеанс координатора завершён. Войдите снова."));
        }
        request.extensions_mut().insert(access);
        Ok(next.run(request).await)
    }
    .await
    .unwrap_or_else(AppError::into_response);
    no_store(response)
}

fn status(required: bool, authenticated: bool) -> Response {
    Json(SessionStatus {
        required,
        authenticated,
    })
    .into_response()
}

fn session_cookie(value: &str, max_age: Option<i64>) -> Result<HeaderValue, AppError> {
    let mut cookie = format!("{COOKIE_NAME}={value}; {COOKIE_ATTRIBUTES}");
    if let Some(seconds) = max_age {
        cookie.push_str(&format!("; Max-Age={seconds}"));
    }
    HeaderValue::from_str(&cookie).map_err(|_| AppError::internal())
}

pub(crate) async fn get(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    let Some(key) = &state.settings.coordinator_access_key else {
        return Ok(status(false, false));
    };
    let authenticated = match session(&headers, &state, key) {
        Some(access) => access.is_valid().await?,
        None => false,
    };
    Ok(status(true, authenticated))
}

pub(crate) async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Login>, JsonRejection>,
) -> ApiResult<Response> {
    let Some(key) = &state.settings.coordinator_access_key else {
        return Ok(status(false, false));
    };
    check_origin(&Method::POST, &headers, &state.settings)?;
    let Json(input) = body.map_err(|_| AppError::bad_request("Ожидаются key и remember в JSON."))?;
    if !bool::from(Sha256::digest(input.key.as_bytes()).ct_eq(&Sha256::digest(key.as_bytes()))) {
        return Err(unauthorized("Ключ координатора не подошёл."));
    }

    let mut token = [0; 32];
    getrandom::fill(&mut token).map_err(|_| AppError::internal())?;
    let now = (state.clock)();
    let ttl = if input.remember {
        REMEMBER_SECONDS
    } else {
        SESSION_SECONDS
    };
    let expires = now + chrono::TimeDelta::seconds(ttl);
    let mut tx = state.pool.begin().await?;
    sqlx::query("DELETE FROM coordinator_sessions WHERE expires_at <= $1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    if let Some(previous) = session(&headers, &state, key) {
        sqlx::query("DELETE FROM coordinator_sessions WHERE token_hash = $1")
            .bind(previous.hash.as_slice())
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("INSERT INTO coordinator_sessions (token_hash, expires_at) VALUES ($1, $2)")
        .bind(token_hash(key, &token).as_slice())
        .bind(expires)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let mut result = status(true, true);
    result.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&hex::encode(token), input.remember.then_some(REMEMBER_SECONDS))?,
    );
    Ok(result)
}

pub(crate) async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(key) = &state.settings.coordinator_access_key {
        check_origin(&Method::DELETE, &headers, &state.settings)?;
        if let Some(access) = session(&headers, &state, key) {
            sqlx::query("DELETE FROM coordinator_sessions WHERE token_hash = $1")
                .bind(access.hash.as_slice())
                .execute(&state.pool)
                .await?;
        }
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session_cookie("", Some(0))?)],
    )
        .into_response())
}
