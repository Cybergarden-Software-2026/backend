//! Dashboard assignments are stored in PostgreSQL. The read-only ITSM export can confirm
//! or supersede one by changing the upstream assignment or closing/removing the incident.
use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use chrono_tz::Tz;
use dashboard_config::Settings;
use dashboard_domain::change_detector::{PlanOptions, StoredIncident, build_import_plan};
use dashboard_domain::constants::{DashboardEventReason, NormalizedState};
use dashboard_domain::normalizer::{NormalizedIncident, iso_z, normalize_record};
use dashboard_store::import_repo as repo;
use serde_json::{Value, json};
use sqlx::PgConnection;

const FIELDS: [&str; 4] = [
    "assignedGroupName",
    "assignedEmployee",
    "assignedEmployeeLogin",
    "lastAssignmentToTeamDateTime",
];

pub fn closed(state: NormalizedState) -> bool {
    matches!(state, NormalizedState::Resolved | NormalizedState::Closed)
}

fn source_assignment(incident: &NormalizedIncident) -> Value {
    Value::Array(
        FIELDS
            .iter()
            .map(|field| incident.tracked.get(*field).cloned().unwrap_or(Value::Null))
            .collect(),
    )
}

fn set_team(
    incident: &mut NormalizedIncident,
    team: &str,
    at: DateTime<Utc>,
    tz: Tz,
) -> Result<(), sqlx::Error> {
    let mut raw = std::mem::take(&mut incident.raw);
    for (field, value) in FIELDS.into_iter().zip([team, "", "", &iso_z(&at)]) {
        raw.insert(field.into(), Value::String(value.into()));
    }
    *incident = normalize_record(raw, tz).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    Ok(())
}

pub(crate) async fn reconcile_source(
    conn: &mut PgConnection,
    incidents: &mut [NormalizedIncident],
    tz: Tz,
) -> Result<(), sqlx::Error> {
    let teams: Vec<_> = incidents
        .iter()
        .filter_map(|i| i.columns.assigned_group_name.as_deref())
        .filter(|name| *name != "AutoClosedMi")
        .collect();
    sqlx::query("INSERT INTO teams (name) SELECT DISTINCT unnest($1::text[]) ON CONFLICT DO NOTHING")
        .bind(&teams)
        .execute(&mut *conn)
        .await?;

    let rows: Vec<(String, Value, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT incident_id, source_assignment, team, assigned_at FROM assignment_overrides a
         WHERE NOT EXISTS (SELECT 1 FROM form_incidents f WHERE f.incident_id = a.incident_id)",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut overrides: HashMap<_, _> = rows
        .into_iter()
        .map(|(id, source, team, at)| (id, (source, team, at)))
        .collect();
    let mut obsolete = Vec::new();
    for item in incidents {
        if let Some((baseline, team, at)) = overrides.remove(&item.id) {
            if !closed(item.state.state) && source_assignment(item) == baseline {
                set_team(item, &team, at, tz)?;
            } else {
                obsolete.push(item.id.clone());
            }
        }
    }
    obsolete.extend(overrides.into_keys());
    if !obsolete.is_empty() {
        sqlx::query("DELETE FROM assignment_overrides WHERE incident_id = ANY($1)")
            .bind(obsolete)
            .execute(conn)
            .await?;
    }
    Ok(())
}

/// Caller holds the same import_state row lock as the importer and has checked expectedRevision.
pub async fn record_assignment(
    conn: &mut PgConnection,
    settings: &Settings,
    current: &StoredIncident,
    team: &str,
    now: DateTime<Utc>,
    actor_login: Option<&str>,
) -> Result<(), sqlx::Error> {
    let tz = settings.source_naive_timezone;
    let mut next =
        normalize_record(current.raw.clone(), tz).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    // On a second local reassignment retain the original upstream values for reconciliation.
    sqlx::query("INSERT INTO assignment_overrides (incident_id, source_assignment, team, assigned_at)
        VALUES ($1, $2, $3, $4) ON CONFLICT (incident_id) DO UPDATE SET team = EXCLUDED.team, assigned_at = EXCLUDED.assigned_at")
        .bind(&current.id).bind(source_assignment(&next)).bind(team).bind(now).execute(&mut *conn).await?;
    set_team(&mut next, team, now, tz)?;
    let options = PlanOptions {
        is_initial: false,
        naive_tz: tz,
        coordinator_group: settings.coordinator_group_name.clone(),
        now: Some(now),
        resolved_window: TimeDelta::hours(settings.critical_resolved_window_hours.into()),
    };
    let plan = build_import_plan(std::slice::from_ref(current), vec![next], &options);
    let payload = json!({
        "schemaVersion": 1,
        "sysId": current.sys_id,
        "team": team,
        "actor": actor_login.unwrap_or(&settings.default_user_id),
        "actorLogin": actor_login,
    });
    let revision = repo::new_dashboard_event(conn, DashboardEventReason::Assignment, &payload, now).await?;
    crate::service::apply_plan(conn, &plan, revision, now).await?;
    if settings.llm_enabled {
        let spec = dashboard_store::analysis_repo::JobSpec::summary(settings);
        dashboard_store::analysis_repo::enqueue_incident(conn, &spec, current.pk).await?;
    }
    sqlx::query("UPDATE import_state SET last_change_at = $1, updated_at = $1 WHERE id = $2")
        .bind(now)
        .bind(repo::IMPORT_STATE_ID)
        .execute(conn)
        .await?;
    Ok(())
}
