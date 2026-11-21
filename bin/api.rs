//! HTTP API. Порт по умолчанию 8080 (`API_PORT`), рядом с Python-версией публикуется как 8081.
//!
//! `api --healthcheck` — проверка `/health` без curl (для HEALTHCHECK в distroless-образе).

#[path = "common.rs"]
mod common;
#[path = "api_coordinator.rs"]
mod coordinator;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use dashboard_api::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1).peekable();
    if arguments.next_if(|argument| argument == "coordinator").is_some() {
        return coordinator::run(arguments.collect()).await;
    }
    if arguments.any(|argument| argument == "--healthcheck") {
        std::process::exit(healthcheck());
    }

    let settings = common::load_settings();
    common::init_tracing(&settings);
    let pool = dashboard_store::connect_lazy(&settings).context("invalid DATABASE_URL")?;
    let addr: SocketAddr = format!("{}:{}", settings.api_host, settings.api_port)
        .parse()
        .context("invalid API_HOST/API_PORT")?;
    let settings = Arc::new(settings);
    let state = AppState::new(Arc::clone(&settings), pool.clone());

    // IMPORTER_EMBEDDED=true: импортёр в том же процессе (с тем же advisory lock).
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let importer = settings.importer_embedded.then(|| {
        let settings = Arc::clone(&settings);
        tokio::spawn(async move {
            if let Err(e) = dashboard_importer::run_importer(settings, stop_rx).await {
                tracing::error!("embedded importer failed: {e}");
            }
        })
    });

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, env = %state.settings.environment, "API started");
    let shutdown = async move {
        common::shutdown_signal().await;
        let _ = stop_tx.send(true);
    };
    dashboard_api::serve(listener, state, shutdown).await?;
    if let Some(task) = importer {
        let _ = task.await;
    }
    pool.close().await;
    tracing::info!("API stopped");
    Ok(())
}

/// GET http://127.0.0.1:$API_PORT/health; 0 — ответ 200.
fn healthcheck() -> i32 {
    use std::io::{Read, Write};
    use std::time::Duration;
    let port = std::env::var("API_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3)) else {
        return 1;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    if stream
        .write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .is_err()
    {
        return 1;
    }
    let mut head = [0u8; 16];
    match stream.read(&mut head) {
        Ok(n) if head[..n].starts_with(b"HTTP/1.1 200") || head[..n].starts_with(b"HTTP/1.0 200") => 0,
        _ => 1,
    }
}
