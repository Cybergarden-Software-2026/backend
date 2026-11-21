use axum::extract::{Json, Request, State, rejection::JsonRejection};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use dashboard_config::{CoordinatorAccessKey, Settings};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::coordinator_accounts::{self, Coordinator};
use crate::{ApiResult, AppError, AppState, Clock};

const COOKIE_NAME: &str = "__Host-underctrl-coordinator";
const COOKIE_ATTRIBUTES: &str = "Path=/; Secure; HttpOnly; SameSite=Strict";
const SESSION_SECONDS: i64 = 12 * 60 * 60;
const REMEMBER_SECONDS: i64 = 7 * 24 * 60 * 60;

#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum Login {
    Key(KeyLogin),
    Password(PasswordLogin),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyLogin {
    key: String,
    remember: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PasswordLogin {
    login: String,
    password: String,
    remember: bool,
}

#[derive(Clone, Copy)]
enum AuthMode<'a> {
    Accounts,
    SharedKey(&'a CoordinatorAccessKey),
    None,
}

impl<'a> AuthMode<'a> {
    fn from_settings(settings: &'a Settings) -> Self {
        if settings.coordinator_accounts_enabled {
            Self::Accounts
        } else if let Some(key) = &settings.coordinator_access_key {
            Self::SharedKey(key)
        } else {
            Self::None
        }
    }

    fn required(self) -> bool {
        !matches!(self, Self::None)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Accounts => "accounts",
            Self::SharedKey(_) => "shared-key",
            Self::None => "none",
        }
    }

    fn token_hash(self, token: &[u8; 32]) -> Option<[u8; 32]> {
        let digest = match self {
            Self::Accounts => Sha256::new().chain_update(b"underctrl:individual-coordinator-session:v1\0"),
            // Keep legacy tokens compatible during rollout. A configured key rotation invalidates them.
            Self::SharedKey(key) => Sha256::new()
                .chain_update(b"underctrl:coordinator-session:v1\0")
                .chain_update(Sha256::digest(key.as_bytes())),
            Self::None => return None,
        };
        Some(digest.chain_update(token).finalize().into())
    }
}

#[derive(Serialize)]
struct SessionStatus {
    required: bool,
    authenticated: bool,
    mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    coordinator: Option<Coordinator>,
}

enum SessionIdentity {
    Account(Coordinator),
    SharedKey,
}

#[derive(Clone)]
pub(crate) struct ActiveSession {
    pool: sqlx::PgPool,
    clock: Clock,
    hash: [u8; 32],
    accounts: bool,
}

impl ActiveSession {
    pub(crate) async fn is_valid(&self) -> Result<bool, sqlx::Error> {
        Ok(self.identity().await?.is_some())
    }

    async fn identity(&self) -> Result<Option<SessionIdentity>, sqlx::Error> {
        let now = (self.clock)();
        if self.accounts {
            let principal = sqlx::query_as::<_, (String, String)>(
                "SELECT accounts.login, accounts.display_name FROM coordinator_sessions sessions \
                 JOIN coordinator_accounts accounts ON accounts.login = sessions.account_login \
                 WHERE sessions.token_hash = $1 AND sessions.expires_at > $2 AND accounts.disabled_at IS NULL",
            )
            .bind(self.hash.as_slice())
            .bind(now)
            .fetch_optional(&self.pool)
            .await?;
            return Ok(principal.map(|account| SessionIdentity::Account(account.into())));
        }
        let valid: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM coordinator_sessions \
             WHERE token_hash = $1 AND expires_at > $2 AND account_login IS NULL)",
        )
        .bind(self.hash.as_slice())
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(valid.then_some(SessionIdentity::SharedKey))
    }
}

fn session(headers: &HeaderMap, state: &AppState, mode: AuthMode<'_>) -> Option<ActiveSession> {
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
        hash: mode.token_hash(&token)?,
        accounts: matches!(mode, AuthMode::Accounts),
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
    let mode = AuthMode::from_settings(&state.settings);
    if !mode.required() {
        return next.run(request).await;
    }
    let response = async {
        check_origin(request.method(), request.headers(), &state.settings)?;
        let access = session(request.headers(), &state, mode)
            .ok_or_else(|| unauthorized("Войдите как координатор."))?;
        let Some(identity) = access.identity().await? else {
            return Err(unauthorized("Сеанс координатора завершён. Войдите снова."));
        };
        if let SessionIdentity::Account(coordinator) = identity {
            request.extensions_mut().insert(coordinator);
        }
        request.extensions_mut().insert(access);
        Ok(next.run(request).await)
    }
    .await
    .unwrap_or_else(AppError::into_response);
    no_store(response)
}

fn status(mode: AuthMode<'_>, identity: Option<SessionIdentity>) -> Response {
    Json(SessionStatus {
        required: mode.required(),
        authenticated: identity.is_some(),
        mode: mode.label(),
        coordinator: match identity {
            Some(SessionIdentity::Account(coordinator)) => Some(coordinator),
            _ => None,
        },
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
    let mode = AuthMode::from_settings(&state.settings);
    let identity = match session(&headers, &state, mode) {
        Some(access) => access.identity().await?,
        None => None,
    };
    Ok(status(mode, identity))
}

pub(crate) async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Login>, JsonRejection>,
) -> ApiResult<Response> {
    let mode = AuthMode::from_settings(&state.settings);
    if !mode.required() {
        return Ok(status(mode, None));
    }
    check_origin(&Method::POST, &headers, &state.settings)?;
    let Json(input) =
        body.map_err(|_| AppError::bad_request("Укажите ключ или логин и пароль вместе с remember."))?;
    let invalid = || unauthorized("Неверные данные для входа.");
    let (login, remember) = match (mode, input) {
        (AuthMode::Accounts, Login::Key(input)) => {
            let login = coordinator_accounts::authenticate_key(&state.pool, input.key)
                .await?
                .ok_or_else(invalid)?;
            (Some(login), input.remember)
        }
        (AuthMode::Accounts, Login::Password(input)) => {
            let login = coordinator_accounts::authenticate_password(&state.pool, input.login, input.password)
                .await?
                .ok_or_else(invalid)?;
            (Some(login), input.remember)
        }
        (AuthMode::SharedKey(key), Login::Key(input))
            if bool::from(Sha256::digest(input.key.as_bytes()).ct_eq(&Sha256::digest(key.as_bytes()))) =>
        {
            (None, input.remember)
        }
        _ => return Err(invalid()),
    };

    let mut token = [0; 32];
    getrandom::fill(&mut token).map_err(|_| AppError::internal())?;
    let now = (state.clock)();
    let ttl = if remember {
        REMEMBER_SECONDS
    } else {
        SESSION_SECONDS
    };
    let expires = now + chrono::TimeDelta::seconds(ttl);
    let mut tx = state.pool.begin().await?;
    let identity = if let Some(login) = &login {
        let account = sqlx::query_as::<_, (String, String)>(
            "SELECT login, display_name FROM coordinator_accounts WHERE login = $1 AND disabled_at IS NULL FOR SHARE",
        )
        .bind(login)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(invalid)?;
        SessionIdentity::Account(account.into())
    } else {
        SessionIdentity::SharedKey
    };
    sqlx::query("DELETE FROM coordinator_sessions WHERE expires_at <= $1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    if let Some(previous) = session(&headers, &state, mode) {
        sqlx::query("DELETE FROM coordinator_sessions WHERE token_hash = $1")
            .bind(previous.hash.as_slice())
            .execute(&mut *tx)
            .await?;
    }
    let hash = mode.token_hash(&token).ok_or_else(AppError::internal)?;
    sqlx::query(
        "INSERT INTO coordinator_sessions (token_hash, expires_at, account_login) VALUES ($1, $2, $3)",
    )
    .bind(hash.as_slice())
    .bind(expires)
    .bind(login)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let mut result = status(mode, Some(identity));
    result.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&hex::encode(token), remember.then_some(REMEMBER_SECONDS))?,
    );
    Ok(result)
}

pub(crate) async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    let mode = AuthMode::from_settings(&state.settings);
    if mode.required() {
        check_origin(&Method::DELETE, &headers, &state.settings)?;
        if let Some(access) = session(&headers, &state, mode) {
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
