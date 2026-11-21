//! Этап 5 на PostgreSQL: порт `backend/tests/integration/test_stage5_import.py`.
//! Нужен `TEST_DATABASE_URL`, без него тесты пропускаются.

mod common;

use chrono::{TimeZone, Utc};
use common::*;
use dashboard_domain::constants::SourceStatus;
use dashboard_importer::{IMPORTER_LOCK_KEY, OutcomeStatus};
use serde_json::Value;
use sqlx::{ConnectOptions, PgConnection};

macro_rules! env_or_skip {
    () => {
        match Env::new().await {
            Some(e) => e,
            None => return,
        }
    };
}

// ================================================================== успешный импорт

#[tokio::test]
async fn initial_import_of_44_incidents() {
    let mut e = env_or_skip!();
    e.write_json(&records());
    let outcome = e.tick().await;
    assert_eq!(outcome.status, OutcomeStatus::Applied);
    assert_eq!((outcome.created, outcome.revision), (44, Some(1)));
    assert_eq!(e.counts().await, [44, 44, 0, 1, 0]);
    assert_eq!(
        e.string("SELECT status FROM import_state").await.as_deref(),
        Some("ok")
    );
    assert_eq!(
        e.opt_i64("SELECT incident_count::bigint FROM import_state").await,
        Some(44)
    );
    // Все поля исходной записи сохранены дословно.
    let raw = e
        .json(&format!("SELECT raw FROM incidents WHERE id = '{DEMO_ID}'"))
        .await;
    assert_eq!(raw, Value::Object(demo(&mut records()).clone()));
    assert_eq!(
        e.time(&format!("SELECT deadline FROM incidents WHERE id = '{DEMO_ID}'"))
            .await,
        Some(Utc.with_ymd_and_hms(2026, 11, 10, 12, 32, 14).unwrap())
    );
    assert_eq!(
        e.i64("SELECT count(*) FROM incidents WHERE waiting_exit_date IS NOT NULL")
            .await,
        0
    );
    assert_eq!(
        e.i64("SELECT count(*) FROM incidents WHERE normalized_state = 'Research'")
            .await,
        5
    );
    e.finish().await;
}

#[tokio::test]
async fn reimport_same_file_creates_nothing() {
    let mut e = env_or_skip!();
    e.write_json(&records());
    e.tick().await;
    let before = e.counts().await;
    assert_eq!(e.tick().await.status, OutcomeStatus::Unchanged); // stat не изменился
    e.write_json(&records()); // тот же снимок, новый inode
    assert_eq!(e.tick().await.status, OutcomeStatus::NoChanges);
    let mut shuffled = records();
    shuffled.reverse();
    for r in &mut shuffled {
        let header = format!("  {}  ", r["header"].as_str().unwrap());
        r.insert("header".into(), Value::String(header));
    }
    e.write_json(&shuffled); // перестановка и пробелы
    assert_eq!(e.tick().await.status, OutcomeStatus::NoChanges);
    assert_eq!(e.counts().await, before);
    assert_eq!(
        e.time("SELECT last_checked_at FROM import_state").await,
        Some(e.clock.now())
    );
    e.finish().await;
}

#[tokio::test]
async fn unchanged_file_updates_read_time_only_when_ok() {
    let mut e = env_or_skip!();
    e.write_json(&records());
    e.tick().await;
    let first_change = e.time("SELECT last_change_at FROM import_state").await;
    assert_eq!(e.tick().await.status, OutcomeStatus::Unchanged);
    assert_eq!(
        e.time("SELECT last_success_at FROM import_state").await,
        Some(e.clock.now())
    );
    assert_eq!(
        e.time("SELECT last_change_at FROM import_state").await,
        first_change
    );
    e.finish().await;
}

#[tokio::test]
async fn new_changed_and_removed() {
    let mut e = env_or_skip!();
    let recs = records();
    e.write_json(&recs);
    e.tick().await;

    let removed_id = recs[0]["id"].as_str().unwrap().to_owned();
    let mut new = recs[1].clone();
    update(
        &mut new,
        &[("id", s("INC000099999999")), ("sysId", s("999999999999999999"))],
    );
    let mut recs: Vec<Record> = recs[1..].to_vec();
    recs.push(new);
    demo(&mut recs).insert("deadline".into(), s("2026-11-25T12:00:00Z"));
    e.write_json(&recs);

    let outcome = e.tick().await;
    assert_eq!(
        (
            outcome.created,
            outcome.updated,
            outcome.removed,
            outcome.revision
        ),
        (1, 1, 1, Some(2))
    );
    let q = |sql: &str, id: &str| sql.replace(":i", &format!("'{id}'"));
    assert_eq!(
        e.i64(&q(
            "SELECT incident_revision::bigint FROM incidents WHERE id=:i",
            DEMO_ID
        ))
        .await,
        2
    );
    assert!(
        !e.boolean(&q("SELECT is_present FROM incidents WHERE id=:i", &removed_id))
            .await
    );
    assert_eq!(e.i64("SELECT count(*) FROM incidents WHERE is_present").await, 44);
    assert_eq!(
        e.strings(&q(
            "SELECT types FROM change_events WHERE incident_id=:i",
            DEMO_ID
        ))
        .await,
        ["deadline"]
    );
    assert_eq!(
        e.strings(&q(
            "SELECT fields FROM change_events WHERE incident_id=:i",
            DEMO_ID
        ))
        .await,
        ["deadline"]
    );
    assert_eq!(
        e.strings(&q(
            "SELECT types FROM change_events WHERE incident_id=:i",
            &removed_id
        ))
        .await,
        ["removed"]
    );
    // Как в Python (SQLAlchemy, none_as_null=False): None в JSONB — JSON null.
    assert_eq!(
        e.json(&q(
            r#"SELECT "current" FROM change_events WHERE incident_id=:i"#,
            &removed_id
        ))
        .await,
        Value::Null
    );
    assert_eq!(
        e.string(&q(
            r#"SELECT jsonb_typeof("current") FROM change_events WHERE incident_id=:i"#,
            &removed_id
        ))
        .await
        .as_deref(),
        Some("null")
    );
    assert_eq!(
        e.strings("SELECT types FROM change_events WHERE incident_id='INC000099999999'")
            .await,
        ["created"]
    );
    assert_eq!(
        e.json("SELECT previous FROM change_events WHERE incident_id='INC000099999999'")
            .await,
        Value::Null
    );
    // Пополевая история
    assert_eq!(
        e.json("SELECT old_value FROM incident_changes WHERE field='deadline'")
            .await,
        s("2026-11-10T12:32:14Z")
    );
    assert_eq!(
        e.json("SELECT new_value FROM incident_changes WHERE field='deadline'")
            .await,
        s("2026-11-25T12:00:00Z")
    );
    // Все события одной ревизии снимка
    assert_eq!(
        e.i64("SELECT count(DISTINCT revision) FROM change_events").await,
        1
    );
    e.finish().await;
}

#[tokio::test]
async fn removed_then_restored() {
    let mut e = env_or_skip!();
    let recs = records();
    e.write_json(&recs);
    e.tick().await;
    let without: Vec<Record> = recs.iter().filter(|r| r["id"] != DEMO_ID).cloned().collect();
    e.write_json(&without);
    e.tick().await;
    e.write_json(&recs);
    let outcome = e.tick().await;
    assert_eq!(outcome.restored, 1);
    let sql = |s: &str| s.replace(":i", &format!("'{DEMO_ID}'"));
    assert!(
        e.boolean(&sql("SELECT is_present FROM incidents WHERE id=:i"))
            .await
    );
    assert_eq!(
        e.i64(&sql(
            "SELECT incident_revision::bigint FROM incidents WHERE id=:i"
        ))
        .await,
        3
    );
    assert_eq!(
        e.strings(&sql(
            "SELECT array_agg(change_type ORDER BY id) FROM incident_changes WHERE incident_pk=\
             (SELECT pk FROM incidents WHERE id=:i)"
        ))
        .await,
        ["created", "removed", "restored"]
    );
    e.finish().await;
}

// ================================================================== ошибки источника

#[tokio::test]
async fn broken_json_keeps_last_snapshot_and_recovers() {
    let mut e = env_or_skip!();
    e.write_json(&records());
    e.tick().await;
    let before = e.counts().await;

    e.write_text(r#"[{"id": "INC1", "#); // недописанный файл
    let first = e.tick().await;
    assert_eq!(first.status, OutcomeStatus::Rejected);
    assert!(first.message.as_deref().unwrap().contains("некорректный JSON"));
    // Ниже порога — без «мигания».
    assert_eq!(
        e.string("SELECT status FROM import_state").await.as_deref(),
        Some("ok")
    );
    let second = e.tick().await;
    assert_eq!(
        (second.source_status, second.revision),
        (Some(SourceStatus::Error), Some(2))
    );
    let third = e.tick().await;
    assert_eq!(third.revision, None); // то же состояние — без новых событий и уведомлений

    assert_eq!(e.i64("SELECT count(*) FROM incidents WHERE is_present").await, 44);
    assert_eq!(
        e.i64("SELECT count(*) FROM notifications WHERE type='source.error'")
            .await,
        1
    );
    assert!(
        e.string("SELECT last_error FROM import_state")
            .await
            .unwrap()
            .starts_with("некорректный JSON")
    );
    assert_eq!(
        e.i64("SELECT consecutive_failures::bigint FROM import_state")
            .await,
        3
    );
    // Ключ и время эпизода — в формате datetime.isoformat() Python-версии.
    assert_eq!(
        e.string("SELECT dedup_key FROM notifications WHERE type='source.error'")
            .await
            .as_deref(),
        Some("source.error:2026-11-20T12:00:03+00:00")
    );

    e.write_json(&records());
    let recovered = e.tick().await;
    assert_eq!(
        (recovered.status, recovered.revision),
        (OutcomeStatus::NoChanges, Some(3))
    );
    assert_eq!(
        e.string("SELECT status FROM import_state").await.as_deref(),
        Some("ok")
    );
    assert_eq!(
        e.i64("SELECT count(*) FROM notifications WHERE type='source.recovered'")
            .await,
        1
    );
    assert_eq!(
        e.json("SELECT payload FROM notifications WHERE type='source.recovered'")
            .await,
        serde_json::json!({"errorSince": "2026-11-20T12:00:03+00:00"})
    );
    assert_eq!(
        e.string("SELECT reason FROM dashboard_events WHERE id = 3")
            .await
            .as_deref(),
        Some("source")
    );
    let after = e.counts().await;
    assert_eq!((after[0], after[2]), (before[0], before[2]));
    e.finish().await;
}

async fn invalid_snapshot_rejected_entirely(mutate: impl FnOnce(&mut Vec<Record>), fragment: &str) {
    let Some(mut e) = Env::new().await else { return };
    e.write_json(&records());
    e.tick().await;
    let mut bad = records();
    // Корректное изменение в том же файле тоже не применяется.
    demo(&mut bad).insert("priorityId".into(), Value::from(4));
    mutate(&mut bad);
    e.write_json(&bad);
    let outcome = e.tick().await;
    assert_eq!(outcome.status, OutcomeStatus::Rejected);
    let message = outcome.message.unwrap();
    assert!(message.contains(fragment), "{message:?} !~ {fragment:?}");
    assert_eq!(
        e.i64(&format!(
            "SELECT priority_id::bigint FROM incidents WHERE id='{DEMO_ID}'"
        ))
        .await,
        3
    );
    assert_eq!(e.i64("SELECT count(*) FROM change_events").await, 0);
    e.finish().await;
}

#[tokio::test]
async fn invalid_snapshot_missing_submit_date() {
    invalid_snapshot_rejected_entirely(|r| drop(r[3].shift_remove("submitDate")), "submitDate").await;
}

#[tokio::test]
async fn invalid_snapshot_missing_priority() {
    invalid_snapshot_rejected_entirely(|r| drop(r[3].shift_remove("priorityId")), "priorityId").await;
}

#[tokio::test]
async fn invalid_snapshot_bad_date() {
    invalid_snapshot_rejected_entirely(
        |r| update(&mut r[3], &[("deadline", s("25.11.2026"))]),
        "deadline: нераспознаваемая дата",
    )
    .await;
}

#[tokio::test]
async fn invalid_snapshot_duplicate_id() {
    invalid_snapshot_rejected_entirely(
        |r| {
            let id = r[2]["id"].clone();
            r[3].insert("id".into(), id);
        },
        "повторяется",
    )
    .await;
}

#[tokio::test]
async fn missing_file_before_first_import() {
    let mut e = env_or_skip!();
    assert_eq!(e.tick().await.status, OutcomeStatus::ReadFailed);
    assert_eq!(e.tick().await.source_status, Some(SourceStatus::Error));
    assert_eq!(e.time("SELECT last_success_at FROM import_state").await, None);
    assert_eq!(
        e.string("SELECT last_error FROM import_state").await.as_deref(),
        Some("Файл источника не найден: character_problems.json")
    );
    e.write_json(&records());
    let outcome = e.tick().await;
    assert_eq!(outcome.created, 44); // первичная загрузка после восстановления
    assert_eq!(e.i64("SELECT count(*) FROM change_events").await, 0);
    e.finish().await;
}

#[tokio::test]
async fn unknown_status_goes_to_warnings() {
    let mut e = env_or_skip!();
    let mut recs = records();
    update(
        demo(&mut recs),
        &[("stateEnum", s("Suspended")), ("stateId", Value::from(99))],
    );
    e.write_json(&recs);
    assert_eq!(e.tick().await.status, OutcomeStatus::Applied);
    assert_eq!(
        e.string(&format!(
            "SELECT normalized_state FROM incidents WHERE id='{DEMO_ID}'"
        ))
        .await
        .as_deref(),
        Some("Unknown")
    );
    let warnings = e.json("SELECT warnings FROM import_state").await;
    let warnings = warnings.as_array().unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].as_str().unwrap().contains(DEMO_ID));
    e.finish().await;
}

#[tokio::test]
async fn empty_array_removes_everything_but_keeps_history() {
    let mut e = env_or_skip!();
    e.write_json(&records());
    e.tick().await;
    e.write_json(&[]);
    let outcome = e.tick().await;
    assert_eq!(outcome.removed, 44);
    assert_eq!(e.i64("SELECT count(*) FROM incidents").await, 44);
    assert_eq!(e.i64("SELECT count(*) FROM incidents WHERE is_present").await, 0);
    e.finish().await;
}

#[tokio::test]
async fn empty_initial_snapshot_still_publishes_revision() {
    let mut e = env_or_skip!();
    e.write_json(&[]);
    let outcome = e.tick().await;
    assert_eq!(
        (outcome.status, outcome.revision),
        (OutcomeStatus::NoChanges, Some(1))
    );
    assert_eq!(
        e.json("SELECT payload FROM dashboard_events WHERE id = 1").await,
        serde_json::json!({"schemaVersion": 1, "initial": true, "created": 0, "updated": 0,
                           "removed": 0, "restored": 0})
    );
    e.finish().await;
}

/// Ошибка БД внутри транзакции импорта (значение вне диапазона smallint) — откат, реестр цел,
/// ошибка записана как в Python: «Ошибка сохранения снимка: DBAPIError».
#[tokio::test]
async fn database_error_is_recorded_as_failure() {
    let mut e = env_or_skip!();
    e.write_json(&records());
    e.tick().await;
    let mut recs = records();
    update(demo(&mut recs), &[("urgencyId", Value::from(40_000))]);
    e.write_json(&recs);
    let outcome = e.tick().await;
    assert_eq!(outcome.status, OutcomeStatus::Rejected);
    assert_eq!(
        outcome.message.as_deref(),
        Some("Ошибка сохранения снимка: DBAPIError")
    );
    assert_eq!(e.i64("SELECT count(*) FROM change_events").await, 0);
    assert_eq!(e.i64("SELECT count(*) FROM dashboard_events").await, 1);
    // Файл перечитывается снова (отпечаток сброшен) — ошибка повторяется до порога.
    assert_eq!(e.tick().await.source_status, Some(SourceStatus::Error));
    e.finish().await;
}

#[tokio::test]
async fn advisory_lock_allows_single_importer() {
    let e = env_or_skip!();
    let options = dashboard_store::connect_options(&e.settings).unwrap();
    let mut a: PgConnection = options.connect().await.unwrap();
    let mut b: PgConnection = options.connect().await.unwrap();
    let lock = "SELECT pg_try_advisory_lock($1)";
    assert!(
        sqlx::query_scalar::<_, bool>(lock)
            .bind(IMPORTER_LOCK_KEY)
            .fetch_one(&mut a)
            .await
            .unwrap()
    );
    assert!(
        !sqlx::query_scalar::<_, bool>(lock)
            .bind(IMPORTER_LOCK_KEY)
            .fetch_one(&mut b)
            .await
            .unwrap()
    );

    // acquire_lock ждёт, пока блокировка занята, и выходит по сигналу остановки.
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let settings = e.settings.clone();
    let waiter = tokio::spawn(async move {
        dashboard_importer::worker::acquire_lock(&settings, &mut rx)
            .await
            .is_some()
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    tx.send(true).unwrap();
    assert!(!waiter.await.unwrap());

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(IMPORTER_LOCK_KEY)
        .execute(&mut a)
        .await
        .unwrap();
    let (_tx, mut rx) = tokio::sync::watch::channel(false);
    let held = dashboard_importer::worker::acquire_lock(&e.settings, &mut rx).await;
    assert!(held.is_some());
    drop((a, b, held));
    e.finish().await;
}

/// Полный процесс: `run_importer` держит блокировку, опрашивает файл и останавливается по сигналу.
#[tokio::test]
async fn run_importer_polls_and_stops() {
    let Some(e) = Env::with_vars(&[("IMPORT_POLL_INTERVAL_SECONDS", "0.1")]).await else {
        return;
    };
    e.write_json(&records());
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(dashboard_importer::run_importer(e.settings.clone(), rx));
    let mut imported = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if e.i64("SELECT count(*) FROM incidents").await == 44 {
            imported = true;
            break;
        }
    }
    assert!(imported);
    let mut recs = records();
    demo(&mut recs).insert("priorityId".into(), Value::from(4));
    e.write_json(&recs);
    let mut raised = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if e.i64("SELECT count(*) FROM notifications").await == 1 {
            raised = true;
            break;
        }
    }
    assert!(raised);
    tx.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("importer stops")
        .unwrap()
        .unwrap();
    // Блокировка освобождена.
    let options = dashboard_store::connect_options(&e.settings).unwrap();
    let mut c: PgConnection = options.connect().await.unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(IMPORTER_LOCK_KEY)
            .fetch_one(&mut c)
            .await
            .unwrap()
    );
    drop(c);
    e.finish().await;
}
