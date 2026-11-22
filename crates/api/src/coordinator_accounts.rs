//! Individual coordinator credentials and account administration.

use std::collections::HashSet;

use anyhow::{Context, bail};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use dashboard_domain::constants::DashboardEventReason;
use dashboard_store::import_repo;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::{ApiResult, AppError};

const KEY_PREFIX: &str = "uck_";
const DUMMY_PASSWORD_HASH: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
static PASSWORD_CHECKS: Semaphore = Semaphore::const_new(4);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Coordinator {
    pub(crate) login: String,
    pub(crate) display_name: String,
}

impl From<(String, String)> for Coordinator {
    fn from((login, display_name): (String, String)) -> Self {
        Self { login, display_name }
    }
}

pub struct AccountSpec<'a> {
    pub login: &'a str,
    pub display_name: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuedCredentials {
    pub login: String,
    pub display_name: String,
    pub password: String,
    pub key: String,
}

fn normalized_login(value: &str) -> Option<String> {
    let login = value.trim();
    ((1..=64).contains(&login.len())
        && login
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'@' | b'-')))
    .then(|| login.to_ascii_lowercase())
}

fn rng<const N: usize>() -> anyhow::Result<[u8; N]> {
    let mut random = [0; N];
    getrandom::fill(&mut random).map_err(|err| anyhow::anyhow!("cannot generate randomness: {err}"))?;
    Ok(random)
}

fn password_hasher() -> Argon2<'static> {
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(19_456, 2, 1, None).expect("valid Argon2id parameters"),
    )
}

/// Provision all requested accounts in one transaction. Existing logins do not issue credentials.
pub async fn create_accounts(
    pool: &PgPool,
    accounts: &[AccountSpec<'_>],
) -> anyhow::Result<Vec<IssuedCredentials>> {
    let mut seen = HashSet::new();
    let mut specs = Vec::new();
    for account in accounts {
        let login = normalized_login(account.login).context("invalid coordinator login")?;
        if !seen.insert(login.clone()) {
            bail!("duplicate coordinator login in batch");
        }
        let name = account.display_name.trim();
        if !(1..=100).contains(&name.chars().count()) || name.chars().any(char::is_control) {
            bail!("invalid coordinator display name");
        }
        specs.push((login, name.to_owned()));
    }

    let prepared = tokio::task::spawn_blocking(move || {
        specs
            .into_iter()
            .map(|(login, display_name)| {
                let password = hex::encode(rng::<20>()?);
                let key = format!("{KEY_PREFIX}{}", hex::encode(rng::<32>()?));
                let salt = SaltString::encode_b64(&rng::<16>()?)
                    .map_err(|_| anyhow::anyhow!("cannot encode password salt"))?;
                let hash = password_hasher()
                    .hash_password(password.as_bytes(), &salt)
                    .map_err(|_| anyhow::anyhow!("cannot hash coordinator password"))?
                    .to_string();
                Ok((
                    IssuedCredentials {
                        login,
                        display_name,
                        password,
                        key,
                    },
                    hash,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()
    })
    .await??;

    let mut tx = pool.begin().await?;
    let mut issued = Vec::new();
    for (credentials, password_hash) in prepared {
        let key_hash = Sha256::digest(credentials.key.as_bytes());
        let result = sqlx::query(
            "INSERT INTO coordinator_accounts (login, display_name, password_hash, key_hash) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (login) DO NOTHING",
        )
        .bind(&credentials.login)
        .bind(&credentials.display_name)
        .bind(password_hash)
        .bind(&key_hash[..])
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 1 {
            issued.push(credentials);
        }
    }
    tx.commit().await?;
    Ok(issued)
}

/// Revoke the account's sessions, devices and incident claims.
pub async fn disable_account(pool: &PgPool, login: &str) -> anyhow::Result<bool> {
    let login = normalized_login(login).context("invalid coordinator login")?;
    let mut tx = pool.begin().await?;
    import_repo::lock_state(&mut tx).await?;
    let disabled = sqlx::query(
        "UPDATE coordinator_accounts SET disabled_at = now() WHERE login = $1 AND disabled_at IS NULL",
    )
    .bind(&login)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if disabled {
        sqlx::query("DELETE FROM mobile_pairings WHERE account_login = $1")
            .bind(&login)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mobile_devices WHERE account_login = $1")
            .bind(&login)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM coordinator_sessions WHERE account_login = $1")
            .bind(&login)
            .execute(&mut *tx)
            .await?;
        let claims = sqlx::query("DELETE FROM incident_coordination_claims WHERE account_login = $1")
            .bind(&login)
            .execute(&mut *tx)
            .await?;
        if claims.rows_affected() > 0 {
            import_repo::new_dashboard_event(
                &mut tx,
                DashboardEventReason::CoordinationClaim,
                &json!({"schemaVersion": 1}),
                (crate::system_clock())(),
            )
            .await?;
        }
    }
    tx.commit().await?;
    Ok(disabled)
}

pub async fn has_enabled_account(pool: &PgPool) -> anyhow::Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM coordinator_accounts WHERE disabled_at IS NULL)")
            .fetch_one(pool)
            .await?,
    )
}

pub(crate) async fn authenticate_key(pool: &PgPool, key: String) -> ApiResult<Option<String>> {
    let Some(value) = key.strip_prefix(KEY_PREFIX) else {
        return Ok(None);
    };
    if value.len() != 64 || !value.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let hash = Sha256::digest(key.as_bytes());
    Ok(sqlx::query_scalar(
        "SELECT login FROM coordinator_accounts WHERE key_hash = $1 AND disabled_at IS NULL",
    )
    .bind(&hash[..])
    .fetch_optional(pool)
    .await?)
}

pub(crate) async fn authenticate_password(
    pool: &PgPool,
    login: String,
    password: String,
) -> ApiResult<Option<String>> {
    if password.is_empty() || password.len() > 1024 {
        return Ok(None);
    }
    let account = match normalized_login(&login) {
        Some(login) => sqlx::query_as::<_, (String, String)>(
            "SELECT login, password_hash FROM coordinator_accounts WHERE login = $1 AND disabled_at IS NULL",
        )
        .bind(login)
        .fetch_optional(pool)
        .await?,
        None => None,
    };
    let permit = PASSWORD_CHECKS.try_acquire().map_err(|_| {
        AppError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "COORDINATOR_LOGIN_BUSY",
            "Слишком много запросов входа. Повторите попытку позже.",
        )
    })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let hash = account
            .as_ref()
            .map_or(DUMMY_PASSWORD_HASH, |(_, hash)| hash.as_str());
        let parsed = PasswordHash::new(hash).map_err(|_| AppError::internal())?;
        let valid = password_hasher()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
        Ok(account.filter(|_| valid).map(|(login, _)| login))
    })
    .await
    .map_err(|_| AppError::internal())?
}
