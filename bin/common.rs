//! Общее для бинарников: загрузка настроек и логирование.

use dashboard_config::Settings;
use tracing_subscriber::EnvFilter;

/// Настройки из `.env` и окружения; при ошибке — понятное сообщение и код выхода 2.
pub fn load_settings() -> Settings {
    match Settings::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}

/// Уровень из `LOG_LEVEL` (общий `.env`); `RUST_LOG`, если задан, важнее.
pub fn init_tracing(settings: &Settings) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "{level},sqlx=warn,tower_http=info",
            level = settings.tracing_level()
        ))
    });
    // Цвета — только в терминале: в `docker compose logs` и файлах без escape-последовательностей.
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_ansi(ansi)
        .init();
    for warning in &settings.env_file_warnings {
        tracing::warn!("skipped .env line: {warning}");
    }
}

/// SIGINT или SIGTERM.
#[allow(dead_code)]
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
