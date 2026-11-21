//! Объяснимый разбор инцидента по правилам веб-клиента.

use std::cmp::Reverse;
use std::collections::HashMap;

use axum::extract::{OriginalUri, State};
use chrono::{DateTime, Utc};
use dashboard_domain::constants::NormalizedState;
use dashboard_domain::projection::{IncidentRow, format_utc};
use serde_json::{Value, json};

use crate::AppState;
use crate::dashboard::{incident_not_found, load, path_key};
use crate::error::ApiResult;
use crate::params::Params;
use crate::response::PyJson;
use crate::web_queue::{sla_kind, state_of};

pub async fn get(State(state): State<AppState>, OriginalUri(uri): OriginalUri) -> ApiResult<PyJson> {
    let mut params = Params::new(&uri);
    let key = path_key(&mut params, &uri);
    params.finish()?;
    let key = key.unwrap_or_default();
    let loaded = load(&state, true).await?;
    let item = loaded
        .rows
        .iter()
        .find(|row| row.sys_id == key)
        .or_else(|| loaded.rows.iter().find(|row| row.id == key))
        .ok_or_else(incident_not_found)?;
    Ok(PyJson(analyze(
        item,
        &loaded.rows,
        loaded.revision,
        (state.clock)(),
        &state.settings.coordinator_group_name,
    )))
}

fn analyze(
    item: &IncidentRow,
    items: &[IncidentRow],
    revision: i64,
    now: DateTime<Utc>,
    coordinator_group: &str,
) -> Value {
    let state = state_of(item);
    let team = item.assigned_group_name.as_deref().unwrap_or_default();
    let mut attention = Vec::new();
    if state.is_active() && item.priority_id == 4 {
        attention.push("Наивысший приоритет: необходимо оперативное внимание.");
    }
    if state.is_attention() && team.trim() == coordinator_group {
        attention.push("Назначен координаторам: необходимо определить команду для передачи.");
    }
    match sla_kind(item, now) {
        "late" => attention.push("Срок решения истёк. Проверьте необходимость эскалации."),
        "soon" => attention.push("До дедлайна осталось менее 30 минут."),
        "restored" => attention.push("Проблема устранена, но инцидент ещё может вернуться в работу."),
        "closed" => attention.push("Вся работа с инцидентом завершена."),
        "research" => attention
            .push("Идёт исследование причин; прежний дедлайн нельзя автоматически считать SLA исследования."),
        _ => {}
    }
    if item.is_need_up == Some(true) {
        attention.push("После устранения проблемы требуется работа с последствиями.");
    }
    let candidates = candidate_teams(item, items, coordinator_group);
    let mut missing = Vec::new();
    if item.deadline.is_none() {
        missing.push("Не указан срок решения.");
    }
    if item.assigned_employee.as_deref().unwrap_or_default().is_empty() {
        missing.push("Не указан персональный исполнитель.");
    }
    if candidates.is_empty() {
        missing.push("В снимке недостаточно сопоставимых завершённых случаев для рекомендации команды.");
    }
    let summary = match (&item.short_description, &item.description) {
        (Some(short), _) if !short.is_empty() => short.clone(),
        (_, Some(description)) if !description.is_empty() => description.chars().take(420).collect(),
        _ => item.header.clone().unwrap_or_default(),
    };
    let status = match state {
        NormalizedState::Assigned => "Назначен",
        NormalizedState::WorkInProgress => "Выполняется",
        NormalizedState::Resolved => "Завершён",
        NormalizedState::Closed => "Закрыт",
        NormalizedState::Research => "Исследование",
        NormalizedState::Unknown => "Неизвестный статус",
    };
    json!({
        "sysId": item.sys_id,
        "revision": revision,
        "generatedAt": format_utc(&now),
        "provider": "rules",
        "summary": summary,
        "facts": [
            {"label": "Продукт", "value": item.product_name.as_deref().unwrap_or_default()},
            {"label": "Сервис", "value": item.service.as_deref().unwrap_or_default()},
            {"label": "Команда", "value": team},
            {"label": "Статус", "value": status}
        ],
        "attention": attention,
        "missing": missing,
        "candidates": candidates,
    })
}

fn candidate_teams(item: &IncidentRow, items: &[IncidentRow], coordinator_group: &str) -> Vec<Value> {
    let mut teams = HashMap::<&str, (usize, Vec<&str>)>::new();
    for (index, other) in items.iter().enumerate() {
        let team = other.assigned_group_name.as_deref().unwrap_or_default();
        if other.sys_id == item.sys_id
            || !matches!(
                state_of(other),
                NormalizedState::Resolved | NormalizedState::Closed
            )
            || other.completion_date.is_none()
            || team.trim().is_empty()
            || team == coordinator_group
            || team == "AutoClosedMi"
        {
            continue;
        }
        let same_service =
            item.service.as_deref().is_some_and(|s| !s.trim().is_empty()) && item.service == other.service;
        let same_scenario = item.scenario_name.as_deref().is_some_and(|s| !s.is_empty())
            && item.scenario_name == other.scenario_name
            && item.product_name == other.product_name;
        if same_service || same_scenario {
            teams
                .entry(team)
                .or_insert_with(|| (index, Vec::new()))
                .1
                .push(&other.id);
        }
    }
    let mut teams: Vec<_> = teams.into_iter().collect();
    teams.sort_unstable_by_key(|(_, (index, evidence))| (Reverse(evidence.len()), *index));
    teams
        .into_iter()
        .take(3)
        .map(|(team, (_, evidence))| json!({
            "team": team,
            "evidenceIds": evidence,
            "reason": "Команда встречается в завершённых записях того же сервиса или сценария продукта. Совпадение не подтверждает зону ответственности."
        }))
        .collect()
}
