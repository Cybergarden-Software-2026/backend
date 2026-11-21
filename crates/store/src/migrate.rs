//! Миграции схемы (SQL из `migrations/`, тот же, что был у Python-версии).
//!
//! SQL не копируется: файлы `migrations/*_up.sql` встраиваются в бинарник через
//! `include_str!`. Имена файлов (`0001_initial_up.sql` / `0001_initial_down.sql`) не подходят под
//! соглашение `sqlx::migrate!` (оно приняло бы оба файла за две миграции версии 1 и применило бы
//! down как обычную), поэтому `Migrator` собирается вручную из списка [`MIGRATIONS`].
//!
//! Одна база обслуживается любой из версий, поэтому журналы синхронизируются в обе стороны:
//! * база создана Python (`alembic upgrade head`): в `alembic_version` есть ревизия, а в
//!   `_sqlx_migrations` нет — соответствующие миграции помечаются применёнными без выполнения SQL
//!   (`Migrator::skip`, с записью контрольной суммы);
//! * база создана Rust: после применения в `alembic_version` записывается та же ревизия, и
//!   `alembic upgrade head` в Python-версии ничего не делает;
//! * схему откатили `alembic downgrade`: откатывать умеет только alembic, поэтому при наличии
//!   таблицы `alembic_version` главная она — записи `_sqlx_migrations` новее её ревизии удаляются,
//!   и миграции применяются заново.
//!
//! Ревизия alembic, неизвестная Rust-версии (Python ушёл вперёд), — ошибка: схему нельзя
//! считать совместимой, пока миграция не добавлена в [`MIGRATIONS`].

use std::borrow::Cow;

use sqlx::migrate::{Migration, MigrationType, Migrator};
use sqlx::{PgConnection, PgPool};

/// Миграция, общая для обеих версий.
#[derive(Debug, Clone, Copy)]
pub struct SharedMigration {
    /// Версия в `_sqlx_migrations`.
    pub version: i64,
    /// Ревизия alembic Python-версии (значение в `alembic_version`, для совместимости баз).
    pub alembic_revision: &'static str,
    pub description: &'static str,
    pub sql: &'static str,
}

/// Порядок важен: по возрастанию версии; ревизии alembic идут цепочкой в том же порядке.
pub const MIGRATIONS: &[SharedMigration] = &[
    SharedMigration {
        version: 1,
        alembic_revision: "0001",
        description: "initial",
        sql: include_str!("../../../migrations/0001_initial_up.sql"),
    },
    SharedMigration {
        version: 2,
        alembic_revision: "0002",
        description: "dashboard assignments",
        sql: include_str!("../../../migrations/0002_assignments_up.sql"),
    },
    SharedMigration {
        version: 3,
        alembic_revision: "0003",
        description: "shared mass problems workspace",
        sql: include_str!("../../../migrations/0003_mass_problems_up.sql"),
    },
    SharedMigration {
        version: 4,
        alembic_revision: "0004",
        description: "mass problem reviews in analysis_jobs",
        sql: include_str!("../../../migrations/0004_cluster_reviews_up.sql"),
    },
];

/// Ключ advisory lock миграций («MTS» + 2), чтобы два процесса `migrate` не синхронизировали
/// журналы одновременно. Импортёр использует `0x4D545301`.
pub const MIGRATE_LOCK_KEY: i64 = 0x4D54_5302;

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(
        "alembic_version = {0:?} is unknown to the Rust version: the schema was migrated by a newer \
         Python version; add the migration to dashboard_store::migrate::MIGRATIONS"
    )]
    UnknownAlembicRevision(String),
    #[error("alembic_version has several heads {0:?}; expected one")]
    SeveralAlembicHeads(Vec<String>),
}

/// Что сделал запуск миграций.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MigrationReport {
    /// Миграции, SQL которых выполнен сейчас.
    pub applied: Vec<i64>,
    /// Миграции, уже применённые alembic и только отмеченные в `_sqlx_migrations`.
    pub adopted_from_alembic: Vec<i64>,
    /// Миграции, откаченные `alembic downgrade`: убраны из `_sqlx_migrations` перед применением.
    pub reverted_by_alembic: Vec<i64>,
    /// Ревизия в `alembic_version` после запуска.
    pub alembic_revision: Option<String>,
}

pub fn migrator() -> Migrator {
    Migrator::with_migrations(
        MIGRATIONS
            .iter()
            .map(|m| {
                Migration::new(
                    m.version,
                    Cow::Borrowed(m.description),
                    MigrationType::Simple,
                    sqlx::SqlSafeStr::into_sql_str(m.sql),
                    false,
                )
            })
            .collect(),
    )
}

/// Применяет недостающие миграции и синхронизирует `alembic_version` ↔ `_sqlx_migrations`.
pub async fn run(pool: &PgPool) -> Result<MigrationReport, MigrateError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MIGRATE_LOCK_KEY)
        .execute(&mut *conn)
        .await?;
    let result = run_locked(&mut conn).await;
    let unlock = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATE_LOCK_KEY)
        .execute(&mut *conn)
        .await;
    let report = result?;
    unlock?;
    Ok(report)
}

async fn run_locked(conn: &mut PgConnection) -> Result<MigrationReport, MigrateError> {
    let migrator = migrator();
    let mut report = MigrationReport::default();

    let alembic_heads = alembic_heads(conn).await?;
    let alembic_tracked = alembic_heads.is_some();
    let alembic_heads = alembic_heads.unwrap_or_default();
    let alembic_version = match alembic_heads.as_slice() {
        [] => None,
        [rev] => Some(
            MIGRATIONS
                .iter()
                .find(|m| m.alembic_revision == rev)
                .ok_or_else(|| MigrateError::UnknownAlembicRevision(rev.clone()))?
                .version,
        ),
        _ => return Err(MigrateError::SeveralAlembicHeads(alembic_heads)),
    };

    let mut before = sqlx_applied(conn).await?;
    if alembic_tracked {
        let alembic_head = alembic_version.unwrap_or(0);
        let reverted: Vec<i64> = before.iter().filter(|v| **v > alembic_head).copied().collect();
        if !reverted.is_empty() {
            tracing::warn!(
                ?reverted,
                alembic_head,
                "migrations were reverted by alembic downgrade; they will be applied again"
            );
            sqlx::query("DELETE FROM _sqlx_migrations WHERE version > $1")
                .bind(alembic_head)
                .execute(&mut *conn)
                .await?;
            before.retain(|v| *v <= alembic_head);
            report.reverted_by_alembic = reverted;
        }
    }
    if let Some(target) = alembic_version {
        // Схема до `target` уже создана alembic: отмечаем без выполнения SQL.
        migrator.skip(&mut *conn, Some(target)).await?;
        let after_skip = sqlx_applied(conn).await?;
        report.adopted_from_alembic = after_skip
            .iter()
            .filter(|v| !before.contains(v))
            .copied()
            .collect();
    }

    let before_run = sqlx_applied(conn).await?;
    migrator.run(&mut *conn).await?;
    let after = sqlx_applied(conn).await?;
    report.applied = after
        .iter()
        .filter(|v| !before_run.contains(v))
        .copied()
        .collect();

    // Ревизия alembic = последняя применённая миграция.
    let head = after
        .iter()
        .max()
        .and_then(|v| MIGRATIONS.iter().find(|m| m.version == *v));
    if let Some(head) = head {
        if alembic_version != Some(head.version) {
            write_alembic_version(conn, head.alembic_revision).await?;
        }
        report.alembic_revision = Some(head.alembic_revision.to_owned());
    }
    Ok(report)
}

async fn table_exists(conn: &mut PgConnection, name: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
        .bind(name)
        .fetch_one(&mut *conn)
        .await
}

/// `None` — таблицы `alembic_version` нет; `Some([])` — есть, но пуста (`downgrade base`).
async fn alembic_heads(conn: &mut PgConnection) -> Result<Option<Vec<String>>, sqlx::Error> {
    if !table_exists(conn, "alembic_version").await? {
        return Ok(None);
    }
    sqlx::query_scalar("SELECT version_num FROM alembic_version ORDER BY version_num")
        .fetch_all(&mut *conn)
        .await
        .map(Some)
}

async fn sqlx_applied(conn: &mut PgConnection) -> Result<Vec<i64>, sqlx::Error> {
    if !table_exists(conn, "_sqlx_migrations").await? {
        return Ok(Vec::new());
    }
    sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
        .fetch_all(&mut *conn)
        .await
}

async fn write_alembic_version(conn: &mut PgConnection, revision: &str) -> Result<(), sqlx::Error> {
    // Та же DDL, что создаёт alembic (имя ограничения alembic_version_pkc).
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS alembic_version (
             version_num VARCHAR(32) NOT NULL,
             CONSTRAINT alembic_version_pkc PRIMARY KEY (version_num)
         )",
    )
    .execute(&mut *conn)
    .await?;
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    sqlx::query("DELETE FROM alembic_version")
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO alembic_version (version_num) VALUES ($1)")
        .bind(revision)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
