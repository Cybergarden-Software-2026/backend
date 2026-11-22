use axum::Extension;
use axum::extract::{Json, OriginalUri, State, rejection::JsonRejection};
use dashboard_importer::assignment;
use dashboard_store::{import_repo, read_repo};
use serde::Deserialize;
use serde_json::json;

use crate::coordinator_accounts::Coordinator;
use crate::dashboard::{load, load_in_tx, path_key, snapshot_value};
use crate::params::Params;
use crate::response::PyJson;
use crate::{ApiResult, AppError, AppState};

pub async fn teams(State(state): State<AppState>) -> ApiResult<PyJson> {
    let mut data = load(&state, false).await?;
    let counts: Vec<(String, i64, String, bool)> = sqlx::query_as(
        "SELECT t.name, count(i.pk), coalesce(t.phone, team_demo_phone(t.name)), t.phone IS NULL
         FROM teams t LEFT JOIN incidents i
         ON i.assigned_group_name = t.name AND i.is_present AND i.normalized_state NOT IN ('Closed', 'Resolved')
         GROUP BY t.name ORDER BY t.name")
        .fetch_all(data.conn()).await?;
    let items: Vec<_> = counts
        .into_iter()
        .map(|(name, count, phone, phone_is_test)| {
            json!({"name": name, "activeCount": count, "phone": phone, "phoneIsTest": phone_is_test})
        })
        .collect();
    Ok(PyJson(json!({"items": items, "revision": data.revision})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Assignment {
    team: String,
    expected_revision: i64,
}

pub(crate) async fn assign(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    coordinator: Option<Extension<Coordinator>>,
    body: Result<Json<Assignment>, JsonRejection>,
) -> ApiResult<PyJson> {
    let Json(input) =
        body.map_err(|_| AppError::bad_request("Ожидаются team и целое expectedRevision в JSON."))?;
    let mut params = Params::new(&uri);
    let key = path_key(&mut params, &uri);
    params.finish()?;

    let mut tx = state.pool.begin().await?;
    let source = import_repo::lock_state(&mut tx).await?;
    if source.last_success_at.is_none() {
        return Err(AppError::source_not_ready(
            "Первый снимок ещё не принят, повторите запрос позже",
        ));
    }
    let revision = read_repo::revision(&mut tx).await?;
    if input.expected_revision != revision {
        return Err(AppError::conflict(
            "Данные изменились. Проверьте актуальную команду и повторите назначение.",
        )
        .with_detail("currentRevision", revision));
    }
    let row = import_repo::stored_by_sys_id(&mut tx, &key.unwrap_or_default())
        .await?
        .ok_or_else(|| AppError::not_found("Инцидент не найден").with_code("INCIDENT_NOT_FOUND"))?;
    if assignment::closed(row.normalized_state) {
        return Err(AppError::conflict(
            "Нельзя назначить завершённый или закрытый инцидент.",
        ));
    }
    let valid: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM teams WHERE name = $1)")
        .bind(&input.team)
        .fetch_one(&mut *tx)
        .await?;
    if !valid {
        return Err(AppError::bad_request("Выберите команду из справочника."));
    }
    if row.assigned_group_name.as_deref() != Some(&input.team) {
        let actor_login = coordinator.as_ref().map(|account| account.login.as_str());
        assignment::record_assignment(
            &mut tx,
            &state.settings,
            &row,
            &input.team,
            (state.clock)(),
            actor_login,
        )
        .await?;
    }
    let mut data = load_in_tx(tx, true).await?;
    let snapshot = snapshot_value(&state, &mut data, true).await?;
    data.tx.commit().await?;
    Ok(PyJson(snapshot))
}
