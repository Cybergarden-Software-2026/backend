//! Фоновый импортёр — порт `app/importer/worker.py`: раз в `IMPORT_POLL_INTERVAL_SECONDS`
//! проверяет файл и применяет снимок. Единственность экземпляра гарантирует
//! `pg_try_advisory_lock` на выделенном соединении (ключ общий с Python-версией).

use std::sync::Arc;
use std::time::Duration;

use dashboard_config::Settings;
use sqlx::{ConnectOptions, PgConnection};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::reader::{self, FileFingerprint, ReadOptions, ReadResult};
use crate::service::{ImportError, ImportOutcome, ImportService, OutcomeStatus};

/// Ключ advisory lock импортёра — тот же, что в Python-версии («MTS» + 1).
pub const IMPORTER_LOCK_KEY: i64 = 0x4D54_5301;

/// Сигнал остановки: `true` в канале — пора завершаться.
pub type StopSignal = watch::Receiver<bool>;

fn stopped(stop: &StopSignal) -> bool {
    *stop.borrow()
}

/// Ждёт `timeout` или сигнал остановки (`asyncio.wait_for(stop.wait(), timeout)`).
async fn wait_or_stop(stop: &mut StopSignal, timeout: Duration) {
    if stopped(stop) {
        return;
    }
    let _ = tokio::time::timeout(timeout, stop.wait_for(|s| *s)).await;
}

/// Функция чтения (в тестах подменяется).
pub type Reader =
    Arc<dyn Fn(&std::path::Path, Option<FileFingerprint>, &ReadOptions) -> ReadResult + Send + Sync>;

pub struct ImporterWorker {
    service: ImportService,
    reader: Reader,
    /// Отпечаток последнего ПРИНЯТОГО содержимого. После ошибки сбрасывается, чтобы файл
    /// перечитывался каждую итерацию и счётчик ошибок рос до порога.
    fingerprint: Option<FileFingerprint>,
}

impl ImporterWorker {
    pub fn new(service: ImportService) -> Self {
        Self::with_reader(service, Arc::new(reader::read_snapshot_file))
    }

    pub fn with_reader(service: ImportService, reader: Reader) -> Self {
        Self {
            service,
            reader,
            fingerprint: None,
        }
    }

    pub fn service(&self) -> &ImportService {
        &self.service
    }

    pub async fn tick(&mut self) -> Result<ImportOutcome, ImportError> {
        let settings = self.service.settings();
        let path = settings.json_source_path.clone();
        let options = ReadOptions {
            retry_delay: settings.import_stable_read_delay,
            ..ReadOptions::default()
        };
        let known = self.fingerprint;
        let read = Arc::clone(&self.reader);
        let result = tokio::task::spawn_blocking(move || read(&path, known, &options))
            .await
            .map_err(|e| ImportError::Task(e.to_string()))?;
        let fingerprint = result.fingerprint();

        let outcome = match self.service.process(result).await {
            Ok(outcome) => outcome,
            // Ошибка БД или непредвиденная ошибка обработки.
            Err(e) => {
                tracing::error!("Ошибка обработки снимка: {e}");
                self.fingerprint = None;
                let message = format!("Ошибка сохранения снимка: {}", e.python_type_name());
                return match self
                    .service
                    .record_failure(&message, OutcomeStatus::Rejected)
                    .await
                {
                    Ok(outcome) => Ok(outcome),
                    Err(e) => {
                        tracing::error!("Не удалось записать состояние источника: {e}");
                        Err(e)
                    }
                };
            }
        };
        self.fingerprint = if outcome.accepted() { fingerprint } else { None };
        // Окно «Решён» истекает со временем, а не с изменением файла.
        self.service.expire_resolutions().await?;
        Ok(outcome)
    }

    pub async fn run(&mut self, mut stop: StopSignal) {
        let interval = self.service.settings().import_poll_interval;
        tracing::info!(
            "Импортёр запущен: {}, интервал {:.1} с",
            self.service.settings().json_source_path.display(),
            interval.as_secs_f64()
        );
        while !stopped(&stop) {
            let started = Instant::now();
            if let Err(e) = self.tick().await {
                tracing::error!(
                    "Сбой итерации импорта; повтор через {:.1} с: {e}",
                    interval.as_secs_f64()
                );
            }
            let remaining = interval.saturating_sub(started.elapsed());
            wait_or_stop(&mut stop, remaining).await;
        }
    }
}

/// Выделенное соединение с advisory lock — держится всё время работы импортёра.
/// `None` — пришёл сигнал остановки раньше, чем удалось получить блокировку.
pub async fn acquire_lock(settings: &Settings, stop: &mut StopSignal) -> Option<PgConnection> {
    while !stopped(stop) {
        match try_lock(settings).await {
            Ok(Some(conn)) => return Some(conn),
            Ok(None) => {
                tracing::warn!("Импортёр уже запущен в другом процессе; ожидание блокировки");
            }
            Err(e) => tracing::error!("Нет соединения с PostgreSQL; повтор через 2 с: {e}"),
        }
        wait_or_stop(stop, Duration::from_secs(2)).await;
    }
    None
}

async fn try_lock(settings: &Settings) -> Result<Option<PgConnection>, sqlx::Error> {
    let mut conn = dashboard_store::connect_options(settings)?.connect().await?;
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(IMPORTER_LOCK_KEY)
        .fetch_one(&mut conn)
        .await?;
    if got {
        Ok(Some(conn))
    } else {
        let _ = sqlx::Connection::close(conn).await;
        Ok(None)
    }
}

/// Импорт с удержанием advisory lock (`run_importer`): процесс `importer` и, при
/// `IMPORTER_EMBEDDED=true`, процесс API.
pub async fn run_importer(settings: Arc<Settings>, mut stop: StopSignal) -> Result<(), sqlx::Error> {
    let pool = dashboard_store::connect_lazy(&settings)?;
    let Some(mut lock_conn) = acquire_lock(&settings, &mut stop).await else {
        return Ok(());
    };
    ImporterWorker::new(ImportService::new(pool.clone(), settings))
        .run(stop)
        .await;
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(IMPORTER_LOCK_KEY)
        .execute(&mut lock_conn)
        .await;
    let _ = sqlx::Connection::close(lock_conn).await;
    pool.close().await;
    Ok(())
}
