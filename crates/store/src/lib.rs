//! Доступ к PostgreSQL через sqlx.
//!
//! Пул соединений (как `app/core/database.py`), миграции на общем SQL (R1) и репозиторий
//! импортёра (R4), чтение для API в одной транзакции `REPEATABLE READ READ ONLY` (R5).

pub mod import_repo;
pub mod migrate;
pub mod read_repo;

use std::time::Duration;

use dashboard_config::Settings;
use sqlx::ConnectOptions;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

pub use sqlx::PgPool as Pool;

/// Параметры соединения: время сессии в UTC и `application_name`, как в Python-версии.
pub fn connect_options(settings: &Settings) -> Result<PgConnectOptions, sqlx::Error> {
    let app_name: String = settings.app_name.chars().take(60).collect();
    let mut options: PgConnectOptions = settings.database_url.parse()?;
    options = options.application_name(&app_name).options([("timezone", "UTC")]);
    options = if settings.db_echo {
        options.log_statements(tracing::log::LevelFilter::Info)
    } else {
        options.log_statements(tracing::log::LevelFilter::Debug)
    };
    Ok(options)
}

/// Пул без подключения при создании: API стартует и отвечает на `/health`, даже если БД недоступна
/// (`/health/ready` в этом случае вернёт 503).
pub fn connect_lazy(settings: &Settings) -> Result<PgPool, sqlx::Error> {
    Ok(PgPoolOptions::new()
        .max_connections(settings.db_max_connections())
        .acquire_timeout(Duration::from_secs(5))
        // Аналог pool_pre_ping=True.
        .test_before_acquire(true)
        .connect_lazy_with(connect_options(settings)?))
}

/// Пул с немедленной проверкой подключения (для процессов, которым без БД делать нечего).
pub async fn connect(settings: &Settings) -> Result<PgPool, sqlx::Error> {
    let pool = connect_lazy(settings)?;
    // Реальная ошибка подключения (неверный пароль, нет хоста) важнее, чем просто «false».
    drop(pool.acquire().await?);
    Ok(pool)
}

/// `SELECT 1` — порт `check_database()`.
pub async fn check_database(pool: &PgPool) -> bool {
    let probe = sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(pool);
    matches!(
        tokio::time::timeout(Duration::from_secs(5), probe).await,
        Ok(Ok(1))
    )
}
