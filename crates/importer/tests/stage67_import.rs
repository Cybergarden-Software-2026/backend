//! Этапы 6–7 на PostgreSQL: порт `test_stage6_resolution.py` и импортной части
//! `test_stage7_notifications.py` (список/прочтение уведомлений — API, этап R5).
//! Нужен `TEST_DATABASE_URL`, без него тесты пропускаются.

mod common;

use common::*;
use dashboard_store::import_repo::{self, NewNotification};
use serde_json::{Value, json};

macro_rules! env_or_skip {
    () => {
        match Env::new().await {
            Some(e) => e,
            None => return,
        }
    };
}

async fn resolution(e: &Env) -> Value {
    e.json(&format!(
        "SELECT json_build_object('phase', resolution_phase, 'detected', resolved_detected_at, \
         'until', recently_resolved_until)::jsonb FROM incidents WHERE id='{DEMO_ID}'"
    ))
    .await
}

fn research() -> [(&'static str, Value); 4] {
    [
        ("stateEnum", s("Research")),
        ("stateId", Value::from(20)),
        ("state", s("Исследование ситуации")),
        ("completionDate", s("2026-11-20T12:30:00Z")),
    ]
}

// ================================================================== этап 6

#[tokio::test]
async fn demo_scenario_resolution_marks() {
    let mut e = env_or_skip!();
    let mut recs = records();
    e.write_json(&recs);
    e.tick().await;

    // 1. Повышение до критического + координаторы
    update(
        demo(&mut recs),
        &[
            ("priorityId", Value::from(4)),
            ("priority", s("Наивысший")),
            ("assignedGroupName", s(COORD)),
        ],
    );
    e.write_json(&recs);
    e.tick().await;
    assert_eq!(resolution(&e).await["phase"], Value::Null);

    // 3. Research / 20 + completionDate → «Решён · исследование», без ограничения времени
    update(demo(&mut recs), &research());
    e.write_json(&recs);
    e.tick().await;
    let r = resolution(&e).await;
    assert_eq!(r["phase"], "research");
    assert_eq!(r["until"], Value::Null);
    assert_ne!(r["detected"], Value::Null);

    // Research → Resolved: стартует окно 24 ч
    update(
        demo(&mut recs),
        &[
            ("stateEnum", s("Resolved")),
            ("stateId", Value::from(7)),
            ("state", s("Завершен")),
        ],
    );
    e.write_json(&recs);
    e.tick().await;
    let r = resolution(&e).await;
    assert_eq!(r["phase"], "resolved");
    assert_ne!(r["until"], Value::Null);

    // Возврат в работу снимает отметку сразу
    update(
        demo(&mut recs),
        &[
            ("stateEnum", s("WorkInProgress")),
            ("stateId", Value::from(2)),
            ("state", s("Выполняется")),
        ],
    );
    e.write_json(&recs);
    e.tick().await;
    assert_eq!(resolution(&e).await["phase"], Value::Null);
    let types = e
        .strings(&format!(
            "SELECT types FROM change_events WHERE incident_id='{DEMO_ID}' ORDER BY id DESC LIMIT 1"
        ))
        .await;
    // Снова в поле внимания: критический и на координаторах (типы этапа 7) + возврат в работу
    assert_eq!(types, ["critical", "assigned", "reopened"]);
    e.finish().await;
}

#[tokio::test]
async fn window_expires_after_24h_without_file_change() {
    let mut e = env_or_skip!();
    let mut recs = records();
    demo(&mut recs).insert("priorityId".into(), Value::from(4));
    e.write_json(&recs);
    e.tick().await;
    update(
        demo(&mut recs),
        &[
            ("stateEnum", s("Resolved")),
            ("stateId", Value::from(7)),
            ("state", s("Завершен")),
        ],
    );
    e.write_json(&recs);
    e.tick().await;
    let revision_before = e.i64("SELECT max(id) FROM dashboard_events").await;
    let rev_sql = format!("SELECT incident_revision::bigint FROM incidents WHERE id='{DEMO_ID}'");
    let incident_revision = e.i64(&rev_sql).await;

    e.tick_after(23.0 * 3600.0).await;
    assert_eq!(resolution(&e).await["phase"], "resolved");
    assert_eq!(
        e.i64("SELECT max(id) FROM dashboard_events").await,
        revision_before
    );

    e.tick_after(3601.0).await;
    assert_eq!(resolution(&e).await["phase"], Value::Null);
    assert_eq!(
        e.i64("SELECT max(id) FROM dashboard_events").await,
        revision_before + 1
    );
    assert_eq!(
        e.string("SELECT reason FROM dashboard_events ORDER BY id DESC LIMIT 1")
            .await
            .as_deref(),
        Some("expiry")
    );
    assert_eq!(
        e.json("SELECT payload FROM dashboard_events ORDER BY id DESC LIMIT 1")
            .await,
        json!({"schemaVersion": 1, "expired": [DEMO_ID]})
    );
    // Данные выгрузки не менялись — версия инцидента та же.
    assert_eq!(e.i64(&rev_sql).await, incident_revision);
    // Время обнаружения сохраняется (сбрасываются только фаза и окно).
    assert_ne!(resolution(&e).await["detected"], Value::Null);
    e.finish().await;
}

#[tokio::test]
async fn lowering_priority_together_with_resolution_keeps_mark() {
    let mut e = env_or_skip!();
    let mut recs = records();
    demo(&mut recs).insert("priorityId".into(), Value::from(4));
    e.write_json(&recs);
    e.tick().await;
    update(
        demo(&mut recs),
        &[
            ("priorityId", Value::from(2)),
            ("stateEnum", s("Closed")),
            ("stateId", Value::from(10)),
            ("state", s("Закрыт")),
        ],
    );
    e.write_json(&recs);
    e.tick().await;
    assert_eq!(resolution(&e).await["phase"], "closed");
    e.finish().await;
}

#[tokio::test]
async fn already_resolved_critical_at_initial_import_has_no_mark() {
    let mut e = env_or_skip!();
    let mut recs = records();
    update(
        demo(&mut recs),
        &[
            ("priorityId", Value::from(4)),
            ("stateEnum", s("Resolved")),
            ("stateId", Value::from(7)),
        ],
    );
    e.write_json(&recs);
    e.tick().await;
    assert_eq!(resolution(&e).await["phase"], Value::Null);
    e.finish().await;
}

// ================================================================== этап 7

async fn notification_types(e: &Env) -> Vec<String> {
    e.strings("SELECT coalesce(array_agg(type ORDER BY id), '{}') FROM notifications")
        .await
}

#[tokio::test]
async fn demo_scenario_notifications() {
    let mut e = env_or_skip!();
    let mut recs = records();
    e.write_json(&recs);
    e.tick().await;
    assert!(notification_types(&e).await.is_empty()); // первичная загрузка без уведомлений

    update(
        demo(&mut recs),
        &[
            ("priorityId", Value::from(4)),
            ("priority", s("Наивысший")),
            ("assignedGroupName", s(COORD)),
        ],
    );
    e.write_json(&recs);
    e.tick().await;
    assert_eq!(
        notification_types(&e).await,
        ["critical.raised", "coordinator.assigned"]
    );
    // Оба повода связаны с одним ChangeEvent — интерфейс может объединить их
    assert_eq!(
        e.i64("SELECT count(DISTINCT change_event_id) FROM notifications")
            .await,
        1
    );
    assert_eq!(
        e.strings("SELECT types FROM change_events ORDER BY id DESC LIMIT 1")
            .await,
        ["critical", "assigned", "team", "priority"]
    );
    // Поля уведомления — как в Python-версии.
    let row = e
        .json(
            "SELECT jsonb_build_object('pk', incident_pk, 'iid', incident_id, 'sys', sys_id, \
             'rev', incident_revision, 'ce', change_event_id, 'title', title, 'body', body, \
             'payload', payload, 'key', dedup_key, 'at', created_at) FROM notifications \
             WHERE type = 'critical.raised'",
        )
        .await;
    let ce = e.i64("SELECT max(id) FROM change_events").await;
    assert_eq!(row["iid"], DEMO_ID);
    assert_eq!(row["rev"], 2);
    assert_eq!(row["ce"], ce);
    assert_eq!(row["key"], format!("{DEMO_ID}:critical.raised:2"));
    assert_eq!(row["title"], format!("Критический инцидент {DEMO_ID}"));
    assert_eq!(row["payload"]["changeEventId"], ce);
    assert_eq!(row["payload"]["incidentRevision"], 2);
    assert_eq!(row["at"], "2026-11-20T12:00:02+00:00");

    demo(&mut recs).insert("deadline".into(), s("2026-11-25T12:00:00Z"));
    e.write_json(&recs);
    e.tick().await;
    assert_eq!(notification_types(&e).await.len(), 2);

    update(demo(&mut recs), &research());
    e.write_json(&recs);
    e.tick().await;
    assert_eq!(
        notification_types(&e).await,
        ["critical.raised", "coordinator.assigned", "critical.resolved"]
    );

    demo(&mut recs).insert("assignedGroupName".into(), s("ОГЭ Приморье"));
    e.write_json(&recs);
    e.tick().await;
    e.write_json(&recs); // повторное сохранение того же файла
    e.tick().await;
    assert_eq!(notification_types(&e).await.len(), 3);
    assert_eq!(
        e.i64(&format!(
            "SELECT incident_revision::bigint FROM incidents WHERE id='{DEMO_ID}'"
        ))
        .await,
        5
    );
    e.finish().await;
}

/// Повторная обработка (например, после сбоя) упирается в UNIQUE dedup_key.
#[tokio::test]
async fn reprocessing_same_revision_does_not_duplicate() {
    let mut e = env_or_skip!();
    let mut recs = records();
    e.write_json(&recs);
    e.tick().await;
    demo(&mut recs).insert("priorityId".into(), Value::from(4));
    e.write_json(&recs);
    e.tick().await;
    let key = e.string("SELECT dedup_key FROM notifications").await.unwrap();
    assert_eq!(key, format!("{DEMO_ID}:critical.raised:2"));
    let mut tx = e.pool().begin().await.unwrap();
    let again = import_repo::add_notification(
        &mut tx,
        &NewNotification {
            kind: "critical.raised".into(),
            title: "dup".into(),
            dedup_key: key,
            ..NewNotification::default()
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(again, None);
    assert_eq!(e.i64("SELECT count(*) FROM notifications").await, 1);
    e.finish().await;
}

#[tokio::test]
async fn repeated_coordinator_assignments_notify_each_time() {
    let mut e = env_or_skip!();
    let mut recs = records();
    e.write_json(&recs);
    e.tick().await;
    for group in [COORD, "ОГЭ Приморье", COORD, "ОГЭ Приморье", COORD] {
        demo(&mut recs).insert("assignedGroupName".into(), s(group));
        e.write_json(&recs);
        e.tick().await;
    }
    assert_eq!(notification_types(&e).await, ["coordinator.assigned"; 3]);
    assert_eq!(
        e.strings("SELECT array_agg(dedup_key ORDER BY id) FROM notifications")
            .await,
        [2, 4, 6].map(|r| format!("{DEMO_ID}:coordinator.assigned:{r}"))
    );
    e.finish().await;
}

#[tokio::test]
async fn source_notifications_recorded() {
    let mut e = env_or_skip!();
    e.write_json(&records());
    e.tick().await;
    e.write_text("{broken");
    e.tick().await;
    e.tick().await;
    e.write_json(&records());
    e.tick().await;
    assert_eq!(notification_types(&e).await, ["source.error", "source.recovered"]);
    assert_eq!(
        e.i64("SELECT count(*) FROM notifications WHERE incident_id IS NULL AND incident_pk IS NULL")
            .await,
        2
    );
    let body = e
        .string("SELECT body FROM notifications WHERE type='source.error'")
        .await
        .unwrap();
    assert!(body.starts_with("некорректный JSON"), "{body}");
    assert!(body.ends_with(". Показан последний принятый снимок."), "{body}");
    e.finish().await;
}
