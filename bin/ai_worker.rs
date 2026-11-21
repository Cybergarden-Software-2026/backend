//! ИИ-worker: очередь `analysis_jobs` → локальная модель (`LLM_BASE_URL`) → проверенный результат.
//!
//! `ai-worker --healthcheck` — проверка для `HEALTHCHECK`/compose без shell: код 0, если цикл
//! обработки обновлял heartbeat недавно.

#[path = "common.rs"]
mod common;

use std::sync::Arc;

use anyhow::Context;
use dashboard_ai::{LlmClient, Worker, heartbeat_is_fresh, heartbeat_max_age, heartbeat_path};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings = Arc::new(common::load_settings());
    let heartbeat = heartbeat_path();
    if std::env::args().nth(1).as_deref() == Some("--healthcheck") {
        let fresh = heartbeat_is_fresh(&heartbeat, heartbeat_max_age(&settings));
        std::process::exit(if fresh { 0 } else { 1 });
    }
    common::init_tracing(&settings);

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        common::shutdown_signal().await;
        let _ = stop_tx.send(true);
    });

    if !settings.llm_enabled {
        tracing::info!("LLM_ENABLED=false: ИИ-worker не обращается к модели и ждёт остановки");
        dashboard_ai::idle_until_stopped(stop_rx, Some(&heartbeat)).await;
        return Ok(());
    }
    let Some(base_url) = settings.llm_base_url.as_deref() else {
        eprintln!("invalid configuration:\n  - LLM_BASE_URL: is required when LLM_ENABLED=true");
        std::process::exit(2);
    };
    let client = LlmClient::new(base_url, settings.llm_timeout).context("HTTP client")?;
    let pool = dashboard_store::connect_lazy(&settings).context("invalid DATABASE_URL")?;
    let worker = Worker::new(settings.clone(), pool, client);
    tracing::info!(
        model = %worker.spec.model,
        prompt = %worker.spec.prompt_version,
        "ИИ-worker запущен"
    );
    worker.run_loop(stop_rx, Some(&heartbeat)).await;
    tracing::info!("ИИ-worker остановлен");
    Ok(())
}
