use axum::extract::{Json, OriginalUri, State, rejection::JsonRejection};
use axum::http::{HeaderMap, StatusCode, header};
use dashboard_importer::assignment;
use dashboard_store::{import_repo, read_repo};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::dashboard::{load, load_in_tx, path_key, snapshot_value};
use crate::params::Params;
use crate::response::PyJson;
use crate::{ApiResult, AppError, AppState};

pub async fn teams(State(state): State<AppState>) -> ApiResult<PyJson> {
    let mut data = load(&state, false).await?;
    let counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT t.name, count(i.pk) FROM teams t LEFT JOIN incidents i
         ON i.assigned_group_name = t.name AND i.is_present AND i.normalized_state NOT IN ('Closed', 'Resolved')
         GROUP BY t.name ORDER BY t.name")
        .fetch_all(data.conn()).await?;
    let items: Vec<_> = counts
        .into_iter()
        .map(|(name, count)| json!({"name":name,"activeCount":count}))
        .collect();
    Ok(PyJson(json!({"items": items, "revision": data.revision})))
}

/// `Authorization: Bearer <token>` совпадает с ключом координатора (сравнение за постоянное время).
pub(crate) fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, v)| v)
        .unwrap_or("");
    bool::from(Sha256::digest(expected.as_bytes()).ct_eq(&Sha256::digest(token.as_bytes())))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Assignment {
    team: String,
    expected_revision: i64,
}

pub async fn assign(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Json<Assignment>, JsonRejection>,
) -> ApiResult<PyJson> {
    let expected = state.settings.assignment_token.as_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::FORBIDDEN,
            "ASSIGNMENT_DISABLED",
            "Для назначения команд настройте ASSIGNMENT_TOKEN на сервере.",
        )
    })?;
    if !bearer_matches(&headers, expected) {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "ASSIGNMENT_UNAUTHORIZED",
            "Введите ключ координатора для назначения команды.",
        ));
    }
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
        assignment::record_assignment(
            &mut tx,
            &state.settings,
            &row,
            &input.team,
            (state.clock)(),
            &state.settings.default_user_id,
        )
        .await?;
    }
    let mut data = load_in_tx(tx, true).await?;
    let snapshot = snapshot_value(&state, &mut data, true).await?;
    data.tx.commit().await?;
    Ok(PyJson(snapshot))
}
