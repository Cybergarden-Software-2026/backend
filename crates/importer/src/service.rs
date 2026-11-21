//! Применение снимка к PostgreSQL — порт `app/services/import_service.py`.
//!
//! Каждый вызов [`ImportService::process`] — одна транзакция: реестр, история, ChangeEvent,
//! уведомления, запись журнала `dashboard_events` (новая revision) и состояние источника
//! фиксируются вместе. При ошибке файла реестр не трогается — меняется только `import_state`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, SubsecRound, TimeDelta, Utc};
use dashboard_config::Settings;
use dashboard_domain::change_detector::{ImportPlan, IncidentPlan, PlanOptions, build_import_plan};
use dashboard_domain::constants::{ChangeType, DashboardEventReason, NotificationType, SourceStatus};
use dashboard_domain::validator::{ValidatedSnapshot, parse_and_validate};
use dashboard_store::import_repo::{
    self as repo, ImportState, NewChangeEvent, NewIncidentChange, NewNotification,
};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};

use crate::reader::{FileFingerprint, ReadResult};

/// Итог обработки (`OutcomeStatus` Python-версии).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeStatus {
    Unchanged,
    Applied,
    NoChanges,
    Rejected,
    ReadFailed,
}

impl OutcomeStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Applied => "applied",
            Self::NoChanges => "no_changes",
            Self::Rejected => "rejected",
            Self::ReadFailed => "read_failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOutcome {
    pub status: OutcomeStatus,
    pub revision: Option<i64>,
    pub created: usize,
    pub updated: usize,
    pub removed: usize,
    pub restored: usize,
    pub message: Option<String>,
    pub source_status: Option<SourceStatus>,
}

impl ImportOutcome {
    pub fn new(status: OutcomeStatus) -> Self {
        Self {
            status,
            revision: None,
            created: 0,
            updated: 0,
            removed: 0,
            restored: 0,
            message: None,
            source_status: None,
        }
    }

    /// Снимок принят (файл не изменился, применён или изменений нет).
    pub fn accepted(&self) -> bool {
        matches!(
            self.status,
            OutcomeStatus::Unchanged | OutcomeStatus::Applied | OutcomeStatus::NoChanges
        )
    }
}

/// Часы сервиса (в тестах — управляемые).
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Текущее время UTC с точностью PostgreSQL (микросекунды), как `datetime.now(UTC)`.
pub fn utcnow() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(6)
}

/// `datetime.isoformat()` для UTC: `2026-11-20T12:00:00+00:00`, микросекунды — если не ноль.
pub fn py_isoformat(value: &DateTime<Utc>) -> String {
    let micros = value.timestamp_subsec_micros();
    if micros == 0 {
        value.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        format!("{}.{micros:06}+00:00", value.format("%Y-%m-%dT%H:%M:%S"))
    }
}

/// `message[:n]` — по символам, как срез строки Python.
fn truncate_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((pos, _)) => &s[..pos],
        None => s,
    }
}

/// `str(Path)`: без `.`-компонентов и повторных/концевых `/`.
pub fn py_path_str(path: &Path) -> String {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for c in path.components() {
        if c != Component::CurDir {
            out.push(c.as_os_str());
        }
    }
    let s = out.to_string_lossy().into_owned();
    if s.is_empty() { ".".to_owned() } else { s }
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Task(String),
}

impl ImportError {
    /// Имя класса исключения, которое увидела бы Python-версия (`type(exc).__name__`).
    ///
    /// Диалект asyncpg в SQLAlchemy переводит ошибки сервера по иерархии asyncpg
    /// (`_asyncpg_error_translate`): нарушение ограничений (класс SQLSTATE 23) — `IntegrityError`,
    /// синтаксис и доступ (42) — `ProgrammingError`, внутренние (XX) — `InternalError`, всё прочее,
    /// включая 22xxx (например, значение вне диапазона smallint), — `DBAPIError`. Проверено на
    /// настоящем Python в `import_parity.rs`.
    pub fn python_type_name(&self) -> &'static str {
        match self {
            Self::Task(_) => "RuntimeError",
            Self::Database(sqlx::Error::Database(db)) => {
                match db.code().as_deref().map(|c| c.get(..2).unwrap_or(c)) {
                    Some("23") => "IntegrityError",
                    Some("42") => "ProgrammingError",
                    Some("XX") => "InternalError",
                    _ => "DBAPIError",
                }
            }
            Self::Database(sqlx::Error::Io(e)) => match e.kind() {
                std::io::ErrorKind::ConnectionRefused => "ConnectionRefusedError",
                std::io::ErrorKind::ConnectionReset => "ConnectionResetError",
                std::io::ErrorKind::TimedOut => "TimeoutError",
                _ => "OSError",
            },
            Self::Database(sqlx::Error::PoolTimedOut) => "TimeoutError",
            Self::Database(_) => "InterfaceError",
        }
    }
}

pub struct ImportService {
    pool: PgPool,
    settings: Arc<Settings>,
    clock: Clock,
}

impl ImportService {
    pub fn new(pool: PgPool, settings: Arc<Settings>) -> Self {
        Self::with_clock(pool, settings, Arc::new(utcnow))
    }

    pub fn with_clock(pool: PgPool, settings: Arc<Settings>, clock: Clock) -> Self {
        Self {
            pool,
            settings,
            clock,
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    // ============================================================== точка входа

    pub async fn process(&self, result: ReadResult) -> Result<ImportOutcome, ImportError> {
        match result {
            ReadResult::Unchanged { .. } => self.touch().await,
            ReadResult::Failed { message, .. } => {
                self.record_failure(&message, OutcomeStatus::ReadFailed).await
            }
            ReadResult::Ok {
                content,
                sha256,
                fingerprint,
            } => self.process_content(content, sha256, fingerprint).await,
        }
    }

    pub async fn touch(&self) -> Result<ImportOutcome, ImportError> {
        let mut tx = self.pool.begin().await?;
        repo::touch(&mut tx, self.now()).await?;
        tx.commit().await?;
        Ok(ImportOutcome::new(OutcomeStatus::Unchanged))
    }

    /// Снимок не принят: счётчик ошибок, после порога — состояние `error`, новая ревизия и
    /// уведомление `source.error` (один раз на эпизод).
    pub async fn record_failure(
        &self,
        message: &str,
        status: OutcomeStatus,
    ) -> Result<ImportOutcome, ImportError> {
        let now = self.now();
        let mut tx = self.pool.begin().await?;
        let mut state = repo::lock_state(&mut tx).await?;
        state.last_checked_at = Some(now);
        state.last_error_at = Some(now);
        state.last_error = Some(truncate_chars(message, 4000).to_owned());
        state.consecutive_failures += 1;
        state.source_path = Some(py_path_str(&self.settings.json_source_path));
        state.updated_at = now;
        let mut revision = None;
        let threshold = i64::from(self.settings.import_error_threshold);
        if state.status != SourceStatus::Error.as_str() && i64::from(state.consecutive_failures) >= threshold
        {
            state.status = SourceStatus::Error.as_str().to_owned();
            state.error_since = Some(now);
            revision = Some(
                repo::new_dashboard_event(
                    &mut tx,
                    DashboardEventReason::Source,
                    &json!({"schemaVersion": 1, "sourceStatus": "error"}),
                    now,
                )
                .await?,
            );
            let mut payload = serde_json::Map::new();
            payload.insert(
                "error".into(),
                Value::String(truncate_chars(message, 1000).to_owned()),
            );
            repo::add_notification(
                &mut tx,
                &NewNotification {
                    kind: NotificationType::SourceError.as_str().to_owned(),
                    title: "Проблема с источником данных".to_owned(),
                    body: format!(
                        "{}. Показан последний принятый снимок.",
                        truncate_chars(message, 500)
                    ),
                    payload,
                    dedup_key: format!("source.error:{}", py_isoformat(&now)),
                    created_at: Some(now),
                    ..NewNotification::default()
                },
            )
            .await?;
            tracing::warn!("Источник переведён в состояние error: {message}");
        } else {
            tracing::info!(
                "Снимок не принят ({} подряд): {message}",
                state.consecutive_failures
            );
        }
        repo::save_state(&mut tx, &state).await?;
        tx.commit().await?;
        let mut outcome = ImportOutcome::new(status);
        outcome.revision = revision;
        outcome.message = Some(message.to_owned());
        outcome.source_status = state.status.parse().ok();
        Ok(outcome)
    }

    // ============================================================== корректный файл

    async fn process_content(
        &self,
        content: Vec<u8>,
        sha256: String,
        fingerprint: FileFingerprint,
    ) -> Result<ImportOutcome, ImportError> {
        let tz = self.settings.source_naive_timezone;
        let validated = tokio::task::spawn_blocking(move || parse_and_validate(&content, tz))
            .await
            .map_err(|e| ImportError::Task(e.to_string()))?;
        match validated {
            Ok(validated) => self.apply(validated, &sha256, fingerprint).await,
            Err(rejected) => {
                self.record_failure(&rejected.summary(), OutcomeStatus::Rejected)
                    .await
            }
        }
    }

    pub async fn apply(
        &self,
        mut validated: ValidatedSnapshot,
        sha256: &str,
        fingerprint: FileFingerprint,
    ) -> Result<ImportOutcome, ImportError> {
        let now = self.now();
        let mut tx = self.pool.begin().await?;
        let mut state = repo::lock_state(&mut tx).await?;
        let rows = repo::all_incidents(&mut tx).await?;
        let is_initial = state.last_success_at.is_none() && rows.is_empty();
        crate::assignment::reconcile_source(
            &mut tx,
            &mut validated.incidents,
            self.settings.source_naive_timezone,
        )
        .await?;
        let incident_count = validated.incidents.len();
        let warnings = validated.warnings;

        let options = PlanOptions {
            is_initial,
            naive_tz: self.settings.source_naive_timezone,
            coordinator_group: self.settings.coordinator_group_name.clone(),
            now: Some(now),
            resolved_window: TimeDelta::hours(i64::from(self.settings.critical_resolved_window_hours)),
        };
        let plan = build_import_plan(&rows, validated.incidents, &options);
        let was_error = state.status == SourceStatus::Error.as_str();
        let mut revision = None;
        if plan.has_changes() || was_error || is_initial {
            let reason = if plan.has_changes() || is_initial {
                DashboardEventReason::Import
            } else {
                DashboardEventReason::Source
            };
            let rev = repo::new_dashboard_event(&mut tx, reason, &event_payload(&plan), now).await?;
            revision = Some(rev);
            apply_plan(&mut tx, &plan, rev, now).await?;
        }
        for (pk, content_hash) in &plan.hash_updates {
            repo::update_content_hash(&mut tx, *pk, content_hash).await?;
        }
        if self.settings.llm_enabled && revision.is_some() {
            // В той же транзакции, что и новая ревизия: изменение не останется без задачи.
            let spec = dashboard_store::analysis_repo::JobSpec::summary(&self.settings);
            let queued = dashboard_store::analysis_repo::enqueue(&mut tx, &spec).await?;
            if queued > 0 {
                tracing::info!(queued, "Поставлены задачи ИИ-анализа");
            }
        }

        self.mark_success(
            &mut tx,
            &mut state,
            sha256,
            fingerprint,
            incident_count,
            warnings,
            now,
            plan.has_changes(),
        )
        .await?;
        tx.commit().await?;

        let outcome = ImportOutcome {
            status: if plan.has_changes() {
                OutcomeStatus::Applied
            } else {
                OutcomeStatus::NoChanges
            },
            revision,
            created: plan.count(ChangeType::Created),
            updated: plan.count(ChangeType::Updated),
            removed: plan.count(ChangeType::Removed),
            restored: plan.count(ChangeType::Restored),
            message: None,
            source_status: Some(SourceStatus::Ok),
        };
        if plan.has_changes() {
            tracing::info!(
                "Снимок принят: revision={} created={} updated={} removed={} restored={}{}",
                revision.map_or_else(|| "None".to_owned(), |r| r.to_string()),
                outcome.created,
                outcome.updated,
                outcome.removed,
                outcome.restored,
                if is_initial {
                    " (первичная загрузка)"
                } else {
                    ""
                },
            );
        }
        Ok(outcome)
    }

    /// Снимает отметку «Решён», когда истекло окно 24 ч, и публикует новую ревизию снимка.
    ///
    /// Данные выгрузки не менялись, поэтому `incidentRevision` не увеличивается.
    /// Возвращает revision, если что-то истекло.
    pub async fn expire_resolutions(&self) -> Result<Option<i64>, ImportError> {
        let now = self.now();
        let mut tx = self.pool.begin().await?;
        repo::lock_state(&mut tx).await?;
        let expired = repo::expire_resolutions(&mut tx, now).await?;
        if expired.is_empty() {
            tx.commit().await?;
            return Ok(None);
        }
        let revision = repo::new_dashboard_event(
            &mut tx,
            DashboardEventReason::Expiry,
            &json!({"schemaVersion": 1, "expired": expired}),
            now,
        )
        .await?;
        tx.commit().await?;
        tracing::info!("Окно «Решён» истекло: {}", expired.join(", "));
        Ok(Some(revision))
    }

    #[allow(clippy::too_many_arguments)]
    async fn mark_success(
        &self,
        conn: &mut PgConnection,
        state: &mut ImportState,
        sha256: &str,
        fingerprint: FileFingerprint,
        incident_count: usize,
        warnings: Vec<String>,
        now: DateTime<Utc>,
        data_changed: bool,
    ) -> Result<(), ImportError> {
        if state.status == SourceStatus::Error.as_str() {
            let mut payload = serde_json::Map::new();
            payload.insert(
                "errorSince".into(),
                state
                    .error_since
                    .as_ref()
                    .map_or(Value::Null, |t| Value::String(py_isoformat(t))),
            );
            repo::add_notification(
                conn,
                &NewNotification {
                    kind: NotificationType::SourceRecovered.as_str().to_owned(),
                    title: "Источник данных восстановлен".to_owned(),
                    body: "Снимок снова принимается, дашборд обновляется.".to_owned(),
                    payload,
                    dedup_key: format!(
                        "source.recovered:{}",
                        py_isoformat(&state.error_since.unwrap_or(now))
                    ),
                    created_at: Some(now),
                    ..NewNotification::default()
                },
            )
            .await?;
            tracing::info!("Источник восстановлен");
        }
        state.status = SourceStatus::Ok.as_str().to_owned();
        state.error_since = None;
        state.consecutive_failures = 0;
        state.last_checked_at = Some(now);
        state.last_success_at = Some(now);
        if data_changed || state.last_change_at.is_none() {
            state.last_change_at = Some(now);
        }
        state.content_hash = Some(sha256.to_owned());
        state.file_mtime_ns = Some(fingerprint.mtime_ns);
        state.file_size = Some(i64::try_from(fingerprint.size).unwrap_or(i64::MAX));
        state.incident_count = Some(i32::try_from(incident_count).unwrap_or(i32::MAX));
        state.warnings = Value::Array(warnings.into_iter().map(Value::String).collect());
        state.source_path = Some(py_path_str(&self.settings.json_source_path));
        state.updated_at = now;
        repo::save_state(conn, state).await?;
        Ok(())
    }
}

fn event_payload(plan: &ImportPlan) -> Value {
    json!({
        "schemaVersion": 1,
        "initial": plan.is_initial,
        "created": plan.count(ChangeType::Created),
        "updated": plan.count(ChangeType::Updated),
        "removed": plan.count(ChangeType::Removed),
        "restored": plan.count(ChangeType::Restored),
    })
}

/// `_apply_plan`: реестр → лента ChangeEvent → пополевая история → уведомления.
pub(crate) async fn apply_plan(
    conn: &mut PgConnection,
    plan: &ImportPlan,
    revision: i64,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    // 1. Реестр. SQLAlchemy при flush выполняет UPDATE раньше INSERT — тот же порядок здесь.
    let mut pks: HashMap<usize, i64> = HashMap::new();
    for (index, item) in plan.items.iter().enumerate() {
        let Some(pk) = item.pk else { continue };
        match item.kind {
            ChangeType::Removed => repo::mark_removed(conn, pk, item.new_revision, now).await?,
            _ => {
                let current = item.current.as_ref().expect("updated/restored has current");
                repo::update_incident(
                    conn,
                    pk,
                    current,
                    item.resolution.as_ref(),
                    item.new_revision,
                    now,
                )
                .await?;
            }
        }
        pks.insert(index, pk);
    }
    for (index, item) in plan.items.iter().enumerate() {
        if item.kind == ChangeType::Created {
            let current = item.current.as_ref().expect("created has current");
            pks.insert(index, repo::insert_incident(conn, current, now).await?);
        }
    }

    // 2. Лента ChangeEvent (первичная загрузка архива событий не создаёт).
    let mut change_events: Vec<Option<i64>> = Vec::with_capacity(plan.items.len());
    for (index, item) in plan.items.iter().enumerate() {
        if plan.is_initial {
            change_events.push(None);
            continue;
        }
        let fields = if matches!(item.kind, ChangeType::Updated | ChangeType::Restored) {
            item.changed_fields()
        } else {
            Vec::new()
        };
        let id = repo::insert_change_event(
            conn,
            &NewChangeEvent {
                revision,
                incident_pk: pks[&index],
                incident_revision: item.new_revision,
                sys_id: &item.sys_id,
                incident_id: &item.incident_id,
                header: item.header(),
                types: item.event_types.iter().map(|t| t.as_str()).collect(),
                fields,
                previous: item.previous_raw.as_ref(),
                current: item.current.as_ref().map(|c| &c.raw),
                at: now,
            },
        )
        .await?;
        change_events.push(Some(id));
    }

    // 3. Пополевая история.
    let mut history = Vec::new();
    for (index, item) in plan.items.iter().enumerate() {
        history.extend(history_rows(item, pks[&index], change_events[index], now));
    }
    repo::insert_history(conn, &history).await?;

    // 4. Уведомления координатора (идемпотентно по dedup_key).
    for (index, item) in plan.items.iter().enumerate() {
        let ce = change_events[index];
        for draft in &item.notifications {
            let mut payload = draft.payload.clone();
            payload.insert("changeEventId".into(), ce.map_or(Value::Null, Value::from));
            repo::add_notification(
                conn,
                &NewNotification {
                    kind: draft.kind.as_str().to_owned(),
                    incident_pk: Some(pks[&index]),
                    incident_id: Some(item.incident_id.clone()),
                    sys_id: Some(item.sys_id.clone()),
                    incident_revision: Some(item.new_revision),
                    change_event_id: ce,
                    title: draft.title.clone(),
                    body: draft.body.clone(),
                    payload,
                    dedup_key: draft.dedup_key.clone(),
                    created_at: Some(now),
                },
            )
            .await?;
        }
    }
    Ok(())
}

fn history_rows(
    item: &IncidentPlan,
    incident_pk: i64,
    change_event_id: Option<i64>,
    now: DateTime<Utc>,
) -> Vec<NewIncidentChange> {
    let row = |change_type: ChangeType, field: Option<String>, old: Option<Value>, new: Option<Value>| {
        NewIncidentChange {
            incident_pk,
            change_event_id,
            incident_revision: item.new_revision,
            change_type,
            field,
            old_value: old,
            new_value: new,
            observed_at: now,
        }
    };
    let current_raw = || item.current.as_ref().map(|c| Value::Object(c.raw.clone()));
    match item.kind {
        ChangeType::Created => vec![row(ChangeType::Created, None, None, current_raw())],
        ChangeType::Removed => vec![row(
            ChangeType::Removed,
            None,
            item.previous_raw.clone().map(Value::Object),
            None,
        )],
        ChangeType::Updated | ChangeType::Restored => {
            let mut rows = Vec::new();
            if item.kind == ChangeType::Restored {
                rows.push(row(ChangeType::Restored, None, None, current_raw()));
            }
            rows.extend(item.changes.iter().map(|c| {
                row(
                    ChangeType::Updated,
                    Some(c.field.clone()),
                    Some(c.old.clone()),
                    Some(c.new.clone()),
                )
            }));
            rows
        }
    }
}
