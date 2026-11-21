//! Сравнение нового снимка с последним принятым реестром и план импорта — порт
//! `app/importer/change_detector.py`.
//!
//! Сопоставление — по `id`. Сравниваются нормализованные значения (`tracked`), поэтому
//! перестановка записей, пробелы, `""` vs `null` и формат даты событий не создают.
//! Модуль чистый: план исполняет импортёр в одной транзакции PostgreSQL (этап R4).

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, TimeDelta, Utc};
use chrono_tz::Tz;
use serde_json::{Map, Value};

use crate::constants::{ChangeEventType, ChangeType, NormalizedState, ResolutionPhase};
use crate::grouping::{ResolutionState, next_resolution};
use crate::normalizer::{NormalizedIncident, normalize_fields};
use crate::notification_rules::{
    MembershipFacts, NotificationDraft, Transitions, build_notifications, detect_transitions,
};
use crate::pycompat::py_eq;

pub const DEFAULT_COORDINATOR_GROUP: &str = "КЦ ИТ Координаторы Массовых Инцидентов";

/// Окно отметки «Решён» по умолчанию — 24 ч.
pub fn default_window() -> TimeDelta {
    TimeDelta::hours(24)
}

/// Вид элемента плана — те же значения, что `ChangeType` (`created|updated|removed|restored`).
pub type PlanKind = ChangeType;

/// Минимум из таблицы `incidents`, нужный для сравнения.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredIncident {
    pub pk: i64,
    pub id: String,
    pub sys_id: String,
    pub raw: Map<String, Value>,
    pub content_hash: String,
    pub incident_revision: i64,
    pub is_present: bool,
    pub normalized_state: NormalizedState,
    pub priority_id: i64,
    pub assigned_group_name: Option<String>,
    pub resolution_phase: Option<ResolutionPhase>,
    pub resolved_detected_at: Option<DateTime<Utc>>,
    pub recently_resolved_until: Option<DateTime<Utc>>,
}

impl StoredIncident {
    /// Обязательные поля; остальные — значения по умолчанию dataclass Python
    /// (`priority_id = 1`, группа и отметка «Решён» не заданы).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pk: i64,
        id: impl Into<String>,
        sys_id: impl Into<String>,
        raw: Map<String, Value>,
        content_hash: impl Into<String>,
        incident_revision: i64,
        is_present: bool,
        normalized_state: NormalizedState,
    ) -> Self {
        Self {
            pk,
            id: id.into(),
            sys_id: sys_id.into(),
            raw,
            content_hash: content_hash.into(),
            incident_revision,
            is_present,
            normalized_state,
            priority_id: 1,
            assigned_group_name: None,
            resolution_phase: None,
            resolved_detected_at: None,
            recently_resolved_until: None,
        }
    }

    pub fn facts(&self) -> MembershipFacts {
        MembershipFacts {
            state: self.normalized_state,
            priority_id: self.priority_id,
            assigned_group_name: self.assigned_group_name.clone(),
        }
    }

    pub fn resolution(&self) -> ResolutionState {
        ResolutionState {
            phase: self.resolution_phase,
            detected_at: self.resolved_detected_at,
            recently_resolved_until: self.recently_resolved_until,
        }
    }
}

/// Изменение поля: нормализованные значения (`null` — значения не было).
#[derive(Debug, Clone, PartialEq)]
pub struct FieldChange {
    pub field: String,
    pub old: Value,
    pub new: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IncidentPlan {
    pub kind: PlanKind,
    pub incident_id: String,
    pub sys_id: String,
    pub pk: Option<i64>,
    pub new_revision: i64,
    pub previous_state: Option<NormalizedState>,
    pub current_state: Option<NormalizedState>,
    pub previous_raw: Option<Map<String, Value>>,
    pub current: Option<NormalizedIncident>,
    pub changes: Vec<FieldChange>,
    pub event_types: Vec<ChangeEventType>,
    /// Отметка «Решён» после применения (`None` — не менять: созданные и выбывшие записи).
    pub resolution: Option<ResolutionState>,
    pub transitions: Transitions,
    pub notifications: Vec<NotificationDraft>,
}

impl IncidentPlan {
    #[allow(clippy::too_many_arguments)]
    fn new(
        kind: PlanKind,
        incident_id: String,
        sys_id: String,
        pk: Option<i64>,
        new_revision: i64,
        previous_state: Option<NormalizedState>,
        previous_raw: Option<Map<String, Value>>,
        current: Option<NormalizedIncident>,
        changes: Vec<FieldChange>,
    ) -> Self {
        Self {
            kind,
            incident_id,
            sys_id,
            pk,
            new_revision,
            previous_state,
            current_state: current.as_ref().map(|c| c.state.state),
            previous_raw,
            current,
            changes,
            event_types: Vec::new(),
            resolution: None,
            transitions: Transitions::default(),
            notifications: Vec::new(),
        }
    }

    pub fn changed_fields(&self) -> Vec<&str> {
        self.changes.iter().map(|c| c.field.as_str()).collect()
    }

    /// Заголовок текущей версии, у выбывшей — из последней сохранённой.
    pub fn header(&self) -> Option<&str> {
        match &self.current {
            Some(current) => current.header(),
            None => self.previous_raw.as_ref()?.get("header")?.as_str(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImportPlan {
    pub is_initial: bool,
    pub items: Vec<IncidentPlan>,
    /// Записи без смысловых изменений, у которых изменился только `content_hash`
    /// (например, после правки нормализатора) — обновляются тихо, без новой ревизии. `pk → hash`.
    pub hash_updates: BTreeMap<i64, String>,
}

impl ImportPlan {
    pub fn has_changes(&self) -> bool {
        !self.items.is_empty()
    }

    pub fn count(&self, kind: PlanKind) -> usize {
        self.items.iter().filter(|i| i.kind == kind).count()
    }
}

/// Параметры построения плана (именованные аргументы Python-версии).
#[derive(Debug, Clone)]
pub struct PlanOptions {
    pub is_initial: bool,
    pub naive_tz: Tz,
    pub coordinator_group: String,
    /// Момент для отметки «Решён»; `None` — текущее время UTC.
    pub now: Option<DateTime<Utc>>,
    pub resolved_window: TimeDelta,
}

impl PlanOptions {
    pub fn new(is_initial: bool) -> Self {
        Self {
            is_initial,
            naive_tz: Tz::UTC,
            coordinator_group: DEFAULT_COORDINATOR_GROUP.to_owned(),
            now: None,
            resolved_window: default_window(),
        }
    }
}

/// Пополевые изменения; порядок — по порядку полей в новой записи, затем исчезнувшие поля.
pub fn diff_tracked(old: &Map<String, Value>, new: &Map<String, Value>) -> Vec<FieldChange> {
    let mut changes: Vec<FieldChange> = new
        .iter()
        .filter_map(|(k, v)| {
            let before = old.get(k).unwrap_or(&Value::Null);
            (!py_eq(before, v)).then(|| FieldChange {
                field: k.clone(),
                old: before.clone(),
                new: v.clone(),
            })
        })
        .collect();
    changes.extend(
        old.iter()
            .filter(|(k, v)| !new.contains_key(*k) && !v.is_null())
            .map(|(k, v)| FieldChange {
                field: k.clone(),
                old: v.clone(),
                new: Value::Null,
            }),
    );
    changes
}

fn sorted_unique(mut types: Vec<ChangeEventType>) -> Vec<ChangeEventType> {
    types.sort();
    types.dedup();
    types
}

/// Типы ChangeEvent, определяемые по статусу и полям (API.md §5).
///
/// `critical` и `assigned` зависят от групп и добавляются правилами переходов.
pub fn classify_event_types(
    kind: PlanKind,
    previous_state: Option<NormalizedState>,
    current_state: Option<NormalizedState>,
    changed_fields: &[&str],
) -> Vec<ChangeEventType> {
    use ChangeEventType as T;
    if kind == ChangeType::Removed {
        return vec![T::Removed];
    }
    let existing = matches!(kind, ChangeType::Updated | ChangeType::Restored);
    let mut types = Vec::new();
    if matches!(kind, ChangeType::Created | ChangeType::Restored) {
        types.push(T::Created);
    }
    if let Some(previous) = previous_state.filter(|p| existing && Some(*p) != current_state) {
        match current_state {
            Some(NormalizedState::Resolved) => types.push(T::Resolved),
            Some(NormalizedState::Closed) => types.push(T::Closed),
            Some(NormalizedState::Research) => types.push(T::Research),
            _ => {}
        }
        if current_state.is_some_and(NormalizedState::is_active) && previous.is_resolved_phase() {
            types.push(T::Reopened);
        }
    }
    // Типы по полям — только для изменений существующей записи: у новой «изменились» все поля.
    if existing {
        let has = |f: &str| changed_fields.contains(&f);
        if has("deadline") {
            types.push(T::Deadline);
        }
        if has("assignedGroupName") {
            types.push(T::Team);
        }
        if has("priorityId") {
            types.push(T::Priority);
        }
    }
    if types.is_empty() {
        types.push(T::Changed); // только у updated без статусных/полевых типов
    }
    sorted_unique(types)
}

fn previous_tracked(stored: &StoredIncident, naive_tz: Tz) -> Option<Map<String, Value>> {
    normalize_fields(&stored.raw, naive_tz)
        .ok()
        .map(|(_, tracked, _)| tracked)
}

/// План импорта.
pub fn build_import_plan(
    stored: &[StoredIncident],
    incoming: Vec<NormalizedIncident>,
    options: &PlanOptions,
) -> ImportPlan {
    let now = options.now.unwrap_or_else(Utc::now);
    let mut plan = ImportPlan {
        is_initial: options.is_initial,
        ..ImportPlan::default()
    };
    let by_id: HashMap<&str, &StoredIncident> = stored.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut incoming_ids: HashSet<String> = HashSet::new();

    for incident in incoming {
        incoming_ids.insert(incident.id.clone());
        let Some(prev) = by_id.get(incident.id.as_str()).copied() else {
            let changes = incident
                .tracked
                .iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| FieldChange {
                    field: k.clone(),
                    old: Value::Null,
                    new: v.clone(),
                })
                .collect();
            plan.items.push(IncidentPlan::new(
                ChangeType::Created,
                incident.id.clone(),
                incident.sys_id.clone(),
                None,
                1,
                None,
                None,
                Some(incident),
                changes,
            ));
            continue;
        };

        if prev.is_present && prev.content_hash == incident.content_hash {
            continue; // быстрый путь: запись не изменилась
        }

        let changes = match previous_tracked(prev, options.naive_tz) {
            Some(old) => diff_tracked(&old, &incident.tracked),
            None => incident
                .tracked
                .iter()
                .map(|(k, v)| FieldChange {
                    field: k.clone(),
                    old: Value::Null,
                    new: v.clone(),
                })
                .collect(),
        };
        if prev.is_present && changes.is_empty() {
            plan.hash_updates.insert(prev.pk, incident.content_hash.clone());
            continue;
        }

        plan.items.push(IncidentPlan::new(
            if prev.is_present {
                ChangeType::Updated
            } else {
                ChangeType::Restored
            },
            incident.id.clone(),
            incident.sys_id.clone(),
            Some(prev.pk),
            prev.incident_revision + 1,
            Some(prev.normalized_state),
            Some(prev.raw.clone()),
            Some(incident),
            changes,
        ));
    }

    for prev in stored {
        if prev.is_present && !incoming_ids.contains(&prev.id) {
            plan.items.push(IncidentPlan::new(
                ChangeType::Removed,
                prev.id.clone(),
                prev.sys_id.clone(),
                Some(prev.pk),
                prev.incident_revision + 1,
                Some(prev.normalized_state),
                Some(prev.raw.clone()),
                None,
                Vec::new(),
            ));
        }
    }

    for item in &mut plan.items {
        let prev = by_id.get(item.incident_id.as_str()).copied();
        apply_transitions(item, prev, options, now);
    }
    plan
}

/// Типы события, отметка «Решён» и черновики уведомлений для одного элемента плана.
fn apply_transitions(
    item: &mut IncidentPlan,
    prev: Option<&StoredIncident>,
    options: &PlanOptions,
    now: DateTime<Utc>,
) {
    let mut types = classify_event_types(
        item.kind,
        item.previous_state,
        item.current_state,
        &item.changed_fields(),
    );

    let mut resolution_marked = false;
    let existing = matches!(item.kind, ChangeType::Updated | ChangeType::Restored);
    if let (true, Some(prev), Some(current_state)) = (existing, prev, item.current_state) {
        let before = prev.resolution();
        let after = next_resolution(
            &before,
            prev.normalized_state,
            prev.priority_id,
            current_state,
            now,
            options.resolved_window,
        );
        resolution_marked = before.phase.is_none() && after.phase.is_some();
        item.resolution = Some(after);
    }

    let current_facts = item.current.as_ref().map(|c| MembershipFacts {
        state: c.state.state,
        // Валидатор гарантирует priorityId; без него запись не входит ни в одну группу.
        priority_id: c.columns.priority_id.unwrap_or(0),
        assigned_group_name: c.columns.assigned_group_name.clone(),
    });
    // Вернувшаяся в снимок запись — повторный вход в группы: сравниваем «с пустым местом».
    let previous_facts = prev
        .filter(|_| item.kind == ChangeType::Updated)
        .map(StoredIncident::facts);
    let transitions = detect_transitions(
        previous_facts.as_ref(),
        current_facts.as_ref(),
        &options.coordinator_group,
        resolution_marked,
    );
    item.transitions = transitions;

    types.extend(transitions.event_types());
    if types.len() > 1 {
        // «changed» — только когда других поводов нет.
        if let Some(pos) = types.iter().position(|t| *t == ChangeEventType::Changed) {
            types.remove(pos);
        }
    }
    item.event_types = sorted_unique(types);

    // Первичная загрузка архива уведомлений не создаёт.
    if !options.is_initial {
        if let Some(current) = &item.current {
            item.notifications = build_notifications(
                &transitions,
                &item.incident_id,
                &item.sys_id,
                item.new_revision,
                &current.columns,
                &item.event_types,
            );
        }
    }
}
