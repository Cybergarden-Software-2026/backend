//! Правила переходов: какие изменения требуют оповещения координатора — порт
//! `app/services/notification_rules.py` (чистая логика).
//!
//! * `critical.raised`      — вход в «активные критические»: priorityId = 4 и требует внимания
//!   (новая критическая запись, повышение до 4, возврат критического в работу);
//! * `coordinator.assigned` — вход в группу координаторов;
//! * `critical.resolved`    — у критического появилась отметка «Решён» (решение C16).
//!
//! Смена срока или сотрудника внутри той же группы оповещений не создаёт (демо-шаг 2).
//! Повторный вход после выхода — новое оповещение: ключ дедупликации включает incidentRevision.

use serde_json::{Map, Value, json};

use crate::constants::{ChangeEventType, NormalizedState, NotificationType, PRIORITY_CRITICAL};
use crate::grouping::normalize_group_name;
use crate::normalizer::{Columns, iso_z};

/// То, от чего зависят переходы, для одной версии записи.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipFacts {
    pub state: NormalizedState,
    pub priority_id: i64,
    pub assigned_group_name: Option<String>,
}

impl MembershipFacts {
    pub fn attention(&self) -> bool {
        self.state.is_attention()
    }

    pub fn is_critical(&self) -> bool {
        self.attention() && self.priority_id == PRIORITY_CRITICAL
    }

    pub fn is_on_coordinators(&self, coordinator_group: &str) -> bool {
        self.attention()
            && normalize_group_name(self.assigned_group_name.as_deref())
                == normalize_group_name(Some(coordinator_group))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Transitions {
    pub entered_critical: bool,
    pub entered_coordinators: bool,
    pub critical_resolved: bool,
}

impl Transitions {
    pub fn event_types(&self) -> Vec<ChangeEventType> {
        let mut types = Vec::new();
        if self.entered_critical {
            types.push(ChangeEventType::Critical);
        }
        if self.entered_coordinators {
            types.push(ChangeEventType::Assigned);
        }
        types
    }
}

pub fn detect_transitions(
    previous: Option<&MembershipFacts>,
    current: Option<&MembershipFacts>,
    coordinator_group: &str,
    resolution_marked: bool,
) -> Transitions {
    let Some(current) = current else {
        return Transitions::default(); // запись выбыла из снимка — это не решение и не назначение
    };
    Transitions {
        entered_critical: current.is_critical() && !previous.is_some_and(MembershipFacts::is_critical),
        entered_coordinators: current.is_on_coordinators(coordinator_group)
            && !previous.is_some_and(|p| p.is_on_coordinators(coordinator_group)),
        critical_resolved: resolution_marked,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotificationDraft {
    pub kind: NotificationType,
    pub title: String,
    pub body: String,
    pub dedup_key: String,
    pub payload: Map<String, Value>,
}

/// Уникальный ключ `(incidentId, eventType, incidentRevision)` из ARCHITECTURE.md.
pub fn dedup_key(incident_id: &str, kind: NotificationType, incident_revision: i64) -> String {
    format!("{incident_id}:{}:{incident_revision}", kind.as_str())
}

fn state_label(state: NormalizedState) -> &'static str {
    match state {
        NormalizedState::Resolved => "завершён",
        NormalizedState::Closed => "закрыт",
        NormalizedState::Research => "решён, идёт исследование ситуации",
        other => other.as_str(),
    }
}

pub fn build_notifications(
    transitions: &Transitions,
    incident_id: &str,
    sys_id: &str,
    incident_revision: i64,
    columns: &Columns,
    event_types: &[ChangeEventType],
) -> Vec<NotificationDraft> {
    let header = columns.header.as_deref().unwrap_or("Без заголовка");
    let product = columns.product_name.as_deref().unwrap_or("продукт не указан");
    let assignee = columns
        .assigned_employee
        .as_deref()
        .or(columns.assigned_group_name.as_deref())
        .unwrap_or("исполнитель не указан");
    let payload = match json!({
        "incidentId": incident_id,
        "sysId": sys_id,
        "incidentRevision": incident_revision,
        "priorityId": columns.priority_id,
        "productName": columns.product_name,
        "assignedGroupName": columns.assigned_group_name,
        "assignedEmployee": columns.assigned_employee,
        "deadline": columns.deadline.as_ref().map(iso_z),
        "eventTypes": event_types.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
    }) {
        Value::Object(map) => map,
        _ => unreachable!(),
    };

    let mut drafts = Vec::new();
    let mut add = |kind: NotificationType, title: String, body: String| {
        drafts.push(NotificationDraft {
            kind,
            title,
            body,
            dedup_key: dedup_key(incident_id, kind, incident_revision),
            payload: payload.clone(),
        });
    };
    if transitions.entered_critical {
        add(
            NotificationType::CriticalRaised,
            format!("Критический инцидент {incident_id}"),
            format!("{header}. Продукт: {product}. Исполнитель: {assignee}."),
        );
    }
    if transitions.entered_coordinators {
        add(
            NotificationType::CoordinatorAssigned,
            format!("Назначен на координаторов: {incident_id}"),
            format!("{header}. Продукт: {product}. Нужно передать в профильную команду."),
        );
    }
    if transitions.critical_resolved {
        add(
            NotificationType::CriticalResolved,
            format!("Критический инцидент решён: {incident_id}"),
            format!("{header}. Статус: {}.", state_label(columns.normalized_state)),
        );
    }
    drafts
}
