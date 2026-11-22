//! Миграции на настоящем PostgreSQL. Нужен `TEST_DATABASE_URL` (как у Python-тестов);
//! каждый тест создаёт и удаляет свою временную базу, саму `TEST_DATABASE_URL` не трогает.
//! Без переменной тесты пропускаются.

use dashboard_config::Settings;
use dashboard_store::migrate::{self, MIGRATIONS, MigrateError};
use sqlx::migrate::Migrator;
use sqlx::{Connection, PgConnection, PgPool};

struct TempDb {
    admin_url: String,
    name: String,
    pool: PgPool,
}

impl TempDb {
    async fn create() -> Option<Self> {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skip: TEST_DATABASE_URL is not set");
            return None;
        };
        let admin_url = dashboard_config::normalize_database_url(&url);
        let name = format!(
            "rust_migrate_{}",
            std::process::id() as u64 * 1000 + rand_suffix()
        );
        let mut admin = PgConnection::connect(&admin_url).await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&mut admin)
            .await
            .unwrap();
        let db_url = replace_db_name(&admin_url, &name);
        let settings = Settings::from_vars([("DATABASE_URL", db_url.as_str())]).unwrap();
        let pool = dashboard_store::connect(&settings).await.unwrap();
        Some(Self {
            admin_url,
            name,
            pool,
        })
    }

    async fn drop(self) {
        self.pool.close().await;
        let mut admin = PgConnection::connect(&self.admin_url).await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        )))
        .execute(&mut admin)
        .await
        .unwrap();
    }

    async fn scalar_strings(&self, sql: &'static str) -> Vec<String> {
        sqlx::query_scalar(sql).fetch_all(&self.pool).await.unwrap()
    }

    async fn tables(&self) -> Vec<String> {
        self.scalar_strings(
            "SELECT tablename::text FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename",
        )
        .await
    }

    /// Состояние базы после `alembic upgrade head` в Python-версии: SQL выполнен,
    /// в `alembic_version` — ревизия, `_sqlx_migrations` нет.
    async fn simulate_alembic(&self) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(MIGRATIONS[0].sql))
            .execute(&self.pool)
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE alembic_version (version_num VARCHAR(32) NOT NULL, \
             CONSTRAINT alembic_version_pkc PRIMARY KEY (version_num)); \
             INSERT INTO alembic_version VALUES ('0001');",
        )
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

async fn rollback_to_mobile_bell(pool: &PgPool) {
    rollback_to_team_contact_events(pool).await;
    sqlx::raw_sql(include_str!(
        "../../../migrations/0011_team_contact_events_down.sql"
    ))
    .execute(pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0010_team_phones_down.sql"))
        .execute(pool)
        .await
        .unwrap();
}

async fn rollback_to_team_contact_events(pool: &PgPool) {
    sqlx::raw_sql(include_str!(
        "../../../migrations/0013_coordination_claims_down.sql"
    ))
    .execute(pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0012_incident_forms_down.sql"))
        .execute(pool)
        .await
        .unwrap();
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

fn replace_db_name(url: &str, name: &str) -> String {
    let (base, query) = url.split_once('?').map_or((url, None), |(b, q)| (b, Some(q)));
    let base = &base[..base.rfind('/').unwrap()];
    match query {
        Some(q) => format!("{base}/{name}?{q}"),
        None => format!("{base}/{name}"),
    }
}

const SCHEMA_TABLES: &[&str] = &[
    "analysis_jobs",
    "assignment_overrides",
    "coordinator_accounts",
    "coordinator_sessions",
    "teams",
    "change_events",
    "dashboard_events",
    "form_incidents",
    "forms_import_state",
    "import_state",
    "incident_changes",
    "incident_coordination_claims",
    "incidents",
    "mass_problem_workspace",
    "mobile_bell_state",
    "mobile_pairings",
    "mobile_devices",
    "mobile_push_deliveries",
    "notification_reads",
    "notifications",
];

fn with_service_tables(extra: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = SCHEMA_TABLES.iter().chain(extra).map(|s| s.to_string()).collect();
    v.sort();
    v
}

#[tokio::test]
async fn empty_database_is_migrated_and_marked_for_alembic() {
    let Some(db) = TempDb::create().await else { return };

    let report = migrate::run(&db.pool).await.unwrap();
    assert_eq!(report.applied, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    assert!(report.adopted_from_alembic.is_empty());
    assert_eq!(report.alembic_revision.as_deref(), Some("0013"));

    assert_eq!(
        db.tables().await,
        with_service_tables(&["_sqlx_migrations", "alembic_version"])
    );
    // Установлена последняя ревизия схемы.
    assert_eq!(
        db.scalar_strings("SELECT version_num::text FROM alembic_version")
            .await,
        vec!["0013"]
    );
    // Сид из SQL на месте.
    assert_eq!(
        db.scalar_strings("SELECT status FROM import_state WHERE id = 1")
            .await,
        vec!["initializing"]
    );

    // Повторный запуск — без изменений.
    let again = migrate::run(&db.pool).await.unwrap();
    assert!(again.applied.is_empty() && again.adopted_from_alembic.is_empty());
    assert_eq!(again.alembic_revision.as_deref(), Some("0013"));

    db.drop().await;
}

#[tokio::test]
async fn coordinator_sessions_upgrade_preserves_existing_cluster_reviews() {
    let Some(db) = TempDb::create().await else { return };

    let previous = Migrator::with_migrations(
        migrate::migrator()
            .migrations
            .iter()
            .filter(|migration| migration.version <= 4)
            .cloned()
            .collect(),
    );
    previous.run(&db.pool).await.unwrap();
    sqlx::raw_sql(
        "CREATE TABLE alembic_version (version_num VARCHAR(32) NOT NULL PRIMARY KEY); \
         INSERT INTO alembic_version VALUES ('0004'); \
         WITH seed AS ( \
             INSERT INTO incidents (id, sys_id, normalized_state, priority_id, submit_date, raw, content_hash) \
             VALUES ('INC-SEED', 'sys-seed', 'Assigned', 1, now(), '{}', 'cluster-seed') \
             RETURNING pk, id, sys_id, incident_revision \
         ) \
         INSERT INTO analysis_jobs (incident_pk, incident_id, sys_id, kind, incident_revision, input, model, prompt_version) \
         SELECT pk, id, sys_id, 'cluster', incident_revision, '{}', 'qwen-local', 'v-test' FROM seed;",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let report = migrate::run(&db.pool).await.unwrap();
    assert_eq!(report.applied, vec![5, 6, 7, 8, 9, 10, 11, 12, 13]);
    assert!(report.adopted_from_alembic.is_empty());
    assert!(report.reverted_by_alembic.is_empty());
    assert_eq!(report.alembic_revision.as_deref(), Some("0013"));
    assert_eq!(
        db.scalar_strings("SELECT kind || ':' || model FROM analysis_jobs")
            .await,
        vec!["cluster:qwen-local"]
    );
    assert!(
        db.tables()
            .await
            .iter()
            .any(|table| table == "coordinator_sessions")
    );

    db.drop().await;
}

#[tokio::test]
async fn coordinator_accounts_upgrade_keeps_legacy_sessions_unlinked() {
    let Some(db) = TempDb::create().await else { return };
    let previous = Migrator::with_migrations(
        migrate::migrator()
            .migrations
            .iter()
            .filter(|migration| migration.version <= 5)
            .cloned()
            .collect(),
    );
    previous.run(&db.pool).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO coordinator_sessions (token_hash, expires_at) \
         VALUES (decode(repeat('aa', 32), 'hex'), now() + interval '1 hour');",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let report = migrate::run(&db.pool).await.unwrap();
    assert_eq!(report.applied, vec![6, 7, 8, 9, 10, 11, 12, 13]);
    assert_eq!(report.alembic_revision.as_deref(), Some("0013"));
    assert_eq!(
        db.scalar_strings(
            "SELECT encode(token_hash, 'hex') FROM coordinator_sessions WHERE account_login IS NULL"
        )
        .await,
        vec!["aa".repeat(32)]
    );
    db.drop().await;
}

#[tokio::test]
async fn mobile_session_migration_preserves_legacy_push_registration_in_both_directions() {
    let Some(db) = TempDb::create().await else { return };
    let previous = Migrator::with_migrations(
        migrate::migrator()
            .migrations
            .iter()
            .filter(|migration| migration.version <= 7)
            .cloned()
            .collect(),
    );
    previous.run(&db.pool).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO coordinator_accounts (login, display_name, password_hash, key_hash) \
         VALUES ('alice', 'Alice', 'fixture', decode(repeat('aa', 32), 'hex')); \
         INSERT INTO mobile_devices (account_login, platform, fcm_token, fcm_target_type, secret_hash) \
         VALUES ('alice', 'android', 'legacy-firebase-id', 'fid', decode(repeat('bb', 32), 'hex'));",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    const ALL_DEVICES: &str = "SELECT to_jsonb(d)::text FROM mobile_devices d";
    let legacy = db.scalar_strings(ALL_DEVICES).await;

    let report = migrate::run(&db.pool).await.unwrap();
    assert_eq!(report.applied, vec![8, 9, 10, 11, 12, 13]);
    assert_eq!(
        db.scalar_strings(
            "SELECT (to_jsonb(d) - 'client_device_id' - 'fcm_registered_at')::text FROM mobile_devices d \
             WHERE client_device_id IS NULL",
        )
        .await,
        legacy
    );

    sqlx::query(
        "INSERT INTO mobile_devices (account_login, platform, fcm_token, secret_hash, client_device_id) \
         VALUES ('alice', 'android', 'session-fcm-token', decode(repeat('cc', 32), 'hex'), 'app-install-1')",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    rollback_to_mobile_bell(&db.pool).await;
    sqlx::raw_sql(include_str!("../../../migrations/0009_mobile_bell_down.sql"))
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0008_mobile_sessions_down.sql"))
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.scalar_strings(ALL_DEVICES).await, legacy);
    db.drop().await;
}

#[tokio::test]
async fn bell_upgrade_preserves_queued_pushes_and_sends_new_events_only_to_existing_registrations() {
    let Some(db) = TempDb::create().await else { return };
    Migrator::with_migrations(
        migrate::migrator()
            .migrations
            .iter()
            .filter(|migration| migration.version <= 8)
            .cloned()
            .collect(),
    )
    .run(&db.pool)
    .await
    .unwrap();
    sqlx::raw_sql(
        "INSERT INTO incidents (id, sys_id, normalized_state, priority_id, submit_date, raw, content_hash)
         VALUES ('INC1', 'S1', 'Assigned', 4, now(), '{}', 'seed');
         INSERT INTO coordinator_accounts (login, display_name, password_hash, key_hash)
         VALUES ('alice', 'Alice', 'fixture', decode(repeat('aa', 32), 'hex'));
         INSERT INTO mobile_devices (account_login, platform, fcm_token, secret_hash)
         VALUES ('alice', 'android', 'phone', sha256('phone')),
                ('alice', 'web', 'web', sha256('web')),
                ('alice', 'android', NULL, sha256('late'));
         INSERT INTO notifications (type, incident_id, sys_id, title, dedup_key)
         VALUES ('critical.raised', 'INC1', 'S1', 'Critical', 'before-upgrade');
         UPDATE mobile_push_deliveries SET attempts = 2, available_at = now() + interval '2 minutes';",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    const QUEUE: &str = "SELECT to_jsonb(q)::text FROM mobile_push_deliveries q ORDER BY device_id";
    let pending = db.scalar_strings(QUEUE).await;
    assert_eq!(pending.len(), 2);

    let report = migrate::run(&db.pool).await.unwrap();
    assert_eq!(report.applied, vec![9, 10, 11, 12, 13]);
    assert_eq!(
        db.scalar_strings(
            "SELECT 'baseline' FROM mobile_bell_state WHERE NOT initialized AND next_check_at IS NOT NULL"
        )
        .await,
        ["baseline"]
    );
    let event_time: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    // Token rotation retains eligibility for a delayed tick. First registration after the tick does not.
    sqlx::raw_sql(
        "UPDATE mobile_devices SET fcm_token = 'rotated' WHERE fcm_token = 'phone';
         UPDATE mobile_devices SET fcm_token = 'late' WHERE fcm_token IS NULL;",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query("SELECT enqueue_mobile_bell('waiting', 'INC1', 'S1', $1)")
        .bind(event_time)
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        db.scalar_strings(
            "SELECT d.fcm_token FROM mobile_push_deliveries q JOIN mobile_devices d ON d.id = q.device_id
             WHERE q.bell_kind = 'waiting' AND q.bell_incident_id = 'INC1' AND q.bell_sys_id = 'S1'
               AND q.journal_notification_id IS NULL ORDER BY d.fcm_token",
        )
        .await,
        ["rotated", "web"]
    );
    let event_ids = db
        .scalar_strings("SELECT DISTINCT notification_id::text FROM mobile_push_deliveries")
        .await;
    assert_eq!(event_ids.len(), 2); // one journal ID and one shared bell ID

    rollback_to_mobile_bell(&db.pool).await;
    sqlx::raw_sql(include_str!("../../../migrations/0009_mobile_bell_down.sql"))
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.scalar_strings(QUEUE).await, pending);
    sqlx::raw_sql(
        "DELETE FROM notifications;
         INSERT INTO notifications (type, incident_id, sys_id, title, dedup_key)
         VALUES ('critical.raised', 'INC2', 'S2', 'Critical', 'after-rollback');",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(db.scalar_strings(QUEUE).await.len(), 3);
    db.drop().await;
}

#[tokio::test]
async fn old_importer_writes_during_upgrade_reseed_timers_after_changes_and_restore() {
    let Some(db) = TempDb::create().await else { return };
    migrate::run(&db.pool).await.unwrap();
    const BASELINES: &str = "SELECT i.id FROM mobile_bell_state b JOIN incidents i ON i.pk = b.incident_pk
        WHERE NOT b.initialized AND b.kind IS NULL AND b.next_check_at IS NOT NULL";
    sqlx::query(
        "INSERT INTO incidents (id, sys_id, normalized_state, priority_id, submit_date, raw, content_hash)
         VALUES ('INC1', 'S1', 'Assigned', 4, now(), '{}', 'seed')",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(db.scalar_strings(BASELINES).await, ["INC1"]);

    // The current importer updates its own signatures. Its transaction-local
    // marker must not suppress the hook for a later write from an older binary.
    sqlx::raw_sql(
        "BEGIN;
         SET LOCAL underctrl.mobile_bell_writer = '1';
         UPDATE mobile_bell_state SET initialized = true, kind = 'critical', next_check_at = NULL;
         UPDATE incidents SET incident_revision = incident_revision + 1;
         COMMIT;",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        db.scalar_strings("SELECT kind FROM mobile_bell_state WHERE initialized")
            .await,
        ["critical"]
    );
    sqlx::query("UPDATE incidents SET priority_id = 2, incident_revision = incident_revision + 1")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.scalar_strings(BASELINES).await, ["INC1"]);
    sqlx::query("UPDATE incidents SET is_present = false, incident_revision = incident_revision + 1")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        db.scalar_strings("SELECT incident_pk::text FROM mobile_bell_state")
            .await
            .is_empty()
    );
    sqlx::query("UPDATE incidents SET is_present = true, incident_revision = incident_revision + 1")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.scalar_strings(BASELINES).await, ["INC1"]);
    db.drop().await;
}

#[tokio::test]
async fn database_created_by_alembic_is_adopted_without_running_sql() {
    let Some(db) = TempDb::create().await else { return };
    db.simulate_alembic().await;
    // Метка данных: если бы SQL выполнился повторно, CREATE TABLE упал бы, а строка бы не уцелела.
    sqlx::query("UPDATE import_state SET status = 'ok' WHERE id = 1")
        .execute(&db.pool)
        .await
        .unwrap();

    let report = migrate::run(&db.pool).await.unwrap();
    assert_eq!(report.applied, vec![2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    assert_eq!(report.adopted_from_alembic, vec![1]);
    assert_eq!(report.alembic_revision.as_deref(), Some("0013"));
    assert_eq!(
        db.scalar_strings("SELECT status FROM import_state WHERE id = 1")
            .await,
        vec!["ok"]
    );

    // Контрольная сумма записана как у обычного применения: следующий запуск её проверяет.
    let checksum: Vec<u8> = sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 1")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(checksum, migrate::migrator().migrations[0].checksum.to_vec());
    let again = migrate::run(&db.pool).await.unwrap();
    assert!(again.applied.is_empty() && again.adopted_from_alembic.is_empty());

    db.drop().await;
}

#[tokio::test]
async fn unknown_alembic_revision_is_refused() {
    let Some(db) = TempDb::create().await else { return };
    sqlx::raw_sql(
        "CREATE TABLE alembic_version (version_num VARCHAR(32) NOT NULL PRIMARY KEY); \
         INSERT INTO alembic_version VALUES ('9999');",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let err = migrate::run(&db.pool).await.unwrap_err();
    assert!(
        matches!(err, MigrateError::UnknownAlembicRevision(ref r) if r == "9999"),
        "{err}"
    );
    // Ничего не создано.
    assert_eq!(db.tables().await, vec!["alembic_version"]);

    db.drop().await;
}

#[tokio::test]
async fn concurrent_runs_apply_once() {
    let Some(db) = TempDb::create().await else { return };
    let (a, b, c) = tokio::join!(
        migrate::run(&db.pool),
        migrate::run(&db.pool),
        migrate::run(&db.pool)
    );
    let reports = [a.unwrap(), b.unwrap(), c.unwrap()];
    let applied: Vec<i64> = reports.iter().flat_map(|r| r.applied.clone()).collect();
    assert_eq!(applied, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    assert_eq!(
        db.scalar_strings("SELECT version_num::text FROM alembic_version")
            .await,
        vec!["0013"]
    );
    db.drop().await;
}

#[tokio::test]
async fn team_contact_audit_and_retry_key_survive_downgrade_and_reapply() {
    let Some(db) = TempDb::create().await else { return };
    migrate::run(&db.pool).await.unwrap();
    sqlx::raw_sql(
        r#"WITH incident AS (
               INSERT INTO incidents (id, sys_id, normalized_state, priority_id, submit_date, raw, content_hash)
               VALUES ('INC1', 'sys-1', 'Assigned', 1, now(), '{}', 'seed') RETURNING pk
           ), event AS (
               INSERT INTO dashboard_events (event_type, reason, payload)
               VALUES ('dashboard.changed', 'team_contacted',
                   '{"sysId":"sys-1","contact":{"team":"Team A","actorLogin":"alice","requestId":"f2f138a6-6482-44a5-a5d0-3989c05bc9a4"}}')
               RETURNING id
           )
           INSERT INTO change_events (revision, incident_pk, incident_revision, sys_id, incident_id, types)
           SELECT event.id, incident.pk, 2, 'sys-1', 'INC1', ARRAY['team_contacted'] FROM event, incident"#,
    )
    .execute(&db.pool)
    .await
    .unwrap();
    rollback_to_team_contact_events(&db.pool).await;
    sqlx::raw_sql(include_str!(
        "../../../migrations/0011_team_contact_events_down.sql"
    ))
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE alembic_version SET version_num = '0010'")
        .execute(&db.pool)
        .await
        .unwrap();
    const CONTACT: &str = "SELECT d.reason || ':' || (d.payload->'contact'->>'team')
        FROM change_events c JOIN dashboard_events d ON d.id = c.revision
        WHERE 'team_contacted' = ANY(c.types)";
    assert_eq!(db.scalar_strings(CONTACT).await, ["source:Team A"]);
    let report = migrate::run(&db.pool).await.unwrap();
    assert_eq!(report.applied, vec![11, 12, 13]);
    assert_eq!(db.scalar_strings(CONTACT).await, ["team_contacted:Team A"]);
    let duplicate = sqlx::query(
        "INSERT INTO dashboard_events (event_type, reason, payload)
         SELECT event_type, reason, payload FROM dashboard_events WHERE reason = 'team_contacted'",
    )
    .execute(&db.pool)
    .await
    .unwrap_err();
    assert!(duplicate.as_database_error().unwrap().is_unique_violation());
    db.drop().await;
}

/// `alembic downgrade base` в Python-версии: down-SQL выполнен, `alembic_version` пуста,
/// а `_sqlx_migrations` по-прежнему помнит версию 1.
#[tokio::test]
async fn alembic_downgrade_is_respected() {
    let Some(db) = TempDb::create().await else { return };
    migrate::run(&db.pool).await.unwrap();
    rollback_to_mobile_bell(&db.pool).await;
    sqlx::raw_sql(include_str!("../../../migrations/0009_mobile_bell_down.sql"))
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0008_mobile_sessions_down.sql"))
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0007_mobile_push_down.sql"))
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../migrations/0006_coordinator_accounts_down.sql"
    ))
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../migrations/0005_coordinator_sessions_down.sql"
    ))
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0004_cluster_reviews_down.sql"))
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0003_mass_problems_down.sql"))
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0002_assignments_down.sql"))
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0001_initial_down.sql"))
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM alembic_version")
        .execute(&db.pool)
        .await
        .unwrap();

    let report = migrate::run(&db.pool).await.unwrap();
    assert_eq!(
        report.reverted_by_alembic,
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]
    );
    assert_eq!(report.applied, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    assert_eq!(
        db.tables().await,
        with_service_tables(&["_sqlx_migrations", "alembic_version"])
    );
    assert_eq!(
        db.scalar_strings("SELECT version_num::text FROM alembic_version")
            .await,
        vec!["0013"]
    );
    db.drop().await;
}

#[tokio::test]
async fn modified_sql_is_detected() {
    let Some(db) = TempDb::create().await else { return };
    migrate::run(&db.pool).await.unwrap();
    // Имитация правки уже применённого SQL-файла.
    sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = 1")
        .execute(&db.pool)
        .await
        .unwrap();
    let err = migrate::run(&db.pool).await.unwrap_err();
    assert!(
        matches!(
            err,
            MigrateError::Migrate(sqlx::migrate::MigrateError::VersionMismatch(1))
        ),
        "{err}"
    );
    db.drop().await;
}
