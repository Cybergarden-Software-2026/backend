//! Применяет миграции из `migrations/` — аналог `alembic upgrade head` Python-версии.
//! Совместим с базой, созданной Python-версией, и наоборот (см. `dashboard_store::migrate`).

#[path = "common.rs"]
mod common;

use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings = common::load_settings();
    common::init_tracing(&settings);

    let pool = dashboard_store::connect(&settings)
        .await
        .context("cannot connect to PostgreSQL")?;
    let report = dashboard_store::migrate::run(&pool)
        .await
        .context("migration failed")?;
    tracing::info!(
        applied = ?report.applied,
        adopted_from_alembic = ?report.adopted_from_alembic,
        reverted_by_alembic = ?report.reverted_by_alembic,
        alembic_revision = ?report.alembic_revision,
        "database schema is up to date"
    );
    pool.close().await;
    Ok(())
}
