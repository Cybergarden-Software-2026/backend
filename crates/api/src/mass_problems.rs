//! Shared mass-problem curation. Automatic suggestions are derived from the current incident list;
//! only explicit group membership and exclusions are persisted.

use std::collections::{HashMap, HashSet};

use axum::extract::{Json, State, rejection::JsonRejection};
use chrono::{DateTime, Utc};
use dashboard_domain::constants::DashboardEventReason;
use dashboard_domain::projection::IncidentRow;
use dashboard_store::import_repo;
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgConnection, types::Json as DbJson};
use dashboard_domain::mass_problems::{
    Cluster, Exclusion, ManualGroup, Workspace, context_key, header, index, member,
};

use crate::dashboard::{load, load_in_tx};
use crate::response::PyJson;
use crate::{ApiResult, AppError, AppState, web_queue};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edit {
    expected_revision: i64,
    #[serde(flatten)]
    action: Action,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", rename_all_fields = "camelCase")]
enum Action {
    Add { group_id: String, sys_ids: Vec<String> },
    Exclude { group_id: String, sys_id: String },
    Restore { sys_id: String },
}

pub async fn list(State(state): State<AppState>) -> ApiResult<PyJson> {
    let mut loaded = load(&state, true).await?;
    let workspace = read_workspace(loaded.conn()).await?;
    Ok(workspace.response(loaded.revision, &loaded.rows))
}

pub async fn update(
    State(state): State<AppState>,
    body: Result<Json<Edit>, JsonRejection>,
) -> ApiResult<PyJson> {
    let Json(edit) = body.map_err(|_| AppError::validation("Некорректные параметры изменения группы."))?;
    if edit.expected_revision < 0 {
        return Err(AppError::validation(
            "expectedRevision должен быть неотрицательным.",
        ));
    }

    // Source imports and assignments take this same lock before reading or advancing the revision.
    let mut tx = state.pool.begin().await?;
    import_repo::lock_state(&mut tx).await?;
    let mut loaded = load_in_tx(tx, true).await?;
    if edit.expected_revision != loaded.revision {
        return Err(
            AppError::conflict("Данные изменились. Проверьте состав группы и повторите действие.")
                .with_detail("currentRevision", loaded.revision),
        );
    }
    let mut workspace = read_workspace(loaded.conn()).await?;
    let now = (state.clock)();
    let mut revision = loaded.revision;
    if workspace.edit(edit.action, &loaded.rows, now)? {
        sqlx::query("UPDATE mass_problem_workspace SET state = $1 WHERE id = 1")
            .bind(DbJson(&workspace))
            .execute(loaded.conn())
            .await?;
        revision = import_repo::new_dashboard_event(
            loaded.conn(),
            DashboardEventReason::MassProblems,
            &json!({"schemaVersion": 1}),
            now,
        )
        .await?;
    }
    let response = workspace.response(revision, &loaded.rows);
    loaded.tx.commit().await?;
    Ok(response)
}

async fn read_workspace(conn: &mut PgConnection) -> Result<Workspace, sqlx::Error> {
    let DbJson(workspace) = sqlx::query_scalar("SELECT state FROM mass_problem_workspace WHERE id = 1")
        .fetch_one(conn)
        .await?;
    Ok(workspace)
}

trait WorkspaceExt {
    fn response(&self, revision: i64, rows: &[IncidentRow]) -> PyJson;
    fn edit(&mut self, action: Action, rows: &[IncidentRow], now: DateTime<Utc>) -> ApiResult<bool>;
    fn add_members(&mut self, group_id: &str, sys_ids: &[String], rows: &[IncidentRow]) -> ApiResult<bool>;
    fn exclude(&mut self, group_id: &str, sys_id: &str, rows: &[IncidentRow], now: DateTime<Utc>) -> ApiResult<bool>;
}

impl WorkspaceExt for Workspace {
    fn response(&self, revision: i64, rows: &[IncidentRow]) -> PyJson {
        let current = index(rows);
        let exclusions: Vec<_> = self
            .exclusions
            .iter()
            .filter(|row| {
                current
                    .get(row.sys_id.as_str())
                    .is_some_and(|incident| context_key(incident) == row.context_key)
            })
            .collect();
        PyJson(json!({"revision": revision, "items": self.clusters(rows), "exclusions": exclusions}))
    }

    fn edit(&mut self, action: Action, rows: &[IncidentRow], now: DateTime<Utc>) -> ApiResult<bool> {
        match action {
            Action::Add { group_id, sys_ids } => self.add_members(&group_id, &sys_ids, rows),
            Action::Exclude { group_id, sys_id } => self.exclude(&group_id, &sys_id, rows, now),
            Action::Restore { sys_id } => {
                validate_sys_id(&sys_id)?;
                let previous_len = self.exclusions.len();
                self.exclusions.retain(|row| row.sys_id != sys_id);
                Ok(self.exclusions.len() != previous_len)
            }
        }
    }

    fn add_members(&mut self, group_id: &str, sys_ids: &[String], rows: &[IncidentRow]) -> ApiResult<bool> {
        if sys_ids.is_empty() {
            return Err(AppError::validation("Укажите инциденты для добавления в группу."));
        }
        let groups = self.clusters(rows);
        let group = find_group(&groups, group_id)?;
        let current = index(rows);
        let mut selected: HashSet<_> = group.sys_ids.iter().map(String::as_str).collect();
        let mut added = Vec::new();
        for sys_id in sys_ids {
            validate_sys_id(sys_id)?;
            if !selected.insert(sys_id.as_str()) {
                continue;
            }
            let incident = find_incident(&current, sys_id)?;
            if !web_queue::state_of(incident).is_attention() {
                return Err(AppError::conflict(
                    "Инцидент больше не участвует в работе координатора.",
                ));
            }
            added.push(incident);
        }
        if added.is_empty() {
            return Ok(false);
        }
        let id = if group.id.starts_with("manual:") {
            group.id.clone()
        } else {
            format!("manual:{}", group.id)
        };
        // Preserve temporarily hidden members of an existing curated group. A re-added incident
        // gets its new context, and moves out of any other curated group in the same transaction.
        let mut members: Vec<_> = if let Some(position) = self.groups.iter().position(|old| old.id == id) {
            self.groups.remove(position).members
        } else {
            group
                .sys_ids
                .iter()
                .filter_map(|sys_id| current.get(sys_id.as_str()).map(|row| member(row)))
                .collect()
        };
        let added_ids: HashSet<_> = added.iter().map(|row| row.sys_id.as_str()).collect();
        members.retain(|row| !added_ids.contains(row.sys_id.as_str()));
        members.extend(added.iter().map(|row| member(row)));
        for previous in &mut self.groups {
            previous
                .members
                .retain(|row| !selected.contains(row.sys_id.as_str()));
        }
        self.groups.retain(|row| !row.members.is_empty());
        self.groups.insert(
            0,
            ManualGroup {
                id,
                title: group.title.clone(),
                reason: group.reason.clone(),
                members,
            },
        );
        self.exclusions
            .retain(|row| !added_ids.contains(row.sys_id.as_str()));
        Ok(true)
    }

    fn exclude(
        &mut self,
        group_id: &str,
        sys_id: &str,
        rows: &[IncidentRow],
        now: DateTime<Utc>,
    ) -> ApiResult<bool> {
        validate_sys_id(sys_id)?;
        let current = index(rows);
        let incident = find_incident(&current, sys_id)?;
        let groups = self.clusters(rows);
        let group = find_group(&groups, group_id)?;
        if !group.sys_ids.iter().any(|id| id == sys_id) {
            return Err(AppError::conflict("Инцидент не входит в выбранную группу."));
        }
        self.exclusions.retain(|row| row.sys_id != sys_id);
        self.exclusions.insert(
            0,
            Exclusion {
                sys_id: sys_id.to_owned(),
                incident_id: incident.id.clone(),
                header: header(incident).to_owned(),
                group_title: group.title.clone(),
                context_key: context_key(incident),
                excluded_at: now.timestamp_millis(),
            },
        );
        Ok(true)
    }
}

fn validate_sys_id(value: &str) -> ApiResult<()> {
    if value.trim().is_empty() {
        return Err(AppError::validation("Некорректный sysId инцидента."));
    }
    Ok(())
}

fn find_incident<'a>(rows: &HashMap<&str, &'a IncidentRow>, sys_id: &str) -> ApiResult<&'a IncidentRow> {
    rows.get(sys_id)
        .copied()
        .ok_or_else(|| AppError::not_found("Инцидент не найден").with_code("INCIDENT_NOT_FOUND"))
}

fn find_group<'a>(groups: &'a [Cluster], group_id: &str) -> ApiResult<&'a Cluster> {
    let mut found = groups.iter().filter(|group| group.id == group_id);
    let group = found.next().ok_or_else(|| {
        AppError::not_found("Группа инцидентов не найдена").with_code("MASS_PROBLEM_NOT_FOUND")
    })?;
    if found.next().is_some() {
        return Err(AppError::conflict(
            "Идентификатор подходит нескольким группам инцидентов.",
        ));
    }
    Ok(group)
}
