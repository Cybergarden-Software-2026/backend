//! Порт `backend/tests/unit/test_stage7_notifications.py` (без БД): переходы, типы
//! `critical`/`assigned`, уведомления, дедупликация, курсоры, сценарий демонстрации на плане импорта.

mod common;

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use dashboard_domain::change_detector::{
    ImportPlan, IncidentPlan, PlanOptions, StoredIncident, build_import_plan,
};
use dashboard_domain::constants::{
    ChangeEventType as T, ChangeType, NormalizedState as S, NotificationType as N,
};
use dashboard_domain::cursors::{InvalidCursor, parse_cursor};
use dashboard_domain::notification_rules::{MembershipFacts, dedup_key, detect_transitions};
use dashboard_domain::validator::validate_snapshot;
use serde_json::{Map, Value, json};

use common::{sample, values};

const COORD: &str = "КЦ ИТ Координаторы Массовых Инцидентов";
const DEMO_ID: &str = "INC000021938167";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
}

/// Имитация реестра: применяет план так же, как `import_service` (без БД).
struct Registry {
    /// Порядок — порядок первой вставки, как у `dict` Python.
    rows: Vec<StoredIncident>,
    /// `dedup_key → type` в порядке вставки (как UNIQUE в БД).
    notifications: Vec<(String, N)>,
    now: DateTime<Utc>,
    initial_done: bool,
}

impl Registry {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            notifications: Vec::new(),
            now: t0(),
            initial_done: false,
        }
    }

    fn row(&self, id: &str) -> Option<&StoredIncident> {
        self.rows.iter().find(|r| r.id == id)
    }

    fn put(&mut self, row: StoredIncident) {
        match self.rows.iter_mut().find(|r| r.id == row.id) {
            Some(slot) => *slot = row,
            None => self.rows.push(row),
        }
    }

    fn apply(&mut self, records: &[Map<String, Value>]) -> ImportPlan {
        self.now += TimeDelta::seconds(1);
        let incoming = validate_snapshot(values(records.to_vec()), Tz::UTC)
            .unwrap()
            .incidents;
        let options = PlanOptions {
            coordinator_group: COORD.to_owned(),
            now: Some(self.now),
            ..PlanOptions::new(!self.initial_done)
        };
        let plan = build_import_plan(&self.rows, incoming, &options);
        self.initial_done = true;
        for item in &plan.items {
            let prev = self.row(&item.incident_id).cloned();
            if item.kind == ChangeType::Removed {
                let prev = prev.expect("выбывший был в реестре");
                self.put(StoredIncident {
                    is_present: false,
                    incident_revision: item.new_revision,
                    ..prev
                });
                continue;
            }
            let inc = item.current.as_ref().expect("запись из снимка");
            let c = &inc.columns;
            let res = item.resolution;
            let mut row = StoredIncident::new(
                prev.as_ref().map_or(self.rows.len() as i64 + 1, |p| p.pk),
                &inc.id,
                &inc.sys_id,
                inc.raw.clone(),
                &inc.content_hash,
                item.new_revision,
                true,
                c.normalized_state,
            );
            row.priority_id = c.priority_id.unwrap();
            row.assigned_group_name = c.assigned_group_name.clone();
            // Как в Python-тесте: без перехода отметки фаза берётся из прежней строки,
            // а моменты сбрасываются.
            row.resolution_phase = match res {
                Some(r) => r.phase,
                None => prev.as_ref().and_then(|p| p.resolution_phase),
            };
            row.resolved_detected_at = res.and_then(|r| r.detected_at);
            row.recently_resolved_until = res.and_then(|r| r.recently_resolved_until);
            self.put(row);
            for d in &item.notifications {
                if !self.notifications.iter().any(|(k, _)| *k == d.dedup_key) {
                    self.notifications.push((d.dedup_key.clone(), d.kind));
                }
            }
        }
        plan
    }
}

fn demo_item(plan: &ImportPlan) -> &IncidentPlan {
    plan.items
        .iter()
        .find(|i| i.incident_id == DEMO_ID)
        .expect("элемент плана демо-инцидента")
}

fn demo(records: &mut [Map<String, Value>]) -> &mut Map<String, Value> {
    records.iter_mut().find(|r| r["id"] == DEMO_ID).unwrap()
}

fn set(record: &mut Map<String, Value>, patch: Value) {
    for (k, v) in patch.as_object().unwrap() {
        record.insert(k.clone(), v.clone());
    }
}

fn kinds(item: &IncidentPlan) -> Vec<N> {
    item.notifications.iter().map(|n| n.kind).collect()
}

// ================================================================ правила

fn facts(state: S, priority: i64, group: &str) -> MembershipFacts {
    MembershipFacts {
        state,
        priority_id: priority,
        assigned_group_name: Some(group.to_owned()),
    }
}

const ENG: &str = "ОГЭ Приморье";

#[test]
fn test_detect_transitions() {
    let wip = S::WorkInProgress;
    let cases: [(Option<MembershipFacts>, Option<MembershipFacts>, bool, bool); 10] = [
        (None, Some(facts(wip, 4, ENG)), true, false), // новая критическая запись
        (Some(facts(wip, 3, ENG)), Some(facts(wip, 4, ENG)), true, false), // повышение до 4
        (Some(facts(wip, 4, ENG)), Some(facts(wip, 4, ENG)), false, false), // уже был критическим
        (
            Some(facts(S::Resolved, 4, ENG)),
            Some(facts(wip, 4, ENG)),
            true,
            false,
        ), // возврат в поле внимания
        (
            Some(facts(wip, 4, ENG)),
            Some(facts(S::Research, 4, ENG)),
            false,
            false,
        ), // Research — тоже «внимание»
        (Some(facts(wip, 2, ENG)), Some(facts(wip, 2, COORD)), false, true),
        (
            Some(facts(wip, 2, COORD)),
            Some(facts(wip, 2, COORD)),
            false,
            false,
        ),
        (
            Some(facts(S::Closed, 2, COORD)),
            Some(facts(S::Assigned, 2, COORD)),
            false,
            true,
        ),
        (
            Some(facts(wip, 2, ENG)),
            Some(facts(S::Resolved, 2, COORD)),
            false,
            false,
        ), // решённый — не в группе
        (Some(facts(wip, 4, ENG)), None, false, false), // выбытие — не оповещение
    ];
    for (prev, cur, critical, coordinators) in cases {
        let t = detect_transitions(prev.as_ref(), cur.as_ref(), COORD, false);
        assert_eq!(
            (t.entered_critical, t.entered_coordinators),
            (critical, coordinators),
            "{prev:?} → {cur:?}"
        );
    }
}

#[test]
fn test_dedup_key_format() {
    assert_eq!(
        dedup_key(DEMO_ID, N::CriticalRaised, 2),
        "INC000021938167:critical.raised:2"
    );
}

#[test]
fn test_cursor_parsing() {
    for (cursor, expected) in [(None, None), (Some(""), None), (Some("42"), Some(42))] {
        assert_eq!(parse_cursor(cursor, "cursor"), Ok(expected));
    }
}

#[test]
fn test_bad_cursor_rejected() {
    let nineteen = "9".repeat(19);
    for cursor in ["abc", "-1", "1.5", nineteen.as_str()] {
        let err = parse_cursor(Some(cursor), "cursor").unwrap_err();
        assert_eq!(
            err,
            InvalidCursor {
                name: "cursor".into()
            }
        );
        assert_eq!(
            (InvalidCursor::CODE, err.to_string().as_str()),
            ("INVALID_CURSOR", "Некорректный параметр cursor")
        );
    }
}

// ================================================================ первичная загрузка

#[test]
fn test_initial_import_creates_no_notifications() {
    let mut reg = Registry::new();
    let plan = reg.apply(&sample());
    assert_eq!(plan.count(ChangeType::Created), 44);
    assert!(reg.notifications.is_empty());
}

// ================================================================ сценарий демонстрации

#[test]
fn test_full_demo_scenario() {
    let mut reg = Registry::new();
    let mut recs = sample();
    reg.apply(&recs);

    // 1. priorityId 3 → 4 + координаторы: два повода для оповещения в одном событии
    set(
        demo(&mut recs),
        json!({"priorityId": 4, "priority": "Наивысший", "assignedGroupName": COORD}),
    );
    let plan = reg.apply(&recs);
    let item = demo_item(&plan);
    assert_eq!(item.event_types, [T::Critical, T::Assigned, T::Team, T::Priority]);
    assert_eq!(kinds(item), [N::CriticalRaised, N::CoordinatorAssigned]);
    for n in &item.notifications {
        assert_eq!(n.payload["eventTypes"][0], "critical");
    }
    assert_eq!(reg.notifications.len(), 2);

    // 2. Новый дедлайн — карточка обновляется, оповещений не прибавляется
    set(demo(&mut recs), json!({"deadline": "2026-11-25T12:00:00Z"}));
    let plan = reg.apply(&recs);
    let item = demo_item(&plan);
    assert_eq!(item.event_types, [T::Deadline]);
    assert!(item.notifications.is_empty());
    assert_eq!(reg.notifications.len(), 2);

    // 3. Research / 20 + completionDate, координаторы сохраняются: оповещение о решении
    set(
        demo(&mut recs),
        json!({
            "stateEnum": "Research", "stateId": 20, "state": "Исследование ситуации",
            "completionDate": "2026-11-20T12:30:00Z"
        }),
    );
    let plan = reg.apply(&recs);
    let item = demo_item(&plan);
    assert_eq!(item.event_types, [T::Research]);
    assert_eq!(kinds(item), [N::CriticalResolved]);
    assert!(item.notifications[0].body.contains("идёт исследование"));
    assert_eq!(
        reg.row(DEMO_ID).unwrap().resolution_phase.map(|p| p.as_str()),
        Some("research")
    );
    assert_eq!(reg.notifications.len(), 3);

    // 4. Переназначение на инженеров: уходит из координаторов, без оповещения
    set(demo(&mut recs), json!({"assignedGroupName": ENG}));
    let plan = reg.apply(&recs);
    let item = demo_item(&plan);
    assert_eq!(item.event_types, [T::Team]);
    assert!(item.notifications.is_empty());

    // 5. Повторное сохранение того же файла — ни изменений, ни дублей
    let plan = reg.apply(&recs);
    assert!(!plan.has_changes());
    let mut types: Vec<N> = reg.notifications.iter().map(|(_, t)| *t).collect();
    types.sort();
    assert_eq!(
        types,
        [N::CriticalRaised, N::CoordinatorAssigned, N::CriticalResolved]
    );
}

/// Выход из группы и повторный вход — новое оповещение (новая incidentRevision).
#[test]
fn test_reentry_into_group_creates_new_notification() {
    let mut reg = Registry::new();
    let mut recs = sample();
    reg.apply(&recs);
    for group in [COORD, ENG, COORD] {
        set(demo(&mut recs), json!({"assignedGroupName": group}));
        reg.apply(&recs);
    }
    let assigned: Vec<&str> = reg
        .notifications
        .iter()
        .filter(|(_, t)| *t == N::CoordinatorAssigned)
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(
        assigned,
        [
            format!("{DEMO_ID}:coordinator.assigned:2"),
            format!("{DEMO_ID}:coordinator.assigned:4")
        ]
    );
}

#[test]
fn test_resolved_critical_and_reactivation() {
    let mut reg = Registry::new();
    let mut recs = sample();
    reg.apply(&recs);
    set(demo(&mut recs), json!({"priorityId": 4}));
    reg.apply(&recs);
    set(
        demo(&mut recs),
        json!({"stateEnum": "Resolved", "stateId": 7, "state": "Завершен"}),
    );
    let plan = reg.apply(&recs);
    let item = demo_item(&plan);
    assert_eq!(item.event_types, [T::Resolved]);
    assert_eq!(kinds(item), [N::CriticalResolved]);

    set(
        demo(&mut recs),
        json!({"stateEnum": "WorkInProgress", "stateId": 2, "state": "Выполняется"}),
    );
    let plan = reg.apply(&recs);
    let item = demo_item(&plan);
    assert_eq!(item.event_types, [T::Critical, T::Reopened]); // снова в поле внимания
    assert_eq!(kinds(item), [N::CriticalRaised]);
    assert_eq!(reg.row(DEMO_ID).unwrap().resolution_phase, None);
}

#[test]
fn test_new_critical_incident_after_initial_load() {
    let mut reg = Registry::new();
    let mut recs = sample();
    reg.apply(&recs);
    let mut new = demo(&mut recs).clone();
    set(
        &mut new,
        json!({"id": "INC000099999999", "sysId": "999999999999999999", "priorityId": 4, "assignedGroupName": COORD}),
    );
    recs.push(new);
    let plan = reg.apply(&recs);
    let [item] = plan.items.as_slice() else {
        panic!("ожидался один элемент плана: {:?}", plan.items.len());
    };
    assert_eq!(item.event_types, [T::Created, T::Critical, T::Assigned]);
    assert_eq!(kinds(item), [N::CriticalRaised, N::CoordinatorAssigned]);
}

#[test]
fn test_non_critical_resolution_no_notification() {
    let mut reg = Registry::new();
    let mut recs = sample();
    reg.apply(&recs);
    set(demo(&mut recs), json!({"stateEnum": "Resolved", "stateId": 7})); // priorityId = 3
    let plan = reg.apply(&recs);
    let item = demo_item(&plan);
    assert_eq!(item.event_types, [T::Resolved]);
    assert!(item.notifications.is_empty());
}

#[test]
fn test_removed_critical_is_not_resolution() {
    let mut reg = Registry::new();
    let mut recs = sample();
    reg.apply(&recs);
    set(demo(&mut recs), json!({"priorityId": 4}));
    reg.apply(&recs);
    let without: Vec<_> = recs.iter().filter(|r| r["id"] != DEMO_ID).cloned().collect();
    let plan = reg.apply(&without);
    let item = demo_item(&plan);
    assert_eq!(item.event_types, [T::Removed]);
    assert!(item.notifications.is_empty());
}
