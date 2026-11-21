//! Shared mass-problem curation. Automatic suggestions are derived from the current incident list;
//! only explicit group membership and exclusions are persisted.

use std::collections::{HashMap, HashSet};

use axum::extract::{Json, State, rejection::JsonRejection};
use chrono::{DateTime, Utc};
use dashboard_domain::constants::DashboardEventReason;
use dashboard_domain::projection::IncidentRow;
use dashboard_store::import_repo;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgConnection, types::Json as DbJson};
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};

use crate::dashboard::{load, load_in_tx};
use crate::response::PyJson;
use crate::{ApiResult, AppError, AppState, web_queue};

const SAME_SERVICE: &str = "Один затронутый сервис; регистрации в пределах 24 часов";
const SAME_SYMPTOMS: &str = "Похожие симптомы в заголовках; регистрации в пределах 24 часов";
const MAX_GAP_MS: u64 = 24 * 60 * 60 * 1000;
const STOP_WORDS: &[&str] = &[
    "ошибка",
    "ошибки",
    "проблема",
    "проблемы",
    "неисправность",
    "отказ",
    "сбой",
    "аномалия",
    "на",
    "с",
    "в",
    "и",
    "при",
    "для",
    "не",
    "ситуация",
];

#[derive(Deserialize, Serialize)]
struct Workspace {
    groups: Vec<ManualGroup>,
    exclusions: Vec<Exclusion>,
}

#[derive(Deserialize, Serialize)]
struct ManualGroup {
    id: String,
    title: String,
    reason: String,
    members: Vec<Member>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Member {
    sys_id: String,
    context_key: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Exclusion {
    sys_id: String,
    incident_id: String,
    header: String,
    group_title: String,
    context_key: String,
    excluded_at: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Cluster {
    id: String,
    title: String,
    reason: String,
    sys_ids: Vec<String>,
    incident_ids: Vec<String>,
}

impl Cluster {
    fn from_rows(id: String, title: String, reason: String, rows: &[&IncidentRow]) -> Self {
        Self {
            id,
            title,
            reason,
            sys_ids: rows.iter().map(|row| row.sys_id.clone()).collect(),
            incident_ids: rows.iter().map(|row| row.id.clone()).collect(),
        }
    }
}

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

impl Workspace {
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

    fn clusters(&self, rows: &[IncidentRow]) -> Vec<Cluster> {
        let excluded: HashMap<_, _> = self
            .exclusions
            .iter()
            .map(|row| (row.sys_id.as_str(), row.context_key.as_str()))
            .collect();
        let available: HashMap<_, _> = rows
            .iter()
            .filter(|row| {
                excluded
                    .get(row.sys_id.as_str())
                    .is_none_or(|key| *key != context_key(row))
            })
            .map(|row| (row.sys_id.as_str(), row))
            .collect();
        let mut claimed = HashSet::new();
        let mut groups: Vec<_> = self
            .groups
            .iter()
            .filter_map(|group| {
                let members: Vec<_> = group
                    .members
                    .iter()
                    .filter_map(|member| {
                        available.get(member.sys_id.as_str()).copied().filter(|row| {
                            web_queue::state_of(row).is_attention()
                                && context_key(row) == member.context_key
                                && claimed.insert(row.sys_id.as_str())
                        })
                    })
                    .collect();
                (!members.is_empty()).then(|| {
                    Cluster::from_rows(
                        group.id.clone(),
                        group.title.clone(),
                        group.reason.clone(),
                        &members,
                    )
                })
            })
            .collect();
        groups.extend(automatic_clusters(
            available
                .into_values()
                .filter(|row| !claimed.contains(row.sys_id.as_str())),
        ));
        groups
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

fn index(rows: &[IncidentRow]) -> HashMap<&str, &IncidentRow> {
    rows.iter().map(|row| (row.sys_id.as_str(), row)).collect()
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

fn service(row: &IncidentRow) -> &str {
    row.service.as_deref().unwrap_or_default().trim()
}

fn header(row: &IncidentRow) -> &str {
    row.header.as_deref().unwrap_or_default().trim()
}

fn context_key(row: &IncidentRow) -> String {
    json!([service(row), header(row), row.submit_date]).to_string()
}

fn member(row: &IncidentRow) -> Member {
    Member {
        sys_id: row.sys_id.clone(),
        context_key: context_key(row),
    }
}

struct Candidate<'a> {
    incident: &'a IncidentRow,
    terms: HashSet<String>,
}

impl<'a> Candidate<'a> {
    fn new(incident: &'a IncidentRow) -> Self {
        let terms = header(incident)
            .to_lowercase()
            .split(|ch: char| {
                !matches!(
                    ch.general_category_group(),
                    GeneralCategoryGroup::Letter | GeneralCategoryGroup::Number
                )
            })
            .filter(|word| word.encode_utf16().count() > 2 && !STOP_WORDS.contains(word))
            .map(str::to_owned)
            .collect();
        Self { incident, terms }
    }

    fn related(&self, other: &Self) -> Option<&'static str> {
        let a = self.incident;
        let b = other.incident;
        if a.submit_date
            .timestamp_millis()
            .abs_diff(b.submit_date.timestamp_millis())
            > MAX_GAP_MS
        {
            return None;
        }
        if !service(a).is_empty() && service(a) == service(b) {
            return Some(SAME_SERVICE);
        }
        let common = self.terms.intersection(&other.terms).count();
        let union = self.terms.len() + other.terms.len() - common;
        (common >= 2 && 5 * common >= 3 * union).then_some(SAME_SYMPTOMS)
    }
}

fn automatic_clusters<'a>(rows: impl Iterator<Item = &'a IncidentRow>) -> Vec<Cluster> {
    let mut rows: Vec<_> = rows.filter(|row| web_queue::state_of(row).is_active()).collect();
    rows.sort_by(|a, b| a.sys_id.cmp(&b.sys_id));
    let mut buckets: Vec<Vec<Candidate<'a>>> = Vec::new();
    for incident in rows {
        let candidate = Candidate::new(incident);
        // Complete-link matching avoids grouping dissimilar endpoints of a chain.
        if let Some(bucket) = buckets
            .iter_mut()
            .find(|bucket| bucket.iter().all(|other| candidate.related(other).is_some()))
        {
            bucket.push(candidate);
        } else {
            buckets.push(vec![candidate]);
        }
    }
    let mut groups: Vec<_> = buckets
        .into_iter()
        .filter_map(|bucket| {
            let [first, second, ..] = bucket.as_slice() else {
                return None;
            };
            let rows: Vec<_> = bucket.iter().map(|candidate| candidate.incident).collect();
            let title = if rows.iter().all(|row| service(row) == service(first.incident)) {
                service(first.incident)
            } else {
                "Похожие симптомы"
            };
            let mut ids: Vec<_> = rows.iter().map(|row| row.sys_id.as_str()).collect();
            ids.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            Some(Cluster::from_rows(
                ids.join("-"),
                title.to_owned(),
                first.related(second)?.to_owned(),
                &rows,
            ))
        })
        .collect();
    groups.sort_by(|a, b| {
        b.sys_ids
            .len()
            .cmp(&a.sys_ids.len())
            .then_with(|| a.id.cmp(&b.id))
    });
    groups
}
