//! Правила разбора инцидента, общие для API (`/analysis`) и `ai-worker`: состояние записи и
//! кандидаты на команду по завершённым случаям.

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

/// Команда-кандидат с номерами завершённых инцидентов, на которых основан выбор.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamCandidate<'a> {
    pub team: &'a str,
    pub evidence_ids: Vec<&'a str>,
}

/// До трёх команд из завершённых записей (с датой завершения) того же сервиса или того же
/// сочетания сценария и продукта. Порядок: больше подтверждений, затем первая встреча в `items`.
pub fn candidate_teams<'a>(
    item: &IncidentRow,
    items: &'a [IncidentRow],
    coordinator_group: &str,
) -> Vec<TeamCandidate<'a>> {
    let mut teams = HashMap::<&str, (usize, Vec<&str>)>::new();
    for (index, other) in items.iter().enumerate() {
        let team = other.assigned_group_name.as_deref().unwrap_or_default();
        if other.sys_id == item.sys_id
            || !matches!(state_of(other), NormalizedState::Resolved | NormalizedState::Closed)
            || other.completion_date.is_none()
            || team.trim().is_empty()
            || team == coordinator_group
            || team == AUTO_CLOSED_GROUP
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
        .map(|(team, (_, evidence_ids))| TeamCandidate { team, evidence_ids })
        .collect()
}
