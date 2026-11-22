use axum::extract::{Path, State, rejection::JsonRejection};
use axum::http::StatusCode;
use axum::{Extension, Json};
use dashboard_domain::analysis::AUTO_CLOSED_GROUP;
use dashboard_domain::change_detector::DEFAULT_COORDINATOR_GROUP;
use dashboard_domain::constants::{ChangeEventType, DashboardEventReason};
use dashboard_domain::header::header_team;
use dashboard_domain::projection::change_event_view;
use dashboard_store::import_repo::{self, NewChangeEvent};
use dashboard_store::read_repo::{self, ChangeEventFilter};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::coordinator_accounts::Coordinator;
use crate::response::PyJson;
use crate::{ApiResult, AppError, AppState, dashboard, mobile};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TeamContactRequest {
    team: String,
    request_id: String,
}

async fn event_at_revision(conn: &mut PgConnection, revision: i64) -> ApiResult<Value> {
    let rows = read_repo::change_events(
        conn,
        &ChangeEventFilter {
            limit: 1,
            revision: Some(revision),
            ..Default::default()
        },
    )
    .await?;
    rows.first().map(change_event_view).ok_or_else(AppError::internal)
}

pub(crate) async fn record(
    State(state): State<AppState>,
    Path(sys_id): Path<String>,
    Extension(account): Extension<Coordinator>,
    body: Result<Json<TeamContactRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, PyJson)> {
    let Json(input) = body.map_err(mobile::json_error)?;
    let team = header_team(&input.team).ok_or_else(|| AppError::validation("Укажите команду."))?;
    if sys_id.trim().is_empty() || sys_id.chars().count() > 64 {
        return Err(AppError::validation("Укажите корректный sysId инцидента."));
    }
    let request_id = Uuid::parse_str(&input.request_id)
        .map_err(|_| AppError::validation("Поле requestId должно быть UUID."))?
        .to_string();

    let mut tx = state.pool.begin().await?;
    let source = import_repo::lock_state(&mut tx).await?;
    if source.last_success_at.is_none() {
        return Err(AppError::source_not_ready(
            "Первый снимок ещё не принят, повторите запрос позже",
        ));
    }

    // Look up replays before the live assignment, which may already have changed or disappeared.
    let previous_revision: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM dashboard_events WHERE reason = 'team_contacted'
          AND payload->'contact'->>'actorLogin' = $1 AND payload->>'sysId' = $2
          AND payload->'contact'->>'requestId' = $3",
    )
    .bind(&account.login)
    .bind(&sys_id)
    .bind(&request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(revision) = previous_revision {
        let event = event_at_revision(&mut tx, revision).await?;
        if event["contact"]["team"] != team {
            return Err(
                AppError::conflict("Этот requestId уже использован для другой команды.")
                    .with_code("TEAM_CONTACT_REQUEST_CONFLICT"),
            );
        }
        tx.commit().await?;
        return Ok((StatusCode::OK, PyJson(event)));
    }

    let incident = import_repo::stored_by_sys_id(&mut tx, &sys_id)
        .await?
        .ok_or_else(dashboard::incident_not_found)?;
    let current_team = incident
        .raw
        .get("assignedGroupName")
        .and_then(Value::as_str)
        .and_then(header_team);
    if current_team != Some(team) {
        return Err(
            AppError::conflict("Команда изменилась. Обновите карточку инцидента.")
                .with_code("TEAM_CONTACT_ASSIGNMENT_CHANGED")
                .with_detail("currentTeam", current_team),
        );
    }
    if team == state.settings.coordinator_group_name
        || matches!(team, DEFAULT_COORDINATOR_GROUP | AUTO_CLOSED_GROUP)
    {
        return Err(
            AppError::conflict("Для инцидента нужна назначенная команда решения.")
                .with_code("TEAM_CONTACT_UNAVAILABLE"),
        );
    }

    let now = (state.clock)();
    let payload = json!({
        "schemaVersion": 1,
        "sysId": incident.sys_id,
        "contact": {
            "team": team,
            "actorLogin": account.login,
            "actorDisplayName": account.display_name,
            "requestId": request_id,
        },
    });
    let revision =
        import_repo::new_dashboard_event(&mut tx, DashboardEventReason::TeamContacted, &payload, now).await?;
    // Only the journal revision changes. Keep the existing bell timing and eligibility state.
    sqlx::query("SET LOCAL underctrl.mobile_bell_writer = '1'")
        .execute(&mut *tx)
        .await?;
    let incident_revision: i32 = sqlx::query_scalar(
        "UPDATE incidents SET incident_revision = incident_revision + 1 WHERE pk = $1 RETURNING incident_revision",
    )
    .bind(incident.pk)
    .fetch_one(&mut *tx)
    .await?;
    import_repo::insert_change_event(
        &mut tx,
        &NewChangeEvent {
            revision,
            incident_pk: incident.pk,
            incident_revision: i64::from(incident_revision),
            sys_id: &incident.sys_id,
            incident_id: &incident.id,
            header: incident.raw.get("header").and_then(Value::as_str),
            types: vec![ChangeEventType::TeamContacted.as_str()],
            fields: Vec::new(),
            previous: None,
            current: None,
            at: now,
        },
    )
    .await?;
    let event = event_at_revision(&mut tx, revision).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, PyJson(event)))
}
