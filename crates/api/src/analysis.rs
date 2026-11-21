//! Объяснимый разбор инцидента по правилам веб-клиента и, если готова, выжимка локальной модели.

use std::collections::HashSet;

use axum::extract::{OriginalUri, State};
use chrono::{DateTime, Utc};
use dashboard_domain::analysis::{TeamCandidate, candidate_teams};
use dashboard_domain::constants::NormalizedState;
use dashboard_domain::projection::{IncidentRow, format_utc};
use dashboard_store::analysis_repo::{self, JobSpec};
use serde_json::{Value, json};

use crate::AppState;
use crate::dashboard::{incident_not_found, load, path_key};
use crate::error::ApiResult;
use crate::params::Params;
use crate::response::PyJson;
use crate::web_queue::{sla_kind, state_of};

const RULES_REASON: &str = "Команда встречается в завершённых записях того же сервиса или сценария продукта. Совпадение не подтверждает зону ответственности.";
const LLM_REASON: &str = "Модель выбрала команду среди кандидатов по завершённым записям того же сервиса или сценария продукта. Совпадение не подтверждает зону ответственности.";

pub async fn get(State(state): State<AppState>, OriginalUri(uri): OriginalUri) -> ApiResult<PyJson> {
    let mut params = Params::new(&uri);
    let key = path_key(&mut params, &uri);
    params.finish()?;
    let key = key.unwrap_or_default();
    let mut loaded = load(&state, true).await?;
    let item = loaded
        .rows
        .iter()
        .find(|row| row.sys_id == key)
        .or_else(|| loaded.rows.iter().find(|row| row.id == key))
        .ok_or_else(incident_not_found)?;
    let mut analysis = analyze(
        item,
        &loaded.rows,
        loaded.revision,
        (state.clock)(),
        &state.settings.coordinator_group_name,
    );
    if state.settings.llm_enabled {
        let spec = JobSpec::summary(&state.settings);
        if let Some(stored) = analysis_repo::current_result(&mut loaded.tx, &spec, item.pk).await? {
            let teams: HashSet<String> = analysis_repo::team_names(&mut loaded.tx).await?.into_iter().collect();
            let rules = candidate_teams(item, &loaded.rows, &state.settings.coordinator_group_name);
            apply_llm(&mut analysis, &stored.result, stored.finished_at, &rules, &teams);
        }
    }
    Ok(PyJson(analysis))
}

/// Заменяет выжимку и кандидатов результатом модели. Команда выдаётся, только если её выбрала
/// модель среди переданных ей кандидатов и она до сих пор есть среди кандидатов правил и в
/// справочнике; `evidenceIds` берутся из текущих правил. Иначе `candidates` пуст.
fn apply_llm(
    analysis: &mut Value,
    result: &Value,
    finished_at: DateTime<Utc>,
    rules: &[TeamCandidate<'_>],
    teams: &HashSet<String>,
) {
    let Some(summary) = result.get("summary").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) else {
        return;
    };
    let offered: Vec<&str> = result
        .get("candidates")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let chosen = result
        .get("team")
        .and_then(Value::as_str)
        .filter(|team| offered.contains(team) && teams.contains(*team))
        .and_then(|team| rules.iter().find(|c| c.team == team));
    let candidates: Vec<Value> = chosen
        .map(|c| json!({"team": c.team, "evidenceIds": c.evidence_ids, "reason": LLM_REASON}))
        .into_iter()
        .collect();
    let had_rules = !rules.is_empty();
    analysis["provider"] = json!("llm");
    analysis["summary"] = json!(summary);
    analysis["generatedAt"] = json!(format_utc(&finished_at));
    analysis["candidates"] = Value::Array(candidates);
    if chosen.is_none()
        && had_rules
        && let Some(missing) = analysis["missing"].as_array_mut()
    {
        missing.push(json!("Модель не выбрала команду: оснований в записи недостаточно, выберите команду вручную."));
    }
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
    let candidates: Vec<Value> = candidate_teams(item, items, coordinator_group)
        .into_iter()
        .map(|c| json!({"team": c.team, "evidenceIds": c.evidence_ids, "reason": RULES_REASON}))
        .collect();
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
