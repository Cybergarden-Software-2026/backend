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
    "import_state",
    "incident_changes",
    "incidents",
    "mass_problem_workspace",
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
    assert_eq!(report.applied, vec![1, 2, 3, 4, 5, 6]);
    assert!(report.adopted_from_alembic.is_empty());
    assert_eq!(report.alembic_revision.as_deref(), Some("0006"));

    assert_eq!(
        db.tables().await,
        with_service_tables(&["_sqlx_migrations", "alembic_version"])
    );
    // Установлена последняя ревизия схемы.
    assert_eq!(
        db.scalar_strings("SELECT version_num::text FROM alembic_version")
            .await,
        vec!["0006"]
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
    assert_eq!(again.alembic_revision.as_deref(), Some("0006"));

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
    assert_eq!(report.applied, vec![5, 6]);
    assert!(report.adopted_from_alembic.is_empty());
    assert!(report.reverted_by_alembic.is_empty());
    assert_eq!(report.alembic_revision.as_deref(), Some("0006"));
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
    assert_eq!(report.applied, vec![6]);
    assert_eq!(report.alembic_revision.as_deref(), Some("0006"));
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
async fn database_created_by_alembic_is_adopted_without_running_sql() {
    let Some(db) = TempDb::create().await else { return };
    db.simulate_alembic().await;
    // Метка данных: если бы SQL выполнился повторно, CREATE TABLE упал бы, а строка бы не уцелела.
    sqlx::query("UPDATE import_state SET status = 'ok' WHERE id = 1")
        .execute(&db.pool)
        .await
        .unwrap();

    let report = migrate::run(&db.pool).await.unwrap();
    assert_eq!(report.applied, vec![2, 3, 4, 5, 6]);
    assert_eq!(report.adopted_from_alembic, vec![1]);
    assert_eq!(report.alembic_revision.as_deref(), Some("0006"));
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
    assert_eq!(applied, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(
        db.scalar_strings("SELECT version_num::text FROM alembic_version")
            .await,
        vec!["0006"]
    );
    db.drop().await;
}

/// `alembic downgrade base` в Python-версии: down-SQL выполнен, `alembic_version` пуста,
/// а `_sqlx_migrations` по-прежнему помнит версию 1.
#[tokio::test]
async fn alembic_downgrade_is_respected() {
    let Some(db) = TempDb::create().await else { return };
    migrate::run(&db.pool).await.unwrap();
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
    assert_eq!(report.reverted_by_alembic, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(report.applied, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(
        db.tables().await,
        with_service_tables(&["_sqlx_migrations", "alembic_version"])
    );
    assert_eq!(
        db.scalar_strings("SELECT version_num::text FROM alembic_version")
            .await,
        vec!["0006"]
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
