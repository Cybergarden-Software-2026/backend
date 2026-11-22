use axum::Extension;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use dashboard_domain::constants::DashboardEventReason;
use dashboard_domain::projection::CoordinationClaim;
use dashboard_store::{import_repo, read_repo};
use serde_json::json;

use crate::coordinator_accounts::Coordinator;
use crate::response::PyJson;
use crate::{ApiResult, AppError, AppState, dashboard};

pub(crate) fn already_claimed(claim: &CoordinationClaim) -> AppError {
    AppError::conflict("Инцидент уже в работе у другого координатора.")
        .with_code("COORDINATION_ALREADY_CLAIMED")
        .with_detail("details", json!({"coordinationClaim": claim}))
}

pub(crate) async fn claim(
    State(state): State<AppState>,
    Path(sys_id): Path<String>,
    account: Option<Extension<Coordinator>>,
) -> ApiResult<PyJson> {
    update(state, &sys_id, account, true).await
}

pub(crate) async fn release(
    State(state): State<AppState>,
    Path(sys_id): Path<String>,
    account: Option<Extension<Coordinator>>,
) -> ApiResult<PyJson> {
    update(state, &sys_id, account, false).await
}

fn personal_session_required() -> AppError {
    AppError::new(
        StatusCode::UNAUTHORIZED,
        "COORDINATOR_UNAUTHORIZED",
        "Войдите в личный аккаунт координатора.",
    )
}

async fn update(
    state: AppState,
    sys_id: &str,
    account: Option<Extension<Coordinator>>,
    take: bool,
) -> ApiResult<PyJson> {
    let Extension(account) = account.ok_or_else(personal_session_required)?;
    if sys_id.trim().is_empty() || sys_id.chars().count() > 64 {
        return Err(AppError::validation("Укажите корректный sysId инцидента."));
    }

    // Serialize claim, routing and import writes and publish the revision in the same transaction.
    let mut tx = state.pool.begin().await?;
    let source = import_repo::lock_state(&mut tx).await?;
    if source.last_success_at.is_none() && !import_repo::forms_have_incidents(&mut tx).await? {
        return Err(AppError::source_not_ready(
            "Первый снимок ещё не принят, повторите запрос позже",
        ));
    }
    let login: Option<String> = sqlx::query_scalar(
        "SELECT login FROM coordinator_accounts WHERE login = $1 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(&account.login)
    .fetch_optional(&mut *tx)
    .await?;
    let login = login.ok_or_else(personal_session_required)?;
    let incident = import_repo::stored_by_sys_id(&mut tx, sys_id)
        .await?
        .ok_or_else(dashboard::incident_not_found)?;
    let team = incident.assigned_group_name.as_deref().map(str::trim);
    if !incident.normalized_state.is_attention()
        || team != Some(state.settings.coordinator_group_name.as_str())
    {
        return Err(
            AppError::conflict("Этот инцидент сейчас не в очереди координаторов.")
                .with_code("COORDINATION_CLAIM_UNAVAILABLE"),
        );
    }
    let current = read_repo::coordination_claim(&mut tx, incident.pk).await?;
    let now = (state.clock)();
    let changed = match (take, current.as_ref()) {
        (true, Some(claim)) if claim.login != login => return Err(already_claimed(claim)),
        (false, Some(claim)) if claim.login != login => {
            return Err(
                AppError::conflict("Снять отметку может координатор, который взял инцидент.")
                    .with_code("COORDINATION_CLAIM_NOT_OWNED")
                    .with_detail("details", json!({"coordinationClaim": claim})),
            );
        }
        (true, None) => {
            // A disabled account's claim is already invisible to readers and can be replaced.
            sqlx::query(
                "INSERT INTO incident_coordination_claims (incident_pk, account_login, claimed_at)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (incident_pk) DO UPDATE
                 SET account_login = excluded.account_login, claimed_at = excluded.claimed_at",
            )
            .bind(incident.pk)
            .bind(&login)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            true
        }
        (false, Some(_)) => {
            sqlx::query("DELETE FROM incident_coordination_claims WHERE incident_pk = $1")
                .bind(incident.pk)
                .execute(&mut *tx)
                .await?;
            true
        }
        _ => false,
    };
    let (claim, revision) = if changed {
        let revision = import_repo::new_dashboard_event(
            &mut tx,
            DashboardEventReason::CoordinationClaim,
            &json!({"schemaVersion": 1, "sysId": sys_id}),
            now,
        )
        .await?;
        (
            read_repo::coordination_claim(&mut tx, incident.pk).await?,
            revision,
        )
    } else {
        (current, read_repo::revision(&mut tx).await?)
    };
    tx.commit().await?;
    Ok(PyJson(json!({"coordinationClaim": claim, "revision": revision})))
}
