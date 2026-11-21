use axum::extract::{OriginalUri, State};
use chrono::{DateTime, Utc};
use dashboard_config::Settings;
use dashboard_domain::constants::PRIORITY_CRITICAL;
use dashboard_domain::normalizer::{normalize_int, normalize_text, parse_source_datetime};
use serde::Serialize;
use serde_json::{Value, json};

use crate::params::Params;
use crate::response::PyJson;
use crate::{ApiResult, AppError, AppState};

const SHIFT_ID: &str = "shift";
const KPI_MS: i64 = 10 * 60_000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Transfer {
    id: String,
    coordinator_id: &'static str,
    incident_id: String,
    title: String,
    team: String,
    priority_id: i64,
    waiting_started_at: Option<i64>,
    assigned_at: i64,
    duration_ms: Option<i64>,
    kpi_met: Option<bool>,
}

fn time_bound(params: &Params, key: &str) -> ApiResult<DateTime<Utc>> {
    params
        .query_raw(key)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|parsed| parsed.with_timezone(&Utc))
        .ok_or_else(|| {
            AppError::validation(format!(
                "Параметр {key} должен быть датой RFC3339 с часовым поясом."
            ))
        })
}

fn waiting_started_at(previous: Option<&Value>, settings: &Settings, assigned_at: i64) -> Option<i64> {
    let previous = previous?;
    let team = normalize_text(&previous["assignedGroupName"]).ok()?;
    let date = match team.as_deref() {
        None => &previous["submitDate"],
        Some(team) if team == settings.coordinator_group_name => &previous["lastAssignmentToTeamDateTime"],
        _ => return None,
    };
    let start = parse_source_datetime(date, settings.source_naive_timezone)
        .ok()??
        .timestamp_millis();
    (start <= assigned_at).then_some(start)
}

pub async fn shift(State(state): State<AppState>, OriginalUri(uri): OriginalUri) -> ApiResult<PyJson> {
    let params = Params::new(&uri);
    let from = time_bound(&params, "from")?;
    let to = time_bound(&params, "to")?;
    if from >= to {
        return Err(AppError::validation("Начало периода должно быть раньше конца."));
    }

    // Read the immutable records from the assignment revision. Later imports can
    // change the team, priority or title of the current incident.
    type AssignmentRow = (
        i64,
        String,
        Option<String>,
        String,
        Option<Value>,
        Value,
        DateTime<Utc>,
    );
    let rows: Vec<AssignmentRow> = sqlx::query_as(
        r#"SELECT d.id, c.incident_id, c.header, d.payload->>'team', c.previous, c."current", d.created_at
           FROM dashboard_events d
           JOIN change_events c ON c.revision = d.id AND c.sys_id = d.payload->>'sysId'
           WHERE d.reason = 'assignment' AND d.created_at >= $1 AND d.created_at < $2
           ORDER BY d.created_at DESC, d.id DESC"#,
    )
    .bind(from)
    .bind(to)
    .fetch_all(&state.pool)
    .await?;

    let mut transfers = Vec::with_capacity(rows.len());
    let mut critical = 0;
    let mut measured = 0;
    let mut kpi_met = 0;
    let mut total_duration = 0.0;
    for (id, incident_id, title, team, previous, current, assigned_at) in rows {
        let assigned_at = assigned_at.timestamp_millis();
        let waiting_started_at = waiting_started_at(previous.as_ref(), &state.settings, assigned_at);
        let duration_ms = waiting_started_at.map(|start| assigned_at - start);
        let priority_id = normalize_int(&current["priorityId"])
            .ok()
            .flatten()
            .ok_or_else(AppError::internal)?;
        critical += usize::from(priority_id == PRIORITY_CRITICAL);
        if let Some(duration) = duration_ms {
            measured += 1;
            kpi_met += usize::from(duration < KPI_MS);
            total_duration += duration as f64;
        }
        transfers.push(Transfer {
            id: id.to_string(),
            coordinator_id: SHIFT_ID,
            title: title.unwrap_or_else(|| incident_id.clone()),
            incident_id,
            team,
            priority_id,
            waiting_started_at,
            assigned_at,
            duration_ms,
            kpi_met: duration_ms.map(|duration| duration < KPI_MS),
        });
    }

    Ok(PyJson(json!({
        "source": "api",
        "coordinator": {"id": SHIFT_ID, "name": "Смена координаторов"},
        "start": from.timestamp_millis(),
        "end": to.timestamp_millis(),
        "transferred": transfers.len(),
        "critical": critical,
        "averageMs": (measured > 0).then(|| total_duration / measured as f64),
        "measured": measured,
        "kpiMet": kpi_met,
        "kpiPercent": (measured > 0).then(|| kpi_met as f64 / measured as f64 * 100.0),
        "transfers": transfers,
    })))
}
