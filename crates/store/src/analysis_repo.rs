//! Очередь ИИ-задач в `analysis_jobs`: выжимки карточек (`summary`) и проверка групп массовых
//! сбоев (`cluster`, миграция 0004).
//!
//! Актуальность задачи определяется не номером ревизии, а входом: `input` — снимок полей реестра,
//! из которых строится выжимка ([`INPUT_SQL`]). Поэтому смена команды или статуса не вызывает
//! повторную генерацию, а правка описания сразу делает старый результат неактуальным.
//! `incident_revision` хранит ревизию записи на момент постановки (уникальный ключ схемы).

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgConnection, Row};

/// Поля инцидента `i` для задачи `summary` (ключи — как у `dashboard_domain::llm::SummaryInput`).
pub const INPUT_SQL: &str = "jsonb_build_object(\
'header', i.header, \
'shortDescription', i.short_description, \
'description', i.description, \
'product', i.product_name, \
'service', i.service, \
'scenario', i.scenario_name)";

/// Отметка в `error` для задач, вход которых больше не совпадает с реестром.
pub const OBSOLETE: &str = "obsolete";

/// Идентификатор генерации: какие задачи ставить, забирать и выдавать.
#[derive(Debug, Clone)]
pub struct JobSpec {
    pub kind: String,
    pub model: String,
    pub prompt_version: String,
    pub max_attempts: i32,
}

impl JobSpec {
    /// Проверка групп массовых сбоев текущей генерации.
    pub fn cluster(settings: &dashboard_config::Settings) -> Self {
        Self {
            kind: dashboard_domain::llm::CLUSTER_KIND.to_owned(),
            prompt_version: dashboard_domain::llm::CLUSTER_PROMPT_VERSION.to_owned(),
            ..Self::summary(settings)
        }
    }

    /// Задача `summary` текущей генерации из настроек (общих для импортёра, API и `ai-worker`).
    pub fn summary(settings: &dashboard_config::Settings) -> Self {
        Self {
            kind: dashboard_domain::llm::SUMMARY_KIND.to_owned(),
            model: settings.llm_job_model(),
            prompt_version: dashboard_domain::llm::SUMMARY_PROMPT_VERSION.to_owned(),
            max_attempts: i32::try_from(settings.llm_max_attempts).unwrap_or(i32::MAX),
        }
    }
}

/// Ставит задачи для присутствующих карточек, у которых ещё нет задачи с текущим входом,
/// и снимает с очереди ожидающие задачи с устаревшим входом или другой генерацией.
/// Идемпотентна: повторный вызов без изменений реестра ничего не добавляет.
/// Возвращает число поставленных задач.
pub async fn enqueue(conn: &mut PgConnection, spec: &JobSpec) -> Result<u64, sqlx::Error> {
    let obsolete = format!(
        "UPDATE analysis_jobs j SET status = 'failed', error = '{OBSOLETE}', locked_until = NULL, \
         finished_at = now(), updated_at = now() \
         FROM incidents i \
         WHERE j.incident_pk = i.pk AND j.kind = $1 AND j.status = 'queued' \
           AND (NOT i.is_present OR j.input <> {INPUT_SQL} OR j.model <> $2 OR j.prompt_version <> $3)"
    );
    sqlx::query(sqlx::AssertSqlSafe(obsolete))
        .bind(&spec.kind)
        .bind(&spec.model)
        .bind(&spec.prompt_version)
        .execute(&mut *conn)
        .await?;
    let insert = format!(
        "INSERT INTO analysis_jobs \
           (incident_pk, incident_id, sys_id, kind, incident_revision, input, model, prompt_version, max_attempts) \
         SELECT i.pk, i.id, i.sys_id, $1, i.incident_revision, {INPUT_SQL}, $2, $3, $4 \
         FROM incidents i \
         WHERE i.is_present AND NOT EXISTS ( \
           SELECT 1 FROM analysis_jobs j \
           WHERE j.incident_pk = i.pk AND j.kind = $1 AND j.model = $2 AND j.prompt_version = $3 \
             AND j.input = {INPUT_SQL} AND j.error IS DISTINCT FROM '{OBSOLETE}') \
         ORDER BY i.pk \
         ON CONFLICT DO NOTHING"
    );
    let done = sqlx::query(sqlx::AssertSqlSafe(insert))
        .bind(&spec.kind)
        .bind(&spec.model)
        .bind(&spec.prompt_version)
        .bind(spec.max_attempts)
        .execute(conn)
        .await?;
    Ok(done.rows_affected())
}

/// Задача, взятая в работу.
#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub id: uuid::Uuid,
    pub incident_pk: i64,
    pub incident_id: String,
    pub attempt: i32,
    pub max_attempts: i32,
    pub input: Value,
}

/// Атомарно берёт одну задачу (`FOR UPDATE SKIP LOCKED`): ожидающую или с истёкшей арендой.
/// Номер попытки увеличивается сразу; транзакция фиксируется до обращения к модели.
pub async fn claim(
    conn: &mut PgConnection,
    spec: &JobSpec,
    lease: Duration,
) -> Result<Option<ClaimedJob>, sqlx::Error> {
    // Аренды, истёкшие на последней попытке, завершаются: повторять их больше нельзя.
    sqlx::query(
        "UPDATE analysis_jobs SET status = 'failed', locked_until = NULL, \
           error = coalesce(error, 'lease expired'), finished_at = now(), updated_at = now() \
         WHERE status = 'running' AND locked_until < now() AND attempts >= max_attempts",
    )
    .execute(&mut *conn)
    .await?;
    let row = sqlx::query(
        "WITH next AS ( \
           SELECT id FROM analysis_jobs \
           WHERE kind = $1 AND model = $2 AND prompt_version = $3 AND attempts < max_attempts \
             AND ((status = 'queued' AND available_at <= now()) \
               OR (status = 'running' AND locked_until < now())) \
           ORDER BY available_at, created_at \
           LIMIT 1 FOR UPDATE SKIP LOCKED) \
         UPDATE analysis_jobs j SET status = 'running', attempts = j.attempts + 1, \
           locked_until = now() + make_interval(secs => $4), \
           started_at = coalesce(j.started_at, now()), updated_at = now() \
         FROM next WHERE j.id = next.id \
         RETURNING j.id, j.incident_pk, j.incident_id, j.attempts, j.max_attempts, j.input",
    )
    .bind(&spec.kind)
    .bind(&spec.model)
    .bind(&spec.prompt_version)
    .bind(lease.as_secs_f64())
    .fetch_optional(conn)
    .await?;
    row.map(|r| {
        Ok(ClaimedJob {
            id: r.try_get("id")?,
            incident_pk: r.try_get("incident_pk")?,
            incident_id: r.try_get("incident_id")?,
            attempt: r.try_get("attempts")?,
            max_attempts: r.try_get("max_attempts")?,
            input: r.try_get("input")?,
        })
    })
    .transpose()
}

/// Совпадает ли вход задачи с текущей записью реестра (запись присутствует).
pub async fn input_is_current(
    conn: &mut PgConnection,
    incident_pk: i64,
    input: &Value,
) -> Result<bool, sqlx::Error> {
    let sql = format!("SELECT {INPUT_SQL} = $2 FROM incidents i WHERE i.pk = $1 AND i.is_present");
    let current: Option<bool> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(incident_pk)
        .bind(input)
        .fetch_optional(conn)
        .await?;
    Ok(current == Some(true))
}

/// Сохраняет результат, если задача всё ещё принадлежит этой попытке.
/// `false` — ответ запоздал: задачу уже забрала следующая попытка или она завершена.
pub async fn complete(
    conn: &mut PgConnection,
    job: &ClaimedJob,
    provider: &str,
    result: &Value,
) -> Result<bool, sqlx::Error> {
    let done = sqlx::query(
        "UPDATE analysis_jobs SET status = 'completed', provider = $3, result = $4, error = NULL, \
           locked_until = NULL, finished_at = now(), updated_at = now() \
         WHERE id = $1 AND attempts = $2 AND status = 'running'",
    )
    .bind(job.id)
    .bind(job.attempt)
    .bind(provider)
    .bind(result)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Неуспешная попытка: повтор через `retry_after` (если попытки остались) или `failed`.
/// `retry_after = None` — повторять бессмысленно (например, вход устарел).
pub async fn fail(
    conn: &mut PgConnection,
    job: &ClaimedJob,
    error: &str,
    retry_after: Option<Duration>,
) -> Result<bool, sqlx::Error> {
    let error: String = error.chars().take(500).collect();
    let done = sqlx::query(
        "UPDATE analysis_jobs SET \
           status = CASE WHEN $4::float8 IS NOT NULL AND attempts < max_attempts THEN 'queued' ELSE 'failed' END, \
           available_at = CASE WHEN $4::float8 IS NOT NULL THEN now() + make_interval(secs => $4) \
                               ELSE available_at END, \
           finished_at = CASE WHEN $4::float8 IS NOT NULL AND attempts < max_attempts THEN NULL ELSE now() END, \
           error = $3, locked_until = NULL, updated_at = now() \
         WHERE id = $1 AND attempts = $2 AND status = 'running'",
    )
    .bind(job.id)
    .bind(job.attempt)
    .bind(error)
    .bind(retry_after.map(|d| d.as_secs_f64()))
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Возвращает задачу в очередь без расхода попытки (worker останавливается во время запроса).
pub async fn release(conn: &mut PgConnection, job: &ClaimedJob) -> Result<bool, sqlx::Error> {
    let done = sqlx::query(
        "UPDATE analysis_jobs SET status = 'queued', attempts = attempts - 1, locked_until = NULL, \
           available_at = now(), updated_at = now() \
         WHERE id = $1 AND attempts = $2 AND status = 'running'",
    )
    .bind(job.id)
    .bind(job.attempt)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Готовый результат модели для текущего входа карточки.
#[derive(Debug, Clone)]
pub struct StoredResult {
    pub result: Value,
    pub finished_at: DateTime<Utc>,
}

/// Последний завершённый результат `provider='llm'` для текущих полей инцидента и генерации.
pub async fn current_result(
    conn: &mut PgConnection,
    spec: &JobSpec,
    incident_pk: i64,
) -> Result<Option<StoredResult>, sqlx::Error> {
    let sql = format!(
        "SELECT j.result, j.finished_at FROM analysis_jobs j JOIN incidents i ON i.pk = j.incident_pk \
         WHERE j.incident_pk = $1 AND j.kind = $2 AND j.model = $3 AND j.prompt_version = $4 \
           AND j.status = 'completed' AND j.provider = 'llm' AND j.result IS NOT NULL \
           AND j.finished_at IS NOT NULL AND j.input = {INPUT_SQL} \
         ORDER BY j.finished_at DESC LIMIT 1"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(incident_pk)
        .bind(&spec.kind)
        .bind(&spec.model)
        .bind(&spec.prompt_version)
        .fetch_optional(conn)
        .await?;
    row.map(|r| {
        Ok(StoredResult {
            result: r.try_get("result")?,
            finished_at: r.try_get("finished_at")?,
        })
    })
    .transpose()
}

/// Названия справочника команд.
pub async fn team_names(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT name FROM teams ORDER BY name")
        .fetch_all(conn)
        .await
}

// ---------------------------------------------------------------- проверка групп массовых сбоев

/// Задача `cluster` для группы: запись-якорь и вход (см. `dashboard_domain::mass_problems::review_input`).
#[derive(Debug, Clone)]
pub struct NewReview<'a> {
    pub anchor_pk: i64,
    pub anchor_id: &'a str,
    pub anchor_sys_id: &'a str,
    pub anchor_revision: i32,
    pub input: &'a Value,
}

/// Ставит проверки для текущих автоматических групп, у которых ещё нет задачи с тем же входом,
/// и снимает с очереди ожидающие проверки групп, которых больше нет. Возвращает число новых задач.
pub async fn enqueue_reviews(
    conn: &mut PgConnection,
    spec: &JobSpec,
    reviews: &[NewReview<'_>],
) -> Result<u64, sqlx::Error> {
    let inputs: Vec<&Value> = reviews.iter().map(|r| r.input).collect();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE analysis_jobs SET status = 'failed', error = '{OBSOLETE}', locked_until = NULL, \
           finished_at = now(), updated_at = now() \
         WHERE kind = $1 AND status = 'queued' \
           AND (model <> $2 OR prompt_version <> $3 OR NOT (input = ANY($4::jsonb[])))"
    )))
    .bind(&spec.kind)
    .bind(&spec.model)
    .bind(&spec.prompt_version)
    .bind(&inputs)
    .execute(&mut *conn)
    .await?;
    let mut queued = 0;
    for review in reviews {
        let done = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO analysis_jobs \
               (incident_pk, incident_id, sys_id, kind, incident_revision, input, model, prompt_version, max_attempts) \
             SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9 \
             WHERE NOT EXISTS (SELECT 1 FROM analysis_jobs j \
               WHERE j.kind = $4 AND j.model = $7 AND j.prompt_version = $8 AND j.input = $6 \
                 AND j.error IS DISTINCT FROM '{OBSOLETE}') \
             ON CONFLICT DO NOTHING"
        )))
        .bind(review.anchor_pk)
        .bind(review.anchor_id)
        .bind(review.anchor_sys_id)
        .bind(&spec.kind)
        .bind(review.anchor_revision)
        .bind(review.input)
        .bind(&spec.model)
        .bind(&spec.prompt_version)
        .bind(spec.max_attempts)
        .execute(&mut *conn)
        .await?;
        queued += done.rows_affected();
    }
    Ok(queued)
}

/// Готовые проверки для указанных входов: пары (вход, результат), по последней на вход.
pub async fn review_results(
    conn: &mut PgConnection,
    spec: &JobSpec,
    inputs: &[&Value],
) -> Result<Vec<(Value, Value)>, sqlx::Error> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT DISTINCT ON (input) input, result FROM analysis_jobs \
         WHERE kind = $1 AND model = $2 AND prompt_version = $3 AND status = 'completed' \
           AND provider = 'llm' AND result IS NOT NULL AND input = ANY($4::jsonb[]) \
         ORDER BY input, finished_at DESC",
    )
    .bind(&spec.kind)
    .bind(&spec.model)
    .bind(&spec.prompt_version)
    .bind(inputs)
    .fetch_all(conn)
    .await?;
    rows.iter()
        .map(|r| Ok((r.try_get("input")?, r.try_get("result")?)))
        .collect()
}

/// Разметка групп координаторами (`mass_problem_workspace.state`).
pub async fn mass_problem_workspace(conn: &mut PgConnection) -> Result<Value, sqlx::Error> {
    sqlx::query_scalar("SELECT state FROM mass_problem_workspace WHERE id = 1")
        .fetch_one(conn)
        .await
}
