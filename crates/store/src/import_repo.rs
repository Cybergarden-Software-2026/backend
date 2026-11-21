//! Доступ к данным для импортёра — порт `app/repositories/import_repository.py` и SQL, который
//! в Python-версии строит ORM при flush (`import_service._apply_plan`).
//!
//! Все функции работают на `&mut PgConnection`: вызывающий открывает транзакцию
//! (`pool.begin()`) и передаёт `&mut *tx` — реестр, история, ChangeEvent, уведомления, журнал
//! `dashboard_events` и состояние источника фиксируются вместе, как в Python.

use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use dashboard_domain::change_detector::StoredIncident;
use dashboard_domain::constants::{
    ChangeType, DashboardEventReason, DashboardEventType, NormalizedState, ResolutionPhase,
};
use dashboard_domain::grouping::ResolutionState;
use dashboard_domain::normalizer::{ColumnValue, Columns, NormalizedIncident};
use serde_json::{Map, Value};
use sqlx::postgres::PgArguments;
use sqlx::query::Query;
use sqlx::{AssertSqlSafe, PgConnection, Postgres, Row};

/// `IMPORT_STATE_ID`: единственная строка `import_state`.
pub const IMPORT_STATE_ID: i16 = 1;

/// Строка `import_state` (модель `ImportState`).
#[derive(Debug, Clone, PartialEq)]
pub struct ImportState {
    pub id: i16,
    pub status: String,
    pub source_path: Option<String>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_change_at: Option<DateTime<Utc>>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub error_since: Option<DateTime<Utc>>,
    pub consecutive_failures: i32,
    pub content_hash: Option<String>,
    pub file_mtime_ns: Option<i64>,
    pub file_size: Option<i64>,
    pub incident_count: Option<i32>,
    pub warnings: Value,
    pub updated_at: DateTime<Utc>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for ImportState {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            status: row.try_get("status")?,
            source_path: row.try_get("source_path")?,
            last_checked_at: row.try_get("last_checked_at")?,
            last_success_at: row.try_get("last_success_at")?,
            last_change_at: row.try_get("last_change_at")?,
            last_error_at: row.try_get("last_error_at")?,
            last_error: row.try_get("last_error")?,
            error_since: row.try_get("error_since")?,
            consecutive_failures: row.try_get("consecutive_failures")?,
            content_hash: row.try_get("content_hash")?,
            file_mtime_ns: row.try_get("file_mtime_ns")?,
            file_size: row.try_get("file_size")?,
            incident_count: row.try_get("incident_count")?,
            warnings: row.try_get("warnings")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

const STATE_COLUMNS: &str = "id, status, source_path, last_checked_at, last_success_at, \
    last_change_at, last_error_at, last_error, error_since, consecutive_failures, content_hash, \
    file_mtime_ns, file_size, incident_count, warnings, updated_at";

/// Строка состояния с блокировкой до конца транзакции (`lock_state`).
pub async fn lock_state(conn: &mut PgConnection) -> Result<ImportState, sqlx::Error> {
    static SELECT: LazyLock<String> =
        LazyLock::new(|| format!("SELECT {STATE_COLUMNS} FROM import_state WHERE id = $1 FOR UPDATE"));
    static INSERT: LazyLock<String> = LazyLock::new(|| {
        format!(
            "INSERT INTO import_state (id, status, warnings) VALUES ($1, 'initializing', '[]'::jsonb) \
             RETURNING {STATE_COLUMNS}"
        )
    });
    let found = sqlx::query_as::<_, ImportState>(AssertSqlSafe(SELECT.as_str()))
        .bind(IMPORT_STATE_ID)
        .fetch_optional(&mut *conn)
        .await?;
    match found {
        Some(state) => Ok(state),
        // Миграция создаёт строку; страховка на случай ручной очистки.
        None => {
            sqlx::query_as::<_, ImportState>(AssertSqlSafe(INSERT.as_str()))
                .bind(IMPORT_STATE_ID)
                .fetch_one(conn)
                .await
        }
    }
}

/// Записывает изменённое состояние (в Python — flush изменённых атрибутов ORM).
pub async fn save_state(conn: &mut PgConnection, s: &ImportState) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE import_state SET status = $2, source_path = $3, last_checked_at = $4, \
         last_success_at = $5, last_change_at = $6, last_error_at = $7, last_error = $8, \
         error_since = $9, consecutive_failures = $10, content_hash = $11, file_mtime_ns = $12, \
         file_size = $13, incident_count = $14, warnings = $15, updated_at = $16 WHERE id = $1",
    )
    .bind(s.id)
    .bind(&s.status)
    .bind(&s.source_path)
    .bind(s.last_checked_at)
    .bind(s.last_success_at)
    .bind(s.last_change_at)
    .bind(s.last_error_at)
    .bind(&s.last_error)
    .bind(s.error_since)
    .bind(s.consecutive_failures)
    .bind(&s.content_hash)
    .bind(s.file_mtime_ns)
    .bind(s.file_size)
    .bind(s.incident_count)
    .bind(&s.warnings)
    .bind(s.updated_at)
    .execute(conn)
    .await?;
    Ok(())
}

/// Файл не изменился: обновляем только время проверки (readAt) — только при статусе `ok`.
pub async fn touch(conn: &mut PgConnection, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE import_state SET last_checked_at = $2, last_success_at = $2, updated_at = $2 \
         WHERE id = $1 AND status = 'ok'",
    )
    .bind(IMPORT_STATE_ID)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

// ------------------------------------------------------------------------------------ реестр

const STORED_COLUMNS: &str = "pk, id, sys_id, raw, content_hash, incident_revision, is_present, normalized_state, \
    priority_id, assigned_group_name, resolution_phase, resolved_detected_at, recently_resolved_until";

pub async fn stored_by_sys_id(
    conn: &mut PgConnection,
    sys_id: &str,
) -> Result<Option<StoredIncident>, sqlx::Error> {
    let sql = format!("SELECT {STORED_COLUMNS} FROM incidents WHERE is_present AND sys_id = $1");
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(sys_id)
        .fetch_optional(conn)
        .await?;
    row.as_ref().map(stored_from_row).transpose()
}

/// Весь реестр (включая выбывшие записи) по возрастанию pk — вход плана импорта.
pub async fn all_incidents(conn: &mut PgConnection) -> Result<Vec<StoredIncident>, sqlx::Error> {
    let sql = format!("SELECT {STORED_COLUMNS} FROM incidents ORDER BY pk");
    let rows = sqlx::query(AssertSqlSafe(sql)).fetch_all(conn).await?;
    rows.iter().map(stored_from_row).collect()
}

fn decode_err(column: &str, message: impl std::fmt::Display) -> sqlx::Error {
    sqlx::Error::ColumnDecode {
        index: column.to_owned(),
        source: message.to_string().into(),
    }
}

fn stored_from_row(row: &sqlx::postgres::PgRow) -> Result<StoredIncident, sqlx::Error> {
    let raw = match row.try_get::<Value, _>("raw")? {
        Value::Object(map) => map,
        other => return Err(decode_err("raw", format!("expected JSON object, got {other}"))),
    };
    let normalized_state: String = row.try_get("normalized_state")?;
    let resolution_phase: Option<String> = row.try_get("resolution_phase")?;
    Ok(StoredIncident {
        pk: row.try_get("pk")?,
        id: row.try_get("id")?,
        sys_id: row.try_get("sys_id")?,
        raw,
        content_hash: row.try_get("content_hash")?,
        incident_revision: i64::from(row.try_get::<i32, _>("incident_revision")?),
        is_present: row.try_get("is_present")?,
        normalized_state: normalized_state
            .parse::<NormalizedState>()
            .map_err(|e| decode_err("normalized_state", e))?,
        priority_id: i64::from(row.try_get::<i16, _>("priority_id")?),
        assigned_group_name: row.try_get("assigned_group_name")?,
        resolution_phase: resolution_phase
            .map(|p| p.parse::<ResolutionPhase>())
            .transpose()
            .map_err(|e| decode_err("resolution_phase", e))?,
        resolved_detected_at: row.try_get("resolved_detected_at")?,
        recently_resolved_until: row.try_get("recently_resolved_until")?,
    })
}

/// Колонки из `Columns` в порядке `FIELDS`, затем вычисленные.
fn column_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Columns::empty().values().into_iter().map(|(n, _)| n).collect();
    names.extend(["normalized_state", "status_mismatch"]);
    names
}

type PgQuery<'q> = Query<'q, Postgres, PgArguments>;

fn bind_columns<'q>(mut q: PgQuery<'q>, columns: &'q Columns) -> PgQuery<'q> {
    for (_, value) in columns.values() {
        // Целые привязываются как int8: PostgreSQL сам приводит к smallint/integer колонки
        // и отвечает ошибкой 22003 при выходе за диапазон (в Python — тоже ошибка, `DBAPIError`).
        q = match value {
            ColumnValue::Text(v) => q.bind(v),
            ColumnValue::Int(v) => q.bind(v),
            ColumnValue::Bool(v) => q.bind(v),
            ColumnValue::Time(v) => q.bind(v),
        };
    }
    q.bind(columns.normalized_state.as_str())
        .bind(columns.status_mismatch)
}

/// Новая запись реестра: `incident_revision = 1`, `first_seen_at = updated_at = now`.
pub async fn insert_incident(
    conn: &mut PgConnection,
    incident: &NormalizedIncident,
    now: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    static SQL: LazyLock<String> = LazyLock::new(|| {
        let names = column_names();
        let n = names.len();
        let params: Vec<String> = (1..=n).map(|i| format!("${i}")).collect();
        format!(
            "INSERT INTO incidents ({}, raw, content_hash, incident_revision, is_present, \
             first_seen_at, updated_at) VALUES ({}, ${}, ${}, 1, true, ${}, ${}) RETURNING pk",
            names.join(", "),
            params.join(", "),
            n + 1,
            n + 2,
            n + 3,
            n + 3,
        )
    });
    let raw = Value::Object(incident.raw.clone());
    bind_columns(sqlx::query(AssertSqlSafe(SQL.as_str())), &incident.columns)
        .bind(raw)
        .bind(&incident.content_hash)
        .bind(now)
        .fetch_one(conn)
        .await?
        .try_get(0)
}

/// Изменённая или вернувшаяся запись: все колонки, исходная запись, хэш, присутствие и
/// (если задана) отметка «Решён».
pub async fn update_incident(
    conn: &mut PgConnection,
    pk: i64,
    incident: &NormalizedIncident,
    resolution: Option<&ResolutionState>,
    incident_revision: i64,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    static SQL: LazyLock<String> = LazyLock::new(|| {
        let names = column_names();
        let n = names.len();
        let sets: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, name)| format!("{name} = ${}", i + 1))
            .collect();
        let p = |k: usize| n + k;
        format!(
            "UPDATE incidents SET {}, raw = ${}, content_hash = ${}, is_present = true, \
             removed_at = NULL, incident_revision = ${}, updated_at = ${}, \
             resolution_phase = CASE WHEN ${} THEN ${} ELSE resolution_phase END, \
             resolved_detected_at = CASE WHEN ${} THEN ${} ELSE resolved_detected_at END, \
             recently_resolved_until = CASE WHEN ${} THEN ${} ELSE recently_resolved_until END \
             WHERE pk = ${}",
            sets.join(", "),
            p(1),
            p(2),
            p(3),
            p(4),
            p(5),
            p(6),
            p(5),
            p(7),
            p(5),
            p(8),
            p(9),
        )
    });
    let raw = Value::Object(incident.raw.clone());
    bind_columns(sqlx::query(AssertSqlSafe(SQL.as_str())), &incident.columns)
        .bind(raw)
        .bind(&incident.content_hash)
        .bind(revision_i32(incident_revision)?)
        .bind(now)
        .bind(resolution.is_some())
        .bind(resolution.and_then(|r| r.phase).map(ResolutionPhase::as_str))
        .bind(resolution.and_then(|r| r.detected_at))
        .bind(resolution.and_then(|r| r.recently_resolved_until))
        .bind(pk)
        .execute(conn)
        .await?;
    Ok(())
}

fn revision_i32(revision: i64) -> Result<i32, sqlx::Error> {
    i32::try_from(revision).map_err(|_| decode_err("incident_revision", "out of integer range"))
}

/// Запись выбыла из снимка: остаётся в реестре с `is_present = false`.
pub async fn mark_removed(
    conn: &mut PgConnection,
    pk: i64,
    incident_revision: i64,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE incidents SET is_present = false, removed_at = $2, incident_revision = $3, \
         updated_at = $2 WHERE pk = $1",
    )
    .bind(pk)
    .bind(now)
    .bind(revision_i32(incident_revision)?)
    .execute(conn)
    .await?;
    Ok(())
}

/// Тихое обновление хэша без новой ревизии (`plan.hash_updates`).
pub async fn update_content_hash(
    conn: &mut PgConnection,
    pk: i64,
    content_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE incidents SET content_hash = $2 WHERE pk = $1")
        .bind(pk)
        .bind(content_hash)
        .execute(conn)
        .await?;
    Ok(())
}

/// Снимает отметку «Решён», у которой истекло окно; возвращает `id` инцидентов.
pub async fn expire_resolutions(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE incidents SET resolution_phase = NULL, recently_resolved_until = NULL \
         WHERE recently_resolved_until IS NOT NULL AND recently_resolved_until <= $1 \
         AND resolution_phase IN ('resolved', 'closed') RETURNING id",
    )
    .bind(now)
    .fetch_all(conn)
    .await
}

// ------------------------------------------------------------------------- журнал и события

/// Новая версия снимка; возвращает её `id` (= revision).
pub async fn new_dashboard_event(
    conn: &mut PgConnection,
    reason: DashboardEventReason,
    payload: &Value,
    now: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO dashboard_events (event_type, reason, payload, created_at) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(DashboardEventType::DashboardChanged.as_str())
    .bind(reason.as_str())
    .bind(payload)
    .bind(now)
    .fetch_one(conn)
    .await
}

/// `json.dumps(None)` — так SQLAlchemy пишет `None` в колонку JSONB (`none_as_null=False`):
/// JSON `null`, а не SQL NULL.
fn json_value(value: Option<&Value>) -> Value {
    value.cloned().unwrap_or(Value::Null)
}

/// Строка `change_events`.
#[derive(Debug, Clone)]
pub struct NewChangeEvent<'a> {
    pub revision: i64,
    pub incident_pk: i64,
    pub incident_revision: i64,
    pub sys_id: &'a str,
    pub incident_id: &'a str,
    pub header: Option<&'a str>,
    pub types: Vec<&'static str>,
    pub fields: Vec<&'a str>,
    pub previous: Option<&'a Map<String, Value>>,
    pub current: Option<&'a Map<String, Value>>,
    pub at: DateTime<Utc>,
}

pub async fn insert_change_event(
    conn: &mut PgConnection,
    ce: &NewChangeEvent<'_>,
) -> Result<i64, sqlx::Error> {
    let previous = json_value(ce.previous.map(|m| Value::Object(m.clone())).as_ref());
    let current = json_value(ce.current.map(|m| Value::Object(m.clone())).as_ref());
    sqlx::query_scalar(
        "INSERT INTO change_events (revision, incident_pk, incident_revision, sys_id, incident_id, \
         header, types, fields, previous, \"current\", at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING id",
    )
    .bind(ce.revision)
    .bind(ce.incident_pk)
    .bind(revision_i32(ce.incident_revision)?)
    .bind(ce.sys_id)
    .bind(ce.incident_id)
    .bind(ce.header)
    .bind(&ce.types)
    .bind(&ce.fields)
    .bind(previous)
    .bind(current)
    .bind(ce.at)
    .fetch_one(conn)
    .await
}

/// Строка пополевой истории `incident_changes`.
#[derive(Debug, Clone, PartialEq)]
pub struct NewIncidentChange {
    pub incident_pk: i64,
    pub change_event_id: Option<i64>,
    pub incident_revision: i64,
    pub change_type: ChangeType,
    pub field: Option<String>,
    pub old_value: Option<Value>,
    pub new_value: Option<Value>,
    pub observed_at: DateTime<Utc>,
}

/// Вставляет строки истории по порядку (id растут в том же порядке, что и в Python).
pub async fn insert_history(conn: &mut PgConnection, rows: &[NewIncidentChange]) -> Result<(), sqlx::Error> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut pks = Vec::with_capacity(rows.len());
    let mut events = Vec::with_capacity(rows.len());
    let mut revisions = Vec::with_capacity(rows.len());
    let mut types = Vec::with_capacity(rows.len());
    let mut fields = Vec::with_capacity(rows.len());
    let mut olds = Vec::with_capacity(rows.len());
    let mut news = Vec::with_capacity(rows.len());
    let mut times = Vec::with_capacity(rows.len());
    for r in rows {
        pks.push(r.incident_pk);
        events.push(r.change_event_id);
        revisions.push(revision_i32(r.incident_revision)?);
        types.push(r.change_type.as_str());
        fields.push(r.field.as_deref());
        olds.push(json_value(r.old_value.as_ref()));
        news.push(json_value(r.new_value.as_ref()));
        times.push(r.observed_at);
    }
    // Один INSERT … SELECT FROM unnest WITH ORDINALITY: порядок id = порядок строк.
    sqlx::query(
        "INSERT INTO incident_changes (incident_pk, change_event_id, incident_revision, change_type, \
         field, old_value, new_value, observed_at) \
         SELECT pk, ev, rev, ct, f, o, n, t FROM unnest($1::bigint[], $2::bigint[], $3::int[], \
         $4::text[], $5::text[], $6::jsonb[], $7::jsonb[], $8::timestamptz[]) \
         WITH ORDINALITY AS u(pk, ev, rev, ct, f, o, n, t, ord) ORDER BY ord",
    )
    .bind(pks)
    .bind(events)
    .bind(revisions)
    .bind(types)
    .bind(fields)
    .bind(olds)
    .bind(news)
    .bind(times)
    .execute(conn)
    .await?;
    Ok(())
}

/// Уведомление координатора (`notifications`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewNotification {
    pub kind: String,
    pub incident_pk: Option<i64>,
    pub incident_id: Option<String>,
    pub sys_id: Option<String>,
    pub incident_revision: Option<i64>,
    pub change_event_id: Option<i64>,
    pub title: String,
    pub body: String,
    pub payload: Map<String, Value>,
    pub dedup_key: String,
    /// `None` — значение по умолчанию сервера (`now()`).
    pub created_at: Option<DateTime<Utc>>,
}

/// Идемпотентно: при повторе `dedup_key` ничего не вставляет и возвращает `None`.
pub async fn add_notification(
    conn: &mut PgConnection,
    n: &NewNotification,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO notifications (type, incident_pk, incident_id, sys_id, incident_revision, \
         change_event_id, title, body, payload, dedup_key, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, coalesce($11, now())) \
         ON CONFLICT (dedup_key) DO NOTHING RETURNING id",
    )
    .bind(&n.kind)
    .bind(n.incident_pk)
    .bind(&n.incident_id)
    .bind(&n.sys_id)
    .bind(n.incident_revision.map(revision_i32).transpose()?)
    .bind(n.change_event_id)
    .bind(&n.title)
    .bind(&n.body)
    .bind(Value::Object(n.payload.clone()))
    .bind(&n.dedup_key)
    .bind(n.created_at)
    .fetch_optional(conn)
    .await
}
