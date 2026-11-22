//! Объяснимый разбор инцидента по правилам веб-клиента и, если готова, выжимка локальной модели.

use std::collections::HashSet;

use axum::extract::{OriginalUri, State};
use chrono::{DateTime, Utc};
use dashboard_domain::analysis::{
    CandidateEvidence, TeamCandidate, candidate_teams, needs_coordinator_routing,
};
use dashboard_domain::constants::NormalizedState;
use dashboard_domain::llm::SummaryEvent;
use dashboard_domain::projection::{IncidentRow, format_utc};
use dashboard_store::analysis_repo::{self, JobSpec, SummaryStatus};
use serde_json::{Value, json};

use crate::AppState;
use crate::dashboard::{incident_not_found, load, path_key};
use crate::error::ApiResult;
use crate::params::Params;
use crate::response::PyJson;
use crate::web_queue::{sla_kind, state_of};

const RULES_REASON: &str = "Команда встречается в завершённых записях того же сервиса или сценария продукта. Совпадение не подтверждает зону ответственности.";
const LLM_REASON: &str = "Модель выбрала команду среди кандидатов по завершённым записям того же сервиса или сценария продукта. Совпадение не подтверждает зону ответственности.";
const ACTIVE_RULES_REASON: &str = "Команда сейчас работает с похожими инцидентами того же сервиса или сценария продукта. Проверьте её зону ответственности перед назначением.";
const ACTIVE_LLM_REASON: &str = "Модель предложила команду, которая сейчас работает с похожими инцидентами. Проверьте её зону ответственности перед назначением.";

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
    let coordinator_group = &state.settings.coordinator_group_name;
    let coordinator_routing = needs_coordinator_routing(item, coordinator_group);
    let mut candidates = candidate_teams(item, &loaded.rows, coordinator_group);
    if !candidates.is_empty() {
        let teams: HashSet<_> = analysis_repo::team_names(&mut loaded.tx)
            .await?
            .into_iter()
            .collect();
        candidates.retain(|candidate| teams.contains(candidate.team));
    }
    let mut analysis = analyze(
        item,
        loaded.revision,
        (state.clock)(),
        coordinator_routing,
        &candidates,
    );
    if state.settings.llm_enabled {
        let spec = JobSpec::summary(&state.settings);
        match analysis_repo::summary_status(&mut loaded.tx, &spec, item.pk).await? {
            SummaryStatus::Ready(stored) => apply_llm(
                &mut analysis,
                &stored.result,
                stored.finished_at,
                &candidates,
                coordinator_routing,
            ),
            SummaryStatus::Pending => analysis["summaryPending"] = json!(true),
            SummaryStatus::Unavailable => {}
        }
    }
    if analysis.get("summary").is_none() {
        let history = analysis_repo::summary_history(&mut loaded.tx, item.pk).await?;
        analysis["summary"] = json!(incident_brief(item, &history));
    }
    Ok(PyJson(analysis))
}

/// Короткая выдержка из одного поля. Многоточие отличает обрезанный текст от полной записи.
fn excerpt(value: &str) -> String {
    const LIMIT: usize = 150;
    let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= LIMIT {
        return text;
    }
    let prefix: String = text.chars().take(LIMIT - 1).collect();
    let clipped = prefix
        .rsplit_once(' ')
        .map(|(whole, _)| whole)
        .filter(|whole| whole.chars().count() > 100)
        .unwrap_or(&prefix);
    format!("{}…", clipped.trim_end())
}

fn incident_brief(item: &IncidentRow, history: &[SummaryEvent]) -> String {
    let mut problem = [&item.short_description, &item.description, &item.header]
        .into_iter()
        .filter_map(|field| field.as_deref())
        .find(|text| !text.trim().is_empty())
        .map_or_else(|| "Описание проблемы пока отсутствует.".to_owned(), excerpt);
    if !problem.ends_with(['.', '!', '?', '…']) {
        problem.push('.');
    }
    let state = state_of(item);
    let solution = item
        .solution
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .map(|text| {
            format!(
                "В поле решения указано «{}».",
                excerpt(text).trim_end_matches('.')
            )
        })
        .or_else(|| {
            let previous = history
                .iter()
                .rev()
                .flat_map(|event| [&event.solution, &event.previous_solution])
                .filter_map(|field| field.as_deref())
                .find(|text| !text.trim().is_empty())?;
            Some(format!(
                "Ранее в карточке указали «{}».",
                excerpt(previous).trim_end_matches('.')
            ))
        });
    let reopened = solution.is_some() && history.iter().any(|event| event.reopened);
    let action = solution.or_else(|| {
        history.iter().rev().find_map(|event| {
            if event.reopened {
                return Some("Инцидент вернули в работу.".to_owned());
            }
            event
                .team
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map(|team| format!("Инцидент передали команде «{}».", excerpt(team)))
        })
    });
    let action = action.unwrap_or_else(|| {
        item.assigned_group_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .map_or_else(
                || "Выполненные действия пока не описаны.".to_owned(),
                |team| format!("Карточка назначена группе «{}».", excerpt(team)),
            )
    });
    let outcome = match state {
        NormalizedState::Assigned | NormalizedState::WorkInProgress if reopened => {
            "Инцидент снова в работе, результат пока не подтверждён."
        }
        NormalizedState::Assigned => "Инцидент назначен, результат пока не подтверждён.",
        NormalizedState::WorkInProgress => "Работа продолжается, результат пока не подтверждён.",
        NormalizedState::Research => "Идёт исследование причин.",
        NormalizedState::Resolved => "В карточке инцидент отмечен как решённый.",
        NormalizedState::Closed => "Инцидент закрыт.",
        NormalizedState::Unknown => "Текущий статус не определён.",
    };
    format!("{problem} {action} {outcome}")
}

fn candidate_json(candidate: &TeamCandidate<'_>, llm: bool) -> Value {
    let reason = match (candidate.evidence, llm) {
        (CandidateEvidence::Completed, false) => RULES_REASON,
        (CandidateEvidence::Completed, true) => LLM_REASON,
        (CandidateEvidence::InProgress, false) => ACTIVE_RULES_REASON,
        (CandidateEvidence::InProgress, true) => ACTIVE_LLM_REASON,
    };
    json!({"team": candidate.team, "evidenceIds": candidate.evidence_ids, "reason": reason})
}

/// Рекомендацию модели сверяем со списком, который она получила, и актуальными кандидатами.
/// Для очереди координаторов при отсутствии выбора сохраняем кандидатов правил и выжимку модели.
fn apply_llm(
    analysis: &mut Value,
    result: &Value,
    finished_at: DateTime<Utc>,
    rules: &[TeamCandidate<'_>],
    coordinator_routing: bool,
) {
    let Some(summary) = result
        .get("summary")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    else {
        return;
    };
    let offered = result.get("candidates").and_then(Value::as_array);
    let chosen = result
        .get("team")
        .and_then(Value::as_str)
        .filter(|team| offered.is_some_and(|list| list.iter().any(|name| name.as_str() == Some(team))))
        .and_then(|team| rules.iter().find(|candidate| candidate.team == team));
    analysis["generatedAt"] = json!(format_utc(&finished_at));
    if chosen.is_none() && coordinator_routing && !rules.is_empty() {
        analysis["summary"] = json!(format!("Разбор ИИ. {summary}"));
        return;
    }
    analysis["provider"] = json!("llm");
    analysis["summary"] = json!(summary);
    analysis["candidates"] = Value::Array(chosen.map(|c| candidate_json(c, true)).into_iter().collect());
    if chosen.is_none()
        && !rules.is_empty()
        && let Some(missing) = analysis["missing"].as_array_mut()
    {
        missing.push(json!(
            "Модель не выбрала команду: оснований в записи недостаточно, выберите команду вручную."
        ));
    }
}

fn analyze(
    item: &IncidentRow,
    revision: i64,
    now: DateTime<Utc>,
    coordinator_routing: bool,
    candidates: &[TeamCandidate<'_>],
) -> Value {
    let state = state_of(item);
    let team = item.assigned_group_name.as_deref().unwrap_or_default();
    let mut attention = Vec::new();
    if state.is_active() && item.priority_id == 4 {
        attention.push("Наивысший приоритет: необходимо оперативное внимание.");
    }
    if coordinator_routing {
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
    let mut missing = Vec::new();
    if item.deadline.is_none() {
        missing.push("Не указан срок решения.");
    }
    if item.assigned_employee.as_deref().unwrap_or_default().is_empty() {
        missing.push("Не указан персональный исполнитель.");
    }
    if candidates.is_empty() {
        missing.push(if coordinator_routing {
            "В снимке недостаточно похожих инцидентов для рекомендации команды."
        } else {
            "В снимке недостаточно сопоставимых завершённых случаев для рекомендации команды."
        });
    }
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
        "facts": [
            {"label": "Продукт", "value": item.product_name.as_deref().unwrap_or_default()},
            {"label": "Сервис", "value": item.service.as_deref().unwrap_or_default()},
            {"label": "Команда", "value": team},
            {"label": "Статус", "value": status}
        ],
        "attention": attention,
        "missing": missing,
        "candidates": candidates.iter().map(|candidate| candidate_json(candidate, false)).collect::<Vec<_>>(),
    })
}
