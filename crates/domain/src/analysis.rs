//! Правила разбора инцидента, общие для API (`/analysis`) и `ai-worker`: состояние записи и
//! кандидаты на команду по сопоставимым случаям.

use std::cmp::Reverse;
use std::collections::HashMap;

use crate::constants::NormalizedState;
use crate::projection::IncidentRow;

/// Группа, которая никогда не предлагается как команда решения.
pub const AUTO_CLOSED_GROUP: &str = "AutoClosedMi";

/// Состояние по `stateEnum`, при его отсутствии — по коду `stateId` (как в веб-очереди).
pub fn state_of(row: &IncidentRow) -> NormalizedState {
    match row.state_enum.as_deref() {
        Some(name) if !name.is_empty() => name.parse().unwrap_or(NormalizedState::Unknown),
        _ => row
            .state_id
            .and_then(|id| NormalizedState::from_state_id(i64::from(id)))
            .unwrap_or(NormalizedState::Unknown),
    }
}

/// Нужна передача инцидента от координаторов в команду решения.
pub fn needs_coordinator_routing(item: &IncidentRow, coordinator_group: &str) -> bool {
    state_of(item).is_attention()
        && item
            .assigned_group_name
            .as_deref()
            .map(str::trim)
            .is_some_and(|team| !team.is_empty() && team == coordinator_group.trim())
}

/// Состояние случаев, на которых основано предложение команды.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateEvidence {
    Completed,
    InProgress,
}

/// Команда-кандидат с номерами похожих инцидентов, на которых основан выбор.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamCandidate<'a> {
    pub team: &'a str,
    pub evidence_ids: Vec<&'a str>,
    pub evidence: CandidateEvidence,
}

/// До трёх команд из завершённых записей с датой завершения того же сервиса или того же
/// сочетания сценария и продукта. Если таких записей нет, для инцидента у координаторов
/// используются сопоставимые случаи в работе или на исследовании. Порядок: больше подтверждений,
/// затем первая встреча в `items`.
pub fn candidate_teams<'a>(
    item: &IncidentRow,
    items: &'a [IncidentRow],
    coordinator_group: &str,
) -> Vec<TeamCandidate<'a>> {
    let coordinator_group = coordinator_group.trim();
    let allow_in_progress = needs_coordinator_routing(item, coordinator_group);
    let mut completed = HashMap::<&str, (usize, Vec<&str>)>::new();
    let mut in_progress = HashMap::new();
    let same_nonblank = |a: Option<&str>, b: Option<&str>| a == b && a.is_some_and(|s| !s.trim().is_empty());
    for (index, other) in items.iter().enumerate() {
        let team = other.assigned_group_name.as_deref().unwrap_or_default();
        if other.sys_id == item.sys_id
            || team.trim().is_empty()
            || team.trim() == coordinator_group
            || team.trim() == AUTO_CLOSED_GROUP
        {
            continue;
        }
        let teams = match state_of(other) {
            NormalizedState::Resolved | NormalizedState::Closed if other.completion_date.is_some() => {
                &mut completed
            }
            NormalizedState::WorkInProgress | NormalizedState::Research if allow_in_progress => {
                &mut in_progress
            }
            _ => continue,
        };
        let same_service = same_nonblank(item.service.as_deref(), other.service.as_deref());
        let same_product_scenario =
            same_nonblank(item.scenario_name.as_deref(), other.scenario_name.as_deref())
                && same_nonblank(item.product_name.as_deref(), other.product_name.as_deref());
        if same_service || same_product_scenario {
            teams
                .entry(team)
                .or_insert_with(|| (index, Vec::new()))
                .1
                .push(&other.id);
        }
    }
    let (teams, evidence) = if completed.is_empty() {
        (in_progress, CandidateEvidence::InProgress)
    } else {
        (completed, CandidateEvidence::Completed)
    };
    let mut teams: Vec<_> = teams.into_iter().collect();
    teams.sort_unstable_by_key(|(_, (index, evidence))| (Reverse(evidence.len()), *index));
    teams
        .into_iter()
        .take(3)
        .map(|(team, (_, evidence_ids))| TeamCandidate {
            team,
            evidence_ids,
            evidence,
        })
        .collect()
}
