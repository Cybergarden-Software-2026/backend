//! Очередь ИИ-задач в `analysis_jobs`: выжимки карточек (`summary`) и проверка групп массовых
//! сбоев (`cluster`, миграция 0004).
//!
//! Для выжимки вход хранит текущие факты и номер последнего существенного события карточки.
//! Реестр событий дописывается вместе с карточкой, поэтому переход обратно к прежнему статусу
//! отличается от старого состояния даже при совпадении всех текущих полей.

use std::time::Duration;

use chrono::{DateTime, Utc};
use dashboard_domain::llm::{SUMMARY_HISTORY_LIMIT, SummaryEvent};
use serde_json::Value;
use sqlx::{PgConnection, Row};

const STATE_CHANGED: &str = "fields && ARRAY['state', 'stateEnum', 'stateId']";
const MATERIAL_CHANGE: &str =
    "fields && ARRAY['state', 'stateEnum', 'stateId', 'assignedGroupName', 'solution']";

/// Поиск по уникальному индексу (incident_pk, incident_revision), только для этой карточки.
/// Полные предыдущие версии записей и произвольные поля ITSM не передаются в модель.
pub async fn summary_history(
    conn: &mut PgConnection,
    incident_pk: i64,
) -> Result<Vec<SummaryEvent>, sqlx::Error> {
    let sql = format!(
        r#"SELECT at, 'reopened' = ANY(types) AS reopened,
                  CASE WHEN {STATE_CHANGED} THEN left(coalesce(nullif(trim("current"->>'stateEnum'), ''),
                      "current"->>'stateId', nullif(trim("current"->>'state'), '')), 100) END AS state,
                  CASE WHEN 'assignedGroupName' = ANY(fields)
                      THEN left(nullif(trim("current"->>'assignedGroupName'), ''), 150) END AS team,
                  CASE WHEN 'solution' = ANY(fields)
                      THEN left(nullif(trim("current"->>'solution'), ''), 240) END AS solution,
                  CASE WHEN 'solution' = ANY(fields) OR 'reopened' = ANY(types)
                      THEN left(nullif(trim(previous->>'solution'), ''), 240) END AS previous_solution
           FROM change_events WHERE incident_pk = $1 AND {MATERIAL_CHANGE}
           ORDER BY incident_revision DESC LIMIT $2"#
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(incident_pk)
        .bind(SUMMARY_HISTORY_LIMIT as i64)
        .fetch_all(conn)
        .await?;
    rows.iter()
        .rev()
        .map(|row| {
            Ok(SummaryEvent {
                at: row.try_get("at")?,
                reopened: row.try_get("reopened")?,
                state: row.try_get("state")?,
                team: row.try_get("team")?,
                solution: row.try_get("solution")?,
                previous_solution: row.try_get("previous_solution")?,
            })
        })
        .collect()
}

/// Версия входа меняется вслед за текущими фактами и журналом существенных изменений.
/// Дата сериализуется в UTC независимо от настроек клиента PostgreSQL.
fn summary_input_sql() -> String {
    format!(
        r#"jsonb_build_object(
        'header', i.header, 'shortDescription', i.short_description, 'description', i.description,
        'product', i.product_name, 'service', i.service, 'scenario', i.scenario_name,
        'state', i.normalized_state, 'team', i.assigned_group_name, 'solution', i.solution,
        'eliminationType', i.elimination_type,
        'completionDate', to_char(i.completion_date AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
        'historyRevision', (SELECT incident_revision FROM change_events WHERE incident_pk = i.pk
            AND {MATERIAL_CHANGE} ORDER BY incident_revision DESC LIMIT 1))"#
    )
}

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
    enqueue_for(conn, spec, None).await
}

/// Сразу ставит выжимку после назначения команды из интерфейса.
pub async fn enqueue_incident(conn: &mut PgConnection, spec: &JobSpec, pk: i64) -> Result<u64, sqlx::Error> {
    enqueue_for(conn, spec, Some(pk)).await
}

async fn enqueue_for(conn: &mut PgConnection, spec: &JobSpec, pk: Option<i64>) -> Result<u64, sqlx::Error> {
    let input = summary_input_sql();
    let obsolete = format!(
        "UPDATE analysis_jobs j SET status = 'failed', error = '{OBSOLETE}', locked_until = NULL, \
         finished_at = now(), updated_at = now() FROM incidents i \
         WHERE j.incident_pk = i.pk AND j.kind = $1 AND j.status = 'queued' AND ($4::bigint IS NULL OR i.pk = $4) \
           AND (NOT i.is_present OR j.input <> {input} OR j.model <> $2 OR j.prompt_version <> $3)"
    );
    sqlx::query(sqlx::AssertSqlSafe(obsolete))
        .bind(&spec.kind)
        .bind(&spec.model)
        .bind(&spec.prompt_version)
        .bind(pk)
        .execute(&mut *conn)
        .await?;
    let insert = format!(
        "WITH current_inputs AS MATERIALIZED ( \
             SELECT i.pk, i.id, i.sys_id, i.incident_revision, {input} AS input FROM incidents i \
             WHERE i.is_present AND ($5::bigint IS NULL OR i.pk = $5)) \
         INSERT INTO analysis_jobs \
           (incident_pk, incident_id, sys_id, kind, incident_revision, input, model, prompt_version, max_attempts) \
         SELECT i.pk, i.id, i.sys_id, $1, i.incident_revision, i.input, $2, $3, $4 \
         FROM current_inputs i WHERE NOT EXISTS ( \
             SELECT 1 FROM analysis_jobs j WHERE j.incident_pk = i.pk AND j.kind = $1 \
               AND j.model = $2 AND j.prompt_version = $3 AND j.input = i.input \
               AND j.error IS DISTINCT FROM '{OBSOLETE}') \
         ORDER BY i.pk ON CONFLICT DO NOTHING"
    );
    let done = sqlx::query(sqlx::AssertSqlSafe(insert))
        .bind(&spec.kind)
        .bind(&spec.model)
        .bind(&spec.prompt_version)
        .bind(spec.max_attempts)
        .bind(pk)
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
    let sql = format!(
        "SELECT {} = $2 FROM incidents i WHERE i.pk = $1 AND i.is_present",
        summary_input_sql()
    );
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

/// Состояние фоновой выжимки для текущего входа, модели и версии промпта.
#[derive(Debug, Clone)]
pub enum SummaryStatus {
    Ready(StoredResult),
    Pending,
    Unavailable,
}

pub async fn summary_status(
    conn: &mut PgConnection,
    spec: &JobSpec,
    incident_pk: i64,
) -> Result<SummaryStatus, sqlx::Error> {
    let input = summary_input_sql();
    let sql = format!(
        "WITH current_input AS MATERIALIZED (SELECT {input} AS input FROM incidents i WHERE i.pk = $1) \
         SELECT j.result, j.finished_at, j.status = 'completed' AS ready \
         FROM analysis_jobs j CROSS JOIN current_input i \
         WHERE j.incident_pk = $1 AND j.kind = $2 AND j.model = $3 AND j.prompt_version = $4 \
           AND j.input = i.input AND (j.status IN ('queued', 'running') OR ( \
               j.status = 'completed' AND j.provider = 'llm' AND j.finished_at IS NOT NULL \
               AND jsonb_typeof(j.result->'summary') = 'string' AND trim(j.result->>'summary') <> '')) \
         ORDER BY ready DESC, j.finished_at DESC NULLS LAST LIMIT 1"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(incident_pk)
        .bind(&spec.kind)
        .bind(&spec.model)
        .bind(&spec.prompt_version)
        .fetch_optional(conn)
        .await?;
    let Some(row) = row else {
        return Ok(SummaryStatus::Unavailable);
    };
    if row.try_get("ready")? {
        Ok(SummaryStatus::Ready(StoredResult {
            result: row.try_get("result")?,
            finished_at: row.try_get("finished_at")?,
        }))
    } else {
        Ok(SummaryStatus::Pending)
    }
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
