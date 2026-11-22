//! Чтение для API — порт `app/repositories/read_repository.py` и
//! `app/repositories/notifications.py`.
//!
//! Снимок, курсор (revision) и состояние источника читаются в одной транзакции
//! `REPEATABLE READ READ ONLY` ([`begin_consistent_read`]): импортёр пишет реестр и запись журнала
//! вместе, поэтому курсор всегда соответствует прочитанным карточкам и группам.

use chrono::{DateTime, Utc};
use dashboard_domain::constants::{NormalizedState, ResolutionPhase};
use dashboard_domain::projection::{
    ChangeEventRow, CoordinationClaim, IncidentRow, NotificationRow, SourceState,
};
use dashboard_domain::pycompat;
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder, Row, Transaction};

use crate::import_repo::IMPORT_STATE_ID;

/// Транзакция согласованного чтения (`begin_consistent_read`).
pub async fn begin_consistent_read(pool: &PgPool) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    pool.begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
}

/// Ревизия снимка: последний id `dashboard_events`, 0 — журнал пуст.
pub async fn revision(conn: &mut PgConnection) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT coalesce(max(id), 0)::bigint FROM dashboard_events")
        .fetch_one(conn)
        .await
}

pub async fn import_state(conn: &mut PgConnection) -> Result<Option<SourceState>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT status, source_path, last_checked_at, last_success_at, last_change_at, last_error, \
         error_since, incident_count, warnings FROM import_state WHERE id = $1",
    )
    .bind(IMPORT_STATE_ID)
    .fetch_optional(conn)
    .await?;
    row.map(|r| {
        Ok(SourceState {
            status: r.try_get("status")?,
            source_path: r.try_get("source_path")?,
            last_checked_at: r.try_get("last_checked_at")?,
            last_success_at: r.try_get("last_success_at")?,
            last_change_at: r.try_get("last_change_at")?,
            last_error: r.try_get("last_error")?,
            error_since: r.try_get("error_since")?,
            incident_count: r.try_get("incident_count")?,
            warnings: r.try_get("warnings")?,
        })
    })
    .transpose()
}

// ------------------------------------------------------------------ инциденты

const INCIDENT_COLUMNS: &str = "pk, id, sys_id, header, description, short_description, solution, \
    product_name, product_id, product_master_id, product_url, service, service_id, scenario_name, \
    service_type, service_type_id, inc_type, receiving_channel, state_label, state_enum, state_id, \
    state_reason, state_reason_id, normalized_state, status_mismatch, priority_id, priority, \
    urgency_id, urgency, impact_id, impact, assigned_group_name, assigned_employee, \
    assigned_employee_login, initiator_name, initiator_login, submit_date, deadline, completion_date, \
    last_assignment_to_team_at, waiting_exit_date, is_effect, is_reason, is_double, is_master, \
    is_root_incident, is_need_up, elimination_type, elimination_type_id, raw, incident_revision, \
    is_present, removed_at, resolution_phase, resolved_detected_at, recently_resolved_until, \
    first_seen_at, updated_at, a.login AS claim_login, a.display_name AS claim_display_name, c.claimed_at";

const INCIDENT_WITH_CLAIM: &str = "incidents i
    LEFT JOIN incident_coordination_claims c ON c.incident_pk = i.pk
    LEFT JOIN coordinator_accounts a ON a.login = c.account_login AND a.disabled_at IS NULL";

pub fn coordination_claim_from_row(row: &PgRow) -> Result<Option<CoordinationClaim>, sqlx::Error> {
    let Some(login) = row.try_get("claim_login")? else {
        return Ok(None);
    };
    Ok(Some(CoordinationClaim {
        login,
        display_name: row.try_get("claim_display_name")?,
        claimed_at: row.try_get("claimed_at")?,
    }))
}

pub async fn coordination_claim(
    conn: &mut PgConnection,
    incident_pk: i64,
) -> Result<Option<CoordinationClaim>, sqlx::Error> {
    sqlx::query(
        "SELECT a.login AS claim_login, a.display_name AS claim_display_name, c.claimed_at
         FROM incident_coordination_claims c JOIN coordinator_accounts a ON a.login = c.account_login
         WHERE c.incident_pk = $1 AND a.disabled_at IS NULL",
    )
    .bind(incident_pk)
    .try_map(|row| coordination_claim_from_row(&row))
    .fetch_optional(conn)
    .await
    .map(Option::flatten)
}

fn decode_err(column: &str, message: impl std::fmt::Display) -> sqlx::Error {
    sqlx::Error::ColumnDecode {
        index: column.to_owned(),
        source: message.to_string().into(),
    }
}

fn incident_from_row(r: &PgRow) -> Result<IncidentRow, sqlx::Error> {
    let raw = match r.try_get::<Value, _>("raw")? {
        Value::Object(map) => map,
        other => return Err(decode_err("raw", format!("expected JSON object, got {other}"))),
    };
    let state: String = r.try_get("normalized_state")?;
    let phase: Option<String> = r.try_get("resolution_phase")?;
    Ok(IncidentRow {
        pk: r.try_get("pk")?,
        id: r.try_get("id")?,
        sys_id: r.try_get("sys_id")?,
        header: r.try_get("header")?,
        description: r.try_get("description")?,
        short_description: r.try_get("short_description")?,
        solution: r.try_get("solution")?,
        product_name: r.try_get("product_name")?,
        product_id: r.try_get("product_id")?,
        product_master_id: r.try_get("product_master_id")?,
        product_url: r.try_get("product_url")?,
        service: r.try_get("service")?,
        service_id: r.try_get("service_id")?,
        scenario_name: r.try_get("scenario_name")?,
        service_type: r.try_get("service_type")?,
        service_type_id: r.try_get("service_type_id")?,
        inc_type: r.try_get("inc_type")?,
        receiving_channel: r.try_get("receiving_channel")?,
        state_label: r.try_get("state_label")?,
        state_enum: r.try_get("state_enum")?,
        state_id: r.try_get("state_id")?,
        state_reason: r.try_get("state_reason")?,
        state_reason_id: r.try_get("state_reason_id")?,
        normalized_state: state
            .parse::<NormalizedState>()
            .map_err(|e| decode_err("normalized_state", e))?,
        status_mismatch: r.try_get("status_mismatch")?,
        priority_id: r.try_get("priority_id")?,
        priority: r.try_get("priority")?,
        urgency_id: r.try_get("urgency_id")?,
        urgency: r.try_get("urgency")?,
        impact_id: r.try_get("impact_id")?,
        impact: r.try_get("impact")?,
        assigned_group_name: r.try_get("assigned_group_name")?,
        assigned_employee: r.try_get("assigned_employee")?,
        assigned_employee_login: r.try_get("assigned_employee_login")?,
        coordination_claim: coordination_claim_from_row(r)?,
        initiator_name: r.try_get("initiator_name")?,
        initiator_login: r.try_get("initiator_login")?,
        submit_date: r.try_get("submit_date")?,
        deadline: r.try_get("deadline")?,
        completion_date: r.try_get("completion_date")?,
        last_assignment_to_team_at: r.try_get("last_assignment_to_team_at")?,
        waiting_exit_date: r.try_get("waiting_exit_date")?,
        is_effect: r.try_get("is_effect")?,
        is_reason: r.try_get("is_reason")?,
        is_double: r.try_get("is_double")?,
        is_master: r.try_get("is_master")?,
        is_root_incident: r.try_get("is_root_incident")?,
        is_need_up: r.try_get("is_need_up")?,
        elimination_type: r.try_get("elimination_type")?,
        elimination_type_id: r.try_get("elimination_type_id")?,
        raw,
        incident_revision: r.try_get("incident_revision")?,
        is_present: r.try_get("is_present")?,
        removed_at: r.try_get("removed_at")?,
        resolution_phase: phase
            .map(|p| p.parse::<ResolutionPhase>())
            .transpose()
            .map_err(|e| decode_err("resolution_phase", e))?,
        resolved_detected_at: r.try_get("resolved_detected_at")?,
        recently_resolved_until: r.try_get("recently_resolved_until")?,
        first_seen_at: r.try_get("first_seen_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

/// Присутствующие в выгрузке, `ORDER BY id` (порядок — по правилам сортировки самой БД, как у
/// Python-версии: обе версии читают одну базу одним и тем же запросом).
pub async fn present_incidents(conn: &mut PgConnection) -> Result<Vec<IncidentRow>, sqlx::Error> {
    let sql = format!("SELECT {INCIDENT_COLUMNS} FROM {INCIDENT_WITH_CLAIM} WHERE is_present ORDER BY id");
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(conn).await?;
    rows.iter().map(incident_from_row).collect()
}

/// Как ищется ключ `/incidents/{key}` (`incident_by_key`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncidentKey {
    /// `^INC\d+$` (без учёта регистра) → `id = key.upper()`.
    Number(String),
    /// `^\d+$` → `sys_id = key`.
    SysId(String),
    /// Иначе — оба поля.
    Either(String),
}

/// Буква образца `re.IGNORECASE` Python: `I` совпадает и с `İ`/`ı`.
fn ignorecase_eq(c: char, pattern: char) -> bool {
    match pattern {
        'I' => matches!(c, 'I' | 'i' | 'İ' | 'ı'),
        p => c.eq_ignore_ascii_case(&p),
    }
}

impl IncidentKey {
    /// `key.strip()`, затем регулярные выражения `INCIDENT_NUMBER` / `SYS_ID` (`\d` — любые
    /// десятичные цифры Unicode, как в `re` Python).
    pub fn parse(key: &str) -> Self {
        let key = pycompat::strip(key);
        let all_digits = |s: &str| !s.is_empty() && s.chars().all(|c| pycompat::decimal_digit(c).is_some());
        let mut chars = key.chars();
        let prefix_ok = "INC"
            .chars()
            .all(|p| chars.next().is_some_and(|c| ignorecase_eq(c, p)));
        if prefix_ok && all_digits(chars.as_str()) {
            return Self::Number(key.to_uppercase());
        }
        if all_digits(key) {
            return Self::SysId(key.to_owned());
        }
        Self::Either(key.to_owned())
    }
}

pub async fn incident_by_key(conn: &mut PgConnection, key: &str) -> Result<Option<IncidentRow>, sqlx::Error> {
    let (condition, value) = match IncidentKey::parse(key) {
        IncidentKey::Number(v) => ("id = $1", v),
        IncidentKey::SysId(v) => ("sys_id = $1", v),
        IncidentKey::Either(v) => ("id = $1 OR sys_id = $1", v),
    };
    let sql = format!("SELECT {INCIDENT_COLUMNS} FROM {INCIDENT_WITH_CLAIM} WHERE {condition} LIMIT 1");
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(value)
        .fetch_optional(conn)
        .await?;
    row.as_ref().map(incident_from_row).transpose()
}

// ------------------------------------------------------------------ журнал ChangeEvent

#[derive(Debug, Clone, Default)]
pub struct ChangeEventFilter<'a> {
    pub limit: i64,
    pub before_id: Option<i64>,
    pub after_id: Option<i64>,
    pub event_type: Option<&'a str>,
    pub incident_pk: Option<i64>,
    pub sys_id: Option<&'a str>,
    pub revision: Option<i64>,
}

/// Новые первыми.
pub async fn change_events(
    conn: &mut PgConnection,
    f: &ChangeEventFilter<'_>,
) -> Result<Vec<ChangeEventRow>, sqlx::Error> {
    let mut q = QueryBuilder::<Postgres>::new(
        "SELECT id, revision, incident_pk, incident_revision, sys_id, incident_id, header, types, \
         fields, at, previous, \"current\", CASE WHEN 'team_contacted' = ANY (types) THEN \
         (SELECT payload->'contact' FROM dashboard_events WHERE id = change_events.revision) END AS contact \
         FROM change_events WHERE true",
    );
    if let Some(v) = f.before_id {
        q.push(" AND id < ").push_bind(v);
    }
    if let Some(v) = f.after_id {
        q.push(" AND id > ").push_bind(v);
    }
    if let Some(v) = f.event_type {
        q.push(" AND ").push_bind(v.to_owned()).push(" = ANY (types)");
    }
    if let Some(v) = f.incident_pk {
        q.push(" AND incident_pk = ").push_bind(v);
    }
    if let Some(v) = f.sys_id {
        q.push(" AND sys_id = ").push_bind(v.to_owned());
    }
    if let Some(v) = f.revision {
        q.push(" AND revision = ").push_bind(v);
    }
    q.push(" ORDER BY id DESC LIMIT ").push_bind(f.limit);
    let rows = q.build().fetch_all(conn).await?;
    rows.iter()
        .map(|r| {
            Ok(ChangeEventRow {
                id: r.try_get("id")?,
                revision: r.try_get("revision")?,
                incident_pk: r.try_get("incident_pk")?,
                incident_revision: r.try_get("incident_revision")?,
                sys_id: r.try_get("sys_id")?,
                incident_id: r.try_get("incident_id")?,
                header: r.try_get("header")?,
                types: r.try_get("types")?,
                fields: r.try_get("fields")?,
                at: r.try_get("at")?,
                previous: r.try_get("previous")?,
                current: r.try_get("current")?,
                contact: r.try_get("contact")?,
            })
        })
        .collect()
}

// ------------------------------------------------------------------ уведомления

/// Новые первыми; keyset-пагинация по id, `read_at` — отметка пользователя `user_id`.
pub async fn notifications_page(
    conn: &mut PgConnection,
    user_id: &str,
    before_id: Option<i64>,
    limit: i64,
) -> Result<Vec<NotificationRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT n.id, n.type, n.title, n.body, n.incident_id, n.sys_id, n.incident_revision, \
         n.change_event_id, n.payload, n.created_at, r.read_at \
         FROM notifications n LEFT OUTER JOIN notification_reads r \
           ON r.notification_id = n.id AND r.user_id = $1 \
         WHERE ($2::bigint IS NULL OR n.id < $2) ORDER BY n.id DESC LIMIT $3",
    )
    .bind(user_id)
    .bind(before_id)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(NotificationRow {
                id: r.try_get("id")?,
                r#type: r.try_get("type")?,
                title: r.try_get("title")?,
                body: r.try_get("body")?,
                incident_id: r.try_get("incident_id")?,
                sys_id: r.try_get("sys_id")?,
                incident_revision: r.try_get("incident_revision")?,
                change_event_id: r.try_get("change_event_id")?,
                payload: r.try_get("payload")?,
                created_at: r.try_get("created_at")?,
                read_at: r.try_get::<Option<DateTime<Utc>>, _>("read_at")?,
            })
        })
        .collect()
}

pub async fn unread_count(conn: &mut PgConnection, user_id: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM notifications n WHERE NOT EXISTS \
         (SELECT 1 FROM notification_reads r WHERE r.notification_id = n.id AND r.user_id = $1)",
    )
    .bind(user_id)
    .fetch_one(conn)
    .await
}

pub async fn notification_exists(conn: &mut PgConnection, id: i64) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT id FROM notifications WHERE id = $1")
            .bind(id)
            .fetch_optional(conn)
            .await?
            .is_some(),
    )
}

/// Идемпотентно: повторная отметка не меняет исходное время прочтения.
pub async fn mark_read(
    conn: &mut PgConnection,
    id: i64,
    user_id: &str,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO notification_reads (notification_id, user_id, read_at) VALUES ($1, $2, $3) \
         ON CONFLICT (notification_id, user_id) DO NOTHING",
    )
    .bind(id)
    .bind(user_id)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::IncidentKey as K;

    #[test]
    fn key_like_python_regexes() {
        assert_eq!(
            K::parse(" inc000021938167\n"),
            K::Number("INC000021938167".into())
        );
        assert_eq!(K::parse("ınc1"), K::Number("INC1".into()));
        assert_eq!(K::parse("İnc1"), K::Number("İNC1".into()));
        assert_eq!(K::parse("inc\u{661}"), K::Number("INC\u{661}".into()));
        assert_eq!(
            K::parse("179429233402440192"),
            K::SysId("179429233402440192".into())
        );
        assert_eq!(K::parse("INC"), K::Either("INC".into()));
        assert_eq!(K::parse("ｉnc1"), K::Either("ｉnc1".into()));
        assert_eq!(K::parse("nonsense"), K::Either("nonsense".into()));
    }
}
