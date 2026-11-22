use axum::{Json, extract::State};
use chrono::{DateTime, SecondsFormat, Utc};
use dashboard_domain::constants::NormalizedState;
use dashboard_domain::header::{Header, header_team};
use serde::Serialize;
use sqlx::{Row, postgres::PgRow};

use crate::{ApiResult, AppState, dashboard};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IncidentSnapshot {
    revision: i64,
    server_time: String,
    items: Vec<Incident>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Incident {
    sys_id: String,
    id: String,
    header: Option<String>,
    normalized_state: NormalizedState,
    priority_id: i16,
    assigned_group_name: Option<String>,
    submit_date: String,
    deadline: Option<String>,
    last_assignment_to_team_date_time: Option<String>,
    header_kind: Option<&'static str>,
}

fn utc_millis(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(crate) async fn list(State(state): State<AppState>) -> ApiResult<Json<IncidentSnapshot>> {
    let mut loaded = dashboard::load(&state, false).await.map_err(|mut error| {
        if error.code == "SOURCE_NOT_READY" {
            error.details.remove("source");
        }
        error
    })?;
    let now = (state.clock)();
    let items = sqlx::query(
        "SELECT sys_id, id, header, normalized_state, priority_id, raw ->> 'assignedGroupName' AS team,
                submit_date, deadline, last_assignment_to_team_at
         FROM incidents WHERE is_present ORDER BY id, sys_id",
    )
    .try_map(|row: PgRow| {
        let state: NormalizedState =
            row.try_get::<&str, _>("normalized_state")?
                .parse()
                .map_err(|e| sqlx::Error::ColumnDecode {
                    index: "normalized_state".into(),
                    source: Box::new(e),
                })?;
        let priority: i16 = row.try_get("priority_id")?;
        let team = row
            .try_get::<Option<&str>, _>("team")?
            .and_then(header_team)
            .map(str::to_owned);
        let submitted: DateTime<Utc> = row.try_get("submit_date")?;
        let deadline: Option<DateTime<Utc>> = row.try_get("deadline")?;
        let assignment: Option<DateTime<Utc>> = row.try_get("last_assignment_to_team_at")?;
        let kind = Header {
            state,
            priority,
            team: team.as_deref(),
            submitted: submitted.timestamp_millis(),
            assignment: assignment.map(|t| t.timestamp_millis()),
            deadline: deadline.map(|t| t.timestamp_millis()),
        }
        .kind_at(now.timestamp_millis());
        Ok(Incident {
            sys_id: row.try_get("sys_id")?,
            id: row.try_get("id")?,
            header: row.try_get("header")?,
            normalized_state: state,
            priority_id: priority,
            assigned_group_name: team,
            submit_date: utc_millis(submitted),
            deadline: deadline.map(utc_millis),
            last_assignment_to_team_date_time: assignment.map(utc_millis),
            header_kind: kind,
        })
    })
    .fetch_all(loaded.conn())
    .await?;
    Ok(Json(IncidentSnapshot {
        revision: loaded.revision,
        server_time: utc_millis(now),
        items,
    }))
}
