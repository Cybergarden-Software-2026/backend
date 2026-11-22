use axum::{Json, extract::State};
use chrono::{DateTime, SecondsFormat, Utc};
use dashboard_domain::change_detector::DEFAULT_COORDINATOR_GROUP;
use dashboard_domain::constants::{NormalizedState, PRIORITY_CRITICAL};
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

// The web header evaluates at JavaScript Date (millisecond) precision.
fn header_kind(
    state: NormalizedState,
    priority: i16,
    team: Option<&str>,
    submitted: i64,
    last_assignment: Option<i64>,
    deadline: Option<i64>,
    now: i64,
) -> Option<&'static str> {
    if !state.is_attention() {
        return None;
    }
    if i64::from(priority) == PRIORITY_CRITICAL {
        return Some("critical");
    }
    let team = team.unwrap_or_default();
    let on_coordinators = team == DEFAULT_COORDINATOR_GROUP;
    if on_coordinators && now - last_assignment.unwrap_or(submitted) > 8 * 60_000 {
        return Some("waiting");
    }
    let deadline = deadline?;
    if deadline <= submitted || now < submitted {
        return None;
    }
    let remaining = deadline - now;
    let near_deadline = remaining * 5 < deadline - submitted;
    (near_deadline && (remaining > 0 || team.is_empty() || on_coordinators)).then_some("deadline")
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
        // Match String.trim() on the effective source team, as the web header does.
        let team = row.try_get::<Option<&str>, _>("team")?.and_then(|name| {
            let name = name.trim_matches(|c: char| (c.is_whitespace() && c != '\u{0085}') || c == '\u{feff}');
            (!name.is_empty()).then(|| name.to_owned())
        });
        let submitted: DateTime<Utc> = row.try_get("submit_date")?;
        let deadline: Option<DateTime<Utc>> = row.try_get("deadline")?;
        let assignment: Option<DateTime<Utc>> = row.try_get("last_assignment_to_team_at")?;
        let kind = header_kind(
            state,
            priority,
            team.as_deref(),
            submitted.timestamp_millis(),
            assignment.map(|t| t.timestamp_millis()),
            deadline.map(|t| t.timestamp_millis()),
            now.timestamp_millis(),
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_wait_starts_at_the_last_assignment_and_requires_more_than_eight_minutes() {
        let team = Some(DEFAULT_COORDINATOR_GROUP);
        let alert = |now, assignment, priority, deadline| {
            header_kind(
                NormalizedState::Research,
                priority,
                team,
                0,
                assignment,
                deadline,
                now,
            )
        };
        assert_eq!(alert(480_000, None, 2, None), None);
        assert_eq!(alert(480_001, None, 2, None), Some("waiting"));
        assert_eq!(alert(500_000, Some(100_000), 2, None), None);
        assert_eq!(alert(600_000, Some(100_000), 2, Some(600_001)), Some("waiting"));
        assert_eq!(alert(600_000, None, 4, Some(600_001)), Some("critical"));
    }

    #[test]
    fn deadline_crosses_twenty_percent_then_drops_at_expiry_for_resolution_teams() {
        let alert = |now, team, submit, deadline| {
            header_kind(
                NormalizedState::WorkInProgress,
                1,
                team,
                submit,
                None,
                deadline,
                now,
            )
        };
        assert_eq!(alert(799, Some("Team A"), 0, Some(1000)), None);
        assert_eq!(alert(800, Some("Team A"), 0, Some(1000)), None);
        assert_eq!(alert(801, Some("Team A"), 0, Some(1000)), Some("deadline"));
        assert_eq!(alert(1000, Some("Team A"), 0, Some(1000)), None);
        assert_eq!(alert(1000, None, 0, Some(1000)), Some("deadline"));
        assert_eq!(
            alert(1001, Some(DEFAULT_COORDINATOR_GROUP), 0, Some(1000)),
            Some("deadline")
        );
        assert_eq!(alert(1000, None, 1000, Some(1000)), None);
        assert_eq!(alert(900, None, 1000, Some(1100)), None);
        assert_eq!(alert(1000, None, 0, None), None);
    }
}
