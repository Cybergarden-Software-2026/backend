//! Четыре группы дашборда, отметка «Решён» у критических и сортировка — порт
//! `app/services/grouping.py` (чистая логика).
//!
//! Правила (API.md §3, принятое решение):
//! * «Требует внимания» = Assigned, WorkInProgress, Research.
//! * `critical`     — priorityId = 4 и требует внимания; плюс критический, ушедший в решённое
//!   состояние: остаётся с отметкой «Решён» (в Research — пока идёт исследование,
//!   в Resolved/Closed — 24 ч от обнаружения).
//! * `coordinators` — группа «КЦ ИТ Координаторы Массовых Инцидентов» и требует внимания;
//! * `high`         — priorityId = 3 и требует внимания;
//! * `research`     — stateEnum = Research.
//!
//! Условия независимы: один инцидент может быть в нескольких группах. Неизвестный статус — ни в
//! одной. Вместо duck typing Python функции принимают любой тип с трейтом [`Groupable`]
//! (строка `incidents` из `store`, тестовая структура).

use std::cmp::Ordering;
use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};

use crate::constants::{GroupKey, NormalizedState, PRIORITY_CRITICAL, PRIORITY_HIGH, ResolutionPhase};
use crate::pycompat;

/// Поля ORM-модели `Incident`, нужные группам и отметке «Решён» (`GroupableIncident`).
pub trait Groupable {
    fn id(&self) -> &str;
    fn sys_id(&self) -> &str;
    fn normalized_state(&self) -> NormalizedState;
    fn priority_id(&self) -> i64;
    fn assigned_group_name(&self) -> Option<&str>;
    /// Для [`assignee_of`]; в Python — `getattr(incident, "assigned_employee", None)`.
    fn assigned_employee(&self) -> Option<&str> {
        None
    }
    fn deadline(&self) -> Option<DateTime<Utc>>;
    fn submit_date(&self) -> DateTime<Utc>;
    fn completion_date(&self) -> Option<DateTime<Utc>>;
    fn resolution_phase(&self) -> Option<ResolutionPhase>;
    fn resolved_detected_at(&self) -> Option<DateTime<Utc>>;
    fn recently_resolved_until(&self) -> Option<DateTime<Utc>>;
}

/// `" ".join(name.split()).casefold()`.
pub fn normalize_group_name(name: Option<&str>) -> String {
    let joined: Vec<&str> = pycompat::split_whitespace(name.unwrap_or("")).collect();
    pycompat::casefold(&joined.join(" "))
}

pub fn is_attention(state: NormalizedState) -> bool {
    state.is_attention()
}

/// Активный критический: идёт работа (Assigned/WorkInProgress) и priorityId = 4.
pub fn is_active_critical(state: NormalizedState, priority_id: i64) -> bool {
    state.is_active() && priority_id == PRIORITY_CRITICAL
}

pub fn has_resolution_mark<I: Groupable + ?Sized>(incident: &I, now: DateTime<Utc>) -> bool {
    match incident.resolution_phase() {
        None => false,
        Some(ResolutionPhase::Research) => true,
        Some(_) => incident
            .recently_resolved_until()
            .is_some_and(|until| until > now),
    }
}

/// Группы инцидента в постоянном порядке `critical, coordinators, high, research`.
pub fn compute_groups<I: Groupable + ?Sized>(
    incident: &I,
    coordinator_group: &str,
    now: DateTime<Utc>,
) -> Vec<GroupKey> {
    let state = incident.normalized_state();
    let attention = state.is_attention();
    let mut groups = Vec::new();
    if (attention && incident.priority_id() == PRIORITY_CRITICAL) || has_resolution_mark(incident, now) {
        groups.push(GroupKey::Critical);
    }
    if attention
        && normalize_group_name(incident.assigned_group_name())
            == normalize_group_name(Some(coordinator_group))
    {
        groups.push(GroupKey::Coordinators);
    }
    if attention && incident.priority_id() == PRIORITY_HIGH {
        groups.push(GroupKey::High);
    }
    if state == NormalizedState::Research {
        groups.push(GroupKey::Research);
    }
    groups
}

// ------------------------------------------------------------------ сортировка

/// Порядок внутри группы (`_sort_key`): в «Критических» активные (без отметки «Решён») — в
/// начале; затем ближайший дедлайн (без срока — в конце), `submitDate`, `id`.
fn compare_in_group<I: Groupable + ?Sized>(a: &I, b: &I, group: GroupKey, now: DateTime<Utc>) -> Ordering {
    let resolved_rank = |i: &I| group == GroupKey::Critical && has_resolution_mark(i, now);
    resolved_rank(a)
        .cmp(&resolved_rank(b))
        // `(deadline is None, deadline)`: у `Option` None меньше Some — переворачиваем вручную.
        .then_with(|| match (a.deadline(), b.deadline()) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
        .then_with(|| a.submit_date().cmp(&b.submit_date()))
        // Строки Python сравниваются по кодовым точкам — как байты UTF-8 у `str::cmp`.
        .then_with(|| a.id().cmp(b.id()))
}

#[derive(Debug)]
pub struct GroupResult<'a, I: ?Sized> {
    pub key: GroupKey,
    pub title: &'static str,
    pub incidents: Vec<&'a I>,
}

// Ручная реализация: derive потребовал бы `I: Clone`.
impl<I: ?Sized> Clone for GroupResult<'_, I> {
    fn clone(&self) -> Self {
        Self {
            key: self.key,
            title: self.title,
            incidents: self.incidents.clone(),
        }
    }
}

impl<I: Groupable + ?Sized> GroupResult<'_, I> {
    pub fn incident_ids(&self) -> Vec<&str> {
        self.incidents.iter().map(|i| i.id()).collect()
    }

    pub fn sys_ids(&self) -> Vec<&str> {
        self.incidents.iter().map(|i| i.sys_id()).collect()
    }
}

/// Членство инцидентов в группах по `id` (при повторе `id` побеждает последний, как в `dict`).
pub type Membership = HashMap<String, Vec<GroupKey>>;

/// Возвращает группы в постоянном порядке и членство каждого инцидента по `id`.
pub fn build_groups<'a, I, It>(
    incidents: It,
    coordinator_group: &str,
    now: DateTime<Utc>,
) -> (Vec<GroupResult<'a, I>>, Membership)
where
    I: Groupable + ?Sized + 'a,
    It: IntoIterator<Item = &'a I>,
{
    let mut groups: Vec<GroupResult<'a, I>> = GroupKey::ALL
        .iter()
        .map(|&key| GroupResult {
            key,
            title: key.title(),
            incidents: Vec::new(),
        })
        .collect();
    let mut membership = Membership::new();
    for incident in incidents {
        let keys = compute_groups(incident, coordinator_group, now);
        for key in &keys {
            groups[group_index(*key)].incidents.push(incident);
        }
        membership.insert(incident.id().to_owned(), keys);
    }
    for group in &mut groups {
        let key = group.key;
        // `sorted` в Python устойчив — `sort_by` тоже.
        group.incidents.sort_by(|a, b| compare_in_group(*a, *b, key, now));
    }
    (groups, membership)
}

fn group_index(key: GroupKey) -> usize {
    GroupKey::ALL
        .iter()
        .position(|k| *k == key)
        .expect("GroupKey::ALL")
}

/// `{group: count}` в порядке групп.
pub fn group_counts<I: ?Sized>(groups: &[GroupResult<'_, I>]) -> Vec<(GroupKey, usize)> {
    groups.iter().map(|g| (g.key, g.incidents.len())).collect()
}

/// ARCHITECTURE.md: `assignedEmployee`, при отсутствии — название группы (`or` Python: пустая
/// строка тоже «отсутствие»).
pub fn assignee_of<I: Groupable + ?Sized>(incident: &I) -> Option<&str> {
    incident
        .assigned_employee()
        .filter(|s| !s.is_empty())
        .or(incident.assigned_group_name())
}

// ------------------------------------------------------------------ отметка «Решён» (C16)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResolutionState {
    pub phase: Option<ResolutionPhase>,
    pub detected_at: Option<DateTime<Utc>>,
    pub recently_resolved_until: Option<DateTime<Utc>>,
}

fn phase_for(state: NormalizedState) -> Option<ResolutionPhase> {
    match state {
        NormalizedState::Research => Some(ResolutionPhase::Research),
        NormalizedState::Resolved => Some(ResolutionPhase::Resolved),
        NormalizedState::Closed => Some(ResolutionPhase::Closed),
        _ => None,
    }
}

/// Переход отметки «Решён» при новой версии инцидента.
///
/// * активный критический → Resolved/Closed/Research: отметка ставится; учитывается ПРЕДЫДУЩИЙ
///   приоритет, поэтому одновременное понижение приоритета факт решения не теряет;
/// * Research → Resolved/Closed: начинается окно 24 ч;
/// * возврат в Assigned/WorkInProgress: отметка снимается сразу;
/// * неизвестный статус: отметка не меняется.
pub fn next_resolution(
    previous: &ResolutionState,
    previous_state: NormalizedState,
    previous_priority: i64,
    current_state: NormalizedState,
    now: DateTime<Utc>,
    window: TimeDelta,
) -> ResolutionState {
    if current_state.is_active() {
        return ResolutionState::default();
    }
    let Some(phase) = phase_for(current_state) else {
        return *previous;
    };

    if is_active_critical(previous_state, previous_priority) {
        let until = (phase != ResolutionPhase::Research).then(|| now + window);
        return ResolutionState {
            phase: Some(phase),
            detected_at: Some(now),
            recently_resolved_until: until,
        };
    }

    let Some(previous_phase) = previous.phase else {
        return *previous; // решённый не из активного критического — отметки нет
    };

    if phase == ResolutionPhase::Research {
        return ResolutionState {
            phase: Some(phase),
            detected_at: previous.detected_at,
            recently_resolved_until: None,
        };
    }
    // Resolved/Closed: окно стартует при выходе из исследования, иначе сохраняется.
    let until = match previous.recently_resolved_until {
        Some(until) if previous_phase != ResolutionPhase::Research => until,
        _ => now + window,
    };
    ResolutionState {
        phase: Some(phase),
        detected_at: previous.detected_at,
        recently_resolved_until: Some(until),
    }
}

pub const fn resolution_label(phase: ResolutionPhase) -> &'static str {
    match phase {
        ResolutionPhase::Research => "Решён · исследование",
        ResolutionPhase::Resolved => "Решён",
        ResolutionPhase::Closed => "Решён · закрыт",
    }
}

/// Откуда взято `resolvedAt` (`resolved_at_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedAtSource {
    CompletionDate,
    Detected,
}

impl ResolvedAtSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CompletionDate => "completionDate",
            Self::Detected => "detected",
        }
    }
}

/// Данные для `Resolution` схемы ответа (словарь `describe_resolution` Python-версии).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionInfo {
    pub phase: ResolutionPhase,
    pub label: &'static str,
    pub resolved_at: DateTime<Utc>,
    pub resolved_at_source: ResolvedAtSource,
    pub detected_at: DateTime<Utc>,
    pub recently_resolved_until: Option<DateTime<Utc>>,
}

/// `None`, если отметки нет или окно истекло.
pub fn describe_resolution<I: Groupable + ?Sized>(
    incident: &I,
    now: DateTime<Utc>,
) -> Option<ResolutionInfo> {
    if !has_resolution_mark(incident, now) {
        return None;
    }
    let phase = incident.resolution_phase()?;
    let detected = incident.resolved_detected_at().unwrap_or(now);
    let completed = incident.completion_date();
    Some(ResolutionInfo {
        phase,
        label: resolution_label(phase),
        resolved_at: completed.unwrap_or(detected),
        resolved_at_source: if completed.is_some() {
            ResolvedAtSource::CompletionDate
        } else {
            ResolvedAtSource::Detected
        },
        detected_at: detected,
        recently_resolved_until: incident.recently_resolved_until(),
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::constants::NormalizedState as S;

    #[test]
    fn group_name_comparison_ignores_spacing_and_case() {
        assert_eq!(
            normalize_group_name(Some("  КЦ ИТ  Координаторы\u{a0}Массовых Инцидентов ")),
            normalize_group_name(Some("кц ит координаторы массовых инцидентов"))
        );
        assert_eq!(normalize_group_name(None), "");
    }

    #[test]
    fn resolution_mark_lifecycle() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap();
        let day = TimeDelta::hours(24);
        let none = ResolutionState::default();
        // Критический → Research: отметка без окна.
        let research = next_resolution(&none, S::WorkInProgress, 4, S::Research, t0, day);
        assert_eq!(research.phase, Some(ResolutionPhase::Research));
        assert_eq!(
            (research.detected_at, research.recently_resolved_until),
            (Some(t0), None)
        );
        // Research → Resolved: окно от момента выхода из исследования.
        let t1 = t0 + TimeDelta::hours(2);
        let resolved = next_resolution(&research, S::Research, 4, S::Resolved, t1, day);
        assert_eq!(resolved.recently_resolved_until, Some(t1 + day));
        assert_eq!(resolved.detected_at, Some(t0));
        // Resolved → Closed: окно сохраняется.
        let closed = next_resolution(&resolved, S::Resolved, 4, S::Closed, t1 + day, day);
        assert_eq!(closed.recently_resolved_until, Some(t1 + day));
        // Возврат в работу снимает отметку; неизвестный статус её не трогает.
        assert_eq!(next_resolution(&closed, S::Closed, 4, S::Assigned, t1, day), none);
        assert_eq!(
            next_resolution(&closed, S::Closed, 4, S::Unknown, t1, day),
            closed
        );
        // Решённый не из критического — отметки нет.
        assert_eq!(
            next_resolution(&none, S::WorkInProgress, 3, S::Resolved, t0, day),
            none
        );
    }
}
