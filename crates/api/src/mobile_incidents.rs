use axum::{Json, extract::State};
use chrono::{DateTime, SecondsFormat, Utc};
use dashboard_domain::constants::NormalizedState;
use dashboard_domain::header::{Header, header_team};
use dashboard_domain::projection::CoordinationClaim;
use dashboard_store::read_repo;
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
    team_phone: Option<String>,
    team_phone_is_test: bool,
    submit_date: String,
    deadline: Option<String>,
    last_assignment_to_team_date_time: Option<String>,
    header_kind: Option<&'static str>,
    coordination_claim: Option<CoordinationClaim>,
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
        "SELECT i.sys_id, i.id, i.header, i.normalized_state, i.priority_id,
                i.raw ->> 'assignedGroupName' AS team,
                i.submit_date, i.deadline, i.last_assignment_to_team_at,
                coalesce(t.phone, team_demo_phone(t.name)) AS team_phone,
                t.name IS NOT NULL AND t.phone IS NULL AS team_phone_is_test,
                a.login AS claim_login, a.display_name AS claim_display_name, c.claimed_at
         FROM incidents i LEFT JOIN teams t ON t.name = i.assigned_group_name
         LEFT JOIN incident_coordination_claims c ON c.incident_pk = i.pk
         LEFT JOIN coordinator_accounts a ON a.login = c.account_login AND a.disabled_at IS NULL
         WHERE i.is_present ORDER BY i.id, i.sys_id",
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
            team_phone: row.try_get("team_phone")?,
            team_phone_is_test: row.try_get("team_phone_is_test")?,
            submit_date: utc_millis(submitted),
            deadline: deadline.map(utc_millis),
            last_assignment_to_team_date_time: assignment.map(utc_millis),
            header_kind: kind,
            coordination_claim: read_repo::coordination_claim_from_row(&row)?,
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
