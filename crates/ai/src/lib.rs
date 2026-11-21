//! `ai-worker`: забирает задачи `analysis_jobs`, вызывает локальную модель и сохраняет
//! проверенный результат. API читает готовый результат и не ждёт модель.
//!
//! Свойства цикла:
//! * задача берётся атомарно (`FOR UPDATE SKIP LOCKED`), транзакция фиксируется до запроса к модели;
//! * завершение проверяет номер попытки — запоздалый ответ не затирает более новую попытку;
//! * временные ошибки повторяются с растущей задержкой в пределах `LLM_MAX_ATTEMPTS`;
//! * задача с устаревшим входом (описание успели изменить) закрывается без вызова модели;
//! * параллелизм — один запрос: на сервере один слот `llama-server`.

pub mod client;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use dashboard_config::Settings;
use dashboard_domain::analysis::candidate_teams;
use dashboard_domain::llm::{
    CLUSTER_KIND, SummaryInput, chat_request, cluster_request, parse_cluster_output, parse_output,
};
use dashboard_domain::mass_problems::{ReviewInput, Workspace, review_input};
use dashboard_store::analysis_repo::{self, ClaimedJob, JobSpec, NewReview, OBSOLETE};
use dashboard_store::read_repo;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::watch;

pub use client::{CallError, LlmClient};

/// Повторная постановка задач для карточек без актуальной задачи (на случай, если импортёр
/// работал с выключенной интеграцией).
const BACKFILL_INTERVAL: Duration = Duration::from_secs(300);

/// Как часто сверяются автоматические группы массовых сбоев с очередью проверок.
const REVIEW_SYNC_INTERVAL: Duration = Duration::from_secs(10);

/// Файл, время изменения которого показывает, что цикл жив (`ai-worker --healthcheck`).
pub fn heartbeat_path() -> PathBuf {
    std::env::temp_dir().join("ai-worker.heartbeat")
}

/// Насколько старым может быть heartbeat: самый долгий шаг цикла — запрос к модели.
pub fn heartbeat_max_age(settings: &Settings) -> Duration {
    settings.llm_lease + settings.llm_poll_interval + Duration::from_secs(30)
}

/// Проверка для `HEALTHCHECK`: heartbeat свежий.
pub fn heartbeat_is_fresh(path: &Path, max_age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age <= max_age)
}

fn touch(path: Option<&Path>) {
    if let Some(path) = path
        && let Err(e) = std::fs::write(path, b"ok")
    {
        tracing::warn!("heartbeat: {e}");
    }
}

/// Задержка перед повтором: 10 с, 20 с, 40 с … не больше 5 минут.
pub fn retry_delay(attempt: i32) -> Duration {
    let exp = u32::try_from(attempt.saturating_sub(1).clamp(0, 5)).unwrap_or(5);
    Duration::from_secs(10 * 2u64.pow(exp)).min(Duration::from_secs(300))
}

/// Чем закончилась обработка одной задачи (для журнала и тестов).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    /// Результат не сохранён: задачу уже забрала другая попытка.
    Stale,
    Obsolete,
    Retry(String),
    Failed(String),
    Stopped,
}

pub struct Worker {
    pub settings: Arc<Settings>,
    pub pool: PgPool,
    pub client: LlmClient,
    pub spec: JobSpec,
    pub cluster_spec: JobSpec,
}

impl Worker {
    pub fn new(settings: Arc<Settings>, pool: PgPool, client: LlmClient) -> Self {
        let spec = JobSpec::summary(&settings);
        let cluster_spec = JobSpec::cluster(&settings);
        Self {
            settings,
            pool,
            client,
            spec,
            cluster_spec,
        }
    }

    /// Ставит недостающие задачи (идемпотентно).
    pub async fn backfill(&self) -> Result<u64, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        analysis_repo::enqueue(&mut conn, &self.spec).await
    }

    /// Проверки для текущих автоматических групп (без ручных групп и исключённых записей).
    pub async fn current_reviews(&self) -> Result<Vec<ReviewInput>, sqlx::Error> {
        let mut tx = read_repo::begin_consistent_read(&self.pool).await?;
        let rows = read_repo::present_incidents(&mut tx).await?;
        let state = analysis_repo::mass_problem_workspace(&mut tx).await?;
        tx.rollback().await?;
        let workspace: Workspace = serde_json::from_value(state).unwrap_or(Workspace {
            groups: Vec::new(),
            exclusions: Vec::new(),
        });
        Ok(workspace
            .automatic(&rows)
            .iter()
            .filter_map(|cluster| review_input(cluster, &rows))
            .collect())
    }

    /// Ставит проверки для новых групп и снимает с очереди исчезнувшие (идемпотентно).
    pub async fn sync_reviews(&self) -> Result<u64, sqlx::Error> {
        let reviews = self.current_reviews().await?;
        let new: Vec<NewReview<'_>> = reviews
            .iter()
            .map(|r| NewReview {
                anchor_pk: r.anchor_pk,
                anchor_id: &r.anchor_id,
                anchor_sys_id: &r.anchor_sys_id,
                anchor_revision: r.anchor_revision,
                input: &r.input,
            })
            .collect();
        let mut conn = self.pool.acquire().await?;
        analysis_repo::enqueue_reviews(&mut conn, &self.cluster_spec, &new).await
    }

    /// Берёт и обрабатывает одну задачу (сначала проверки групп: их мало, а координатор
    /// видит их сразу). `None` — очередь пуста.
    pub async fn process_one(
        &self,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<Option<Outcome>, sqlx::Error> {
        let job = {
            let mut conn = self.pool.acquire().await?;
            match analysis_repo::claim(&mut conn, &self.cluster_spec, self.settings.llm_lease).await? {
                Some(job) => Some((job, true)),
                None => analysis_repo::claim(&mut conn, &self.spec, self.settings.llm_lease)
                    .await?
                    .map(|job| (job, false)),
            }
        };
        let Some((job, is_cluster)) = job else {
            return Ok(None);
        };
        let started = Instant::now();
        let outcome = if is_cluster {
            self.run_review(&job, stop).await?
        } else {
            self.run(&job, stop).await?
        };
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match &outcome {
            Outcome::Completed | Outcome::Obsolete | Outcome::Stale | Outcome::Stopped => {
                tracing::info!(job = %job.id, kind = if is_cluster { CLUSTER_KIND } else { "summary" }, attempt = job.attempt, elapsed_ms, outcome = ?outcome, "ИИ-задача обработана");
            }
            Outcome::Retry(_) | Outcome::Failed(_) => {
                tracing::warn!(job = %job.id, kind = if is_cluster { CLUSTER_KIND } else { "summary" }, attempt = job.attempt, elapsed_ms, outcome = ?outcome, "ИИ-задача не выполнена");
            }
        }
        Ok(Some(outcome))
    }

    async fn run(&self, job: &ClaimedJob, stop: &mut watch::Receiver<bool>) -> Result<Outcome, sqlx::Error> {
        let mut tx = read_repo::begin_consistent_read(&self.pool).await?;
        if !analysis_repo::input_is_current(&mut tx, job.incident_pk, &job.input).await? {
            tx.rollback().await?;
            return self.finish_failed(job, OBSOLETE, None, Outcome::Obsolete).await;
        }
        let rows = read_repo::present_incidents(&mut tx).await?;
        let teams = analysis_repo::team_names(&mut tx).await?;
        tx.rollback().await?;
        let Some(item) = rows.iter().find(|r| r.pk == job.incident_pk) else {
            return self.finish_failed(job, OBSOLETE, None, Outcome::Obsolete).await;
        };
        let candidates: Vec<String> = candidate_teams(item, &rows, &self.settings.coordinator_group_name)
            .into_iter()
            .filter(|c| teams.iter().any(|t| t == c.team))
            .map(|c| c.team.to_owned())
            .collect();
        let input: SummaryInput = match serde_json::from_value(job.input.clone()) {
            Ok(input) => input,
            Err(e) => {
                let error = format!("invalid job input: {e}");
                return self
                    .finish_failed(job, &error, None, Outcome::Failed(error.clone()))
                    .await;
            }
        };
        let body = chat_request(
            &self.settings.llm_model,
            &input,
            &candidates,
            self.settings.llm_max_tokens,
            usize::try_from(self.settings.llm_input_max_chars).unwrap_or(usize::MAX),
        );
        let Some(reply) = self.call(job, &body, stop).await? else {
            return Ok(Outcome::Stopped);
        };
        match reply {
            Ok(content) => match parse_output(&content, &candidates) {
                Ok(output) => {
                    let result = json!({
                        "summary": output.summary,
                        "team": output.team,
                        "candidates": candidates,
                    });
                    let mut conn = self.pool.acquire().await?;
                    let saved = analysis_repo::complete(&mut conn, job, "llm", &result).await?;
                    Ok(if saved { Outcome::Completed } else { Outcome::Stale })
                }
                Err(e) => {
                    let error = format!("invalid output: {e}");
                    self.finish_failed(
                        job,
                        &error,
                        Some(retry_delay(job.attempt)),
                        Outcome::Retry(error.clone()),
                    )
                    .await
                }
            },
            Err(e) => self.call_failed(job, e).await,
        }
    }

    async fn run_review(
        &self,
        job: &ClaimedJob,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<Outcome, sqlx::Error> {
        if !self.current_reviews().await?.iter().any(|r| r.input == job.input) {
            return self.finish_failed(job, OBSOLETE, None, Outcome::Obsolete).await;
        }
        let body = cluster_request(&self.settings.llm_model, &job.input, self.settings.llm_max_tokens);
        let Some(reply) = self.call(job, &body, stop).await? else {
            return Ok(Outcome::Stopped);
        };
        match reply {
            Ok(content) => match parse_cluster_output(&content) {
                Ok(output) => {
                    let result = json!({
                        "related": output.related,
                        "title": output.title,
                        "summary": output.summary,
                    });
                    let mut conn = self.pool.acquire().await?;
                    let saved = analysis_repo::complete(&mut conn, job, "llm", &result).await?;
                    Ok(if saved { Outcome::Completed } else { Outcome::Stale })
                }
                Err(e) => {
                    let error = format!("invalid output: {e}");
                    self.finish_failed(
                        job,
                        &error,
                        Some(retry_delay(job.attempt)),
                        Outcome::Retry(error.clone()),
                    )
                    .await
                }
            },
            Err(e) => self.call_failed(job, e).await,
        }
    }

    /// Запрос к модели с учётом остановки. `None` — worker останавливается, задача возвращена.
    async fn call(
        &self,
        job: &ClaimedJob,
        body: &serde_json::Value,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<Option<Result<String, CallError>>, sqlx::Error> {
        tokio::select! {
            reply = self.client.chat(body) => Ok(Some(reply)),
            _ = stop.wait_for(|stopped| *stopped) => {
                // Попытка не засчитывается: задача сразу вернётся в очередь.
                let mut conn = self.pool.acquire().await?;
                analysis_repo::release(&mut conn, job).await?;
                Ok(None)
            }
        }
    }

    async fn call_failed(&self, job: &ClaimedJob, error: CallError) -> Result<Outcome, sqlx::Error> {
        match error {
            CallError::Transient(e) => {
                self.finish_failed(job, &e, Some(retry_delay(job.attempt)), Outcome::Retry(e.clone()))
                    .await
            }
            CallError::Permanent(e) => {
                self.finish_failed(job, &e, None, Outcome::Failed(e.clone()))
                    .await
            }
        }
    }

    async fn finish_failed(
        &self,
        job: &ClaimedJob,
        error: &str,
        retry_after: Option<Duration>,
        outcome: Outcome,
    ) -> Result<Outcome, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        if !analysis_repo::fail(&mut conn, job, error, retry_after).await? {
            return Ok(Outcome::Stale);
        }
        Ok(match outcome {
            Outcome::Retry(e) if job.attempt >= job.max_attempts => Outcome::Failed(e),
            other => other,
        })
    }

    /// Основной цикл до сигнала остановки. Ошибки БД не завершают процесс.
    pub async fn run_loop(&self, mut stop: watch::Receiver<bool>, heartbeat: Option<&Path>) {
        let mut last_backfill: Option<Instant> = None;
        let mut last_sync: Option<Instant> = None;
        while !*stop.borrow() {
            if last_sync.is_none_or(|t| t.elapsed() >= REVIEW_SYNC_INTERVAL) {
                match self.sync_reviews().await {
                    Ok(queued) => {
                        last_sync = Some(Instant::now());
                        if queued > 0 {
                            tracing::info!(queued, "Поставлены проверки групп массовых сбоев");
                        }
                    }
                    Err(e) => tracing::warn!("Не удалось поставить проверки групп: {e}"),
                }
            }
            if last_backfill.is_none_or(|t| t.elapsed() >= BACKFILL_INTERVAL) {
                match self.backfill().await {
                    Ok(queued) => {
                        last_backfill = Some(Instant::now());
                        if queued > 0 {
                            tracing::info!(queued, "Поставлены задачи ИИ-анализа");
                        }
                    }
                    Err(e) => tracing::warn!("Не удалось поставить задачи: {e}"),
                }
            }
            // Heartbeat — только после успешного обращения к очереди: без БД worker нездоров.
            let idle = match self.process_one(&mut stop).await {
                Ok(Some(_)) => {
                    touch(heartbeat);
                    None
                }
                Ok(None) => {
                    touch(heartbeat);
                    Some(self.settings.llm_poll_interval)
                }
                Err(e) => {
                    tracing::warn!("Ошибка БД в цикле ИИ-задач: {e}");
                    Some(self.settings.llm_poll_interval.max(Duration::from_secs(5)))
                }
            };
            if let Some(idle) = idle {
                tokio::select! {
                    _ = tokio::time::sleep(idle) => {}
                    _ = stop.wait_for(|stopped| *stopped) => {}
                }
            }
        }
    }
}

/// Режим `LLM_ENABLED=false`: процесс ничего не вызывает и ждёт остановки (heartbeat свежий).
pub async fn idle_until_stopped(mut stop: watch::Receiver<bool>, heartbeat: Option<&Path>) {
    while !*stop.borrow() {
        touch(heartbeat);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            _ = stop.wait_for(|stopped| *stopped) => {}
        }
    }
}
