//! Фоновый импортёр JSON-источника — аналог `python -m app.importer.worker`.
//! Одновременно работает один импортёр любой версии: advisory lock `0x4D545301` общий.

#[path = "common.rs"]
mod common;

use std::sync::Arc;

use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings = Arc::new(common::load_settings());
    common::init_tracing(&settings);

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        common::shutdown_signal().await;
        let _ = stop_tx.send(true);
    });
    dashboard_importer::run_importer(settings, stop_rx)
        .await
        .context("invalid DATABASE_URL")?;
    tracing::info!("Импортёр остановлен");
    Ok(())
}
