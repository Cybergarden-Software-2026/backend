//! Снимок дашборда, инциденты и журнал — порт `app/services/dashboard_service.py` и маршрутов
//! `app/api/v1/{snapshot, dashboard, incidents, events}.py`.
//!
//! Порядок проверок как в Python: ошибки параметров (FastAPI, 400) → «первый снимок ещё не
//! принят» (503) → ошибки сервиса (404, 400 `INVALID_SORT`/`INVALID_CURSOR`, 409).

use axum::Extension;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, TimeDelta, Utc};
use dashboard_domain::constants::{ChangeEventType, GroupKey, NormalizedState};
use dashboard_domain::cursors::parse_cursor;
use dashboard_domain::grouping::compute_groups;
use dashboard_domain::incident_query::{self, IncidentFilters, QueryError};
use dashboard_domain::projection::{
    self as proj, IncidentRow, ProjectionContext, SourceState, project_all, project_incident,
};
use dashboard_store::read_repo::{self, ChangeEventFilter};
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, Postgres, Transaction};

use crate::AppState;
use crate::coordinator_session::ActiveSession;
use crate::error::{ApiResult, AppError};
use crate::params::{self, Params};
use crate::response::PyJson;
use crate::sse;

pub const SNAPSHOT_EVENTS_LIMIT: i64 = 200;
pub const EVENTS_DEFAULT_LIMIT: i64 = 50;
pub const EVENTS_MAX_LIMIT: i64 = 200;

pub(crate) fn invalid_cursor(e: dashboard_domain::cursors::InvalidCursor) -> AppError {
    AppError::validation(e.to_string()).with_code(dashboard_domain::cursors::InvalidCursor::CODE)
}

fn query_error(e: QueryError) -> AppError {
    let err = match &e {
        QueryError::CursorExpired { current_revision } => {
            AppError::conflict(e.to_string()).with_detail("currentRevision", *current_revision)
        }
        _ => AppError::validation(e.to_string()),
    };
    err.with_code(e.code())
}

/// Согласованное чтение: состояние источника, ревизия и (по запросу) реестр.
pub(crate) struct Loaded {
    pub tx: Transaction<'static, Postgres>,
    pub revision: i64,
    pub state: SourceState,
    pub rows: Vec<IncidentRow>,
}

impl Loaded {
    pub(crate) fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }
}

/// `DashboardService._load`.
pub(crate) async fn load(state: &AppState, with_rows: bool) -> ApiResult<Loaded> {
    load_in_tx(read_repo::begin_consistent_read(&state.pool).await?, with_rows).await
}

pub(crate) async fn load_in_tx(mut tx: Transaction<'static, Postgres>, with_rows: bool) -> ApiResult<Loaded> {
    let source = read_repo::import_state(&mut tx).await?;
    let source = match source {
        Some(s) if s.last_success_at.is_some() => s,
        other => {
            return Err(
                AppError::source_not_ready("Первый снимок ещё не принят, повторите запрос позже")
                    .with_detail("source", other.as_ref().map_or(Value::Null, proj::source_info)),
            );
        }
    };
    let revision = read_repo::revision(&mut tx).await?;
    let rows = if with_rows {
        read_repo::present_incidents(&mut tx).await?
    } else {
        Vec::new()
    };
    Ok(Loaded {
        tx,
        revision,
        state: source,
        rows,
    })
}

pub(crate) fn context(state: &AppState, now: DateTime<Utc>) -> ProjectionContext {
    let s = &state.settings;
    ProjectionContext {
        now,
        coordinator_group: s.coordinator_group_name.clone(),
        warning_window: TimeDelta::minutes(i64::from(s.deadline_warning_minutes)),
        itsm_url_template: s.resolved_itsm_url_template(),
    }
}

// ------------------------------------------------------------------ /snapshot (API.md §3)

/// Полный снимок (`Snapshot`); тот же объект — в событии `snapshot` SSE ([`sse::DbJournal`]).
pub async fn build_snapshot(state: &AppState) -> ApiResult<Value> {
    build_snapshot_with_view(state, false).await
}

pub(crate) async fn build_snapshot_with_view(state: &AppState, web: bool) -> ApiResult<Value> {
    let mut loaded = load(state, true).await?;
    snapshot_value(state, &mut loaded, web).await
}

pub(crate) async fn snapshot_value(state: &AppState, loaded: &mut Loaded, web: bool) -> ApiResult<Value> {
    let now = (state.clock)();
    let ctx = context(state, now);
    let (groups, projections) = project_all(&loaded.rows, &ctx);
    let events = read_repo::change_events(
        &mut loaded.tx,
        &ChangeEventFilter {
            limit: SNAPSHOT_EVENTS_LIMIT,
            ..Default::default()
        },
    )
    .await?;
    let updated_at = loaded
        .state
        .last_change_at
        .or(loaded.state.last_success_at)
        .expect("last_success_at проверен в load");
    let project = if web {
        proj::web_incident_item
    } else {
        proj::incident_item
    };
    Ok(json!({
        "revision": loaded.revision,
        "serverTime": proj::format_utc(&now),
        "updatedAt": proj::format_utc(&updated_at),
        "items": projections.iter().map(project).collect::<Vec<_>>(),
        "events": events.iter().map(proj::change_event_view).collect::<Vec<_>>(),
        "source": proj::source_info(&loaded.state),
        "itsmUrlTemplate": ctx.itsm_url_template,
        "eventCursor": loaded.revision.to_string(),
        "groups": proj::group_views(&groups),
    }))
}

pub async fn snapshot(State(state): State<AppState>, OriginalUri(uri): OriginalUri) -> ApiResult<PyJson> {
    let web = Params::new(&uri).query_raw("view") == Some("web");
    Ok(PyJson(build_snapshot_with_view(&state, web).await?))
}

// ------------------------------------------------------------------ /dashboard (ARCHITECTURE.md)

pub async fn dashboard(State(state): State<AppState>) -> ApiResult<PyJson> {
    let loaded = load(&state, true).await?;
    let now = (state.clock)();
    let (groups, projections) = project_all(&loaded.rows, &context(&state, now));
    let incidents: Map<String, Value> = projections
        .iter()
        .map(|p| (p.row.id.clone(), proj::dashboard_incident(p)))
        .collect();
    Ok(PyJson(json!({
        "eventCursor": loaded.revision.to_string(),
        "serverTime": proj::format_utc(&now),
        "source": proj::source_info(&loaded.state),
        "groups": proj::group_views(&groups),
        "incidents": incidents,
    })))
}

// ------------------------------------------------------------------ /incidents

pub async fn list_incidents(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult<PyJson> {
    let mut p = Params::new(&uri);
    if crate::web_queue::is_page_request(&p) {
        return crate::web_queue::list(&state, p).await;
    }
    let q = p.query_str("q", Some(200));
    let group = p.query_enum("group", GroupKey::ALL, GroupKey::as_str);
    let st = p.query_enum("state", NormalizedState::ALL, NormalizedState::as_str);
    let priority_id = p.query_opt_int("priorityId", 1, 4);
    let team = p.query_str("team", Some(200));
    let limit = p.query_int("limit", 1, 500, incident_query::DEFAULT_LIMIT);
    let cursor = p.query_str("cursor", None);
    let sort = p.query_str("sort", None).unwrap_or_else(|| "deadline".to_owned());
    p.finish()?;

    let filters = IncidentFilters {
        q,
        group,
        state: st,
        priority_id,
        team,
    };
    let loaded = load(&state, true).await?;
    let ctx = context(&state, (state.clock)());
    let (_, projections) = project_all(&loaded.rows, &ctx);
    let (page, next) = incident_query::query_incidents(
        &projections,
        &filters,
        &sort,
        limit,
        cursor.as_deref(),
        loaded.revision,
    )
    .map_err(query_error)?;
    Ok(PyJson(json!({
        "items": page.iter().map(|p| proj::incident_item(p)).collect::<Vec<_>>(),
        "revision": loaded.revision,
        "nextCursor": next,
    })))
}

/// Ключ из пути: `Path(min_length=1, max_length=64)`.
pub(crate) fn path_key(p: &mut Params, uri: &Uri) -> Option<String> {
    let segment = uri.path().split('/').nth(4).unwrap_or("");
    p.path_str("key", params::decode_path_segment(segment), 64)
}

pub async fn get_incident(State(state): State<AppState>, OriginalUri(uri): OriginalUri) -> ApiResult<PyJson> {
    let mut p = Params::new(&uri);
    let key = path_key(&mut p, &uri);
    let web = p.query_raw("view") == Some("web");
    p.finish()?;
    let key = key.unwrap_or_default();

    let mut loaded = load(&state, false).await?;
    let row = read_repo::incident_by_key(loaded.conn(), &key)
        .await?
        .ok_or_else(incident_not_found)?;
    let now = (state.clock)();
    let ctx = context(&state, now);
    // Группы — по тем же правилам, что и в снимке.
    let groups = if row.is_present {
        compute_groups(&row, &ctx.coordinator_group, now)
    } else {
        Vec::new()
    };
    let projection = project_incident(&row, &ctx, groups);
    let incident = if web {
        proj::web_incident_item(&projection)
    } else {
        proj::incident_item(&projection)
    };
    Ok(PyJson(json!({
        "incident": incident,
        "revision": loaded.revision,
        "normalizedState": projection.state().as_str(),
        "details": proj::incident_detail(&projection),
    })))
}

pub(crate) fn incident_not_found() -> AppError {
    AppError::not_found("Инцидент не найден").with_code("INCIDENT_NOT_FOUND")
}

pub async fn get_incident_events(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult<PyJson> {
    let mut p = Params::new(&uri);
    let key = path_key(&mut p, &uri);
    let before_id = p.query_str("beforeId", None);
    let limit = p.query_int("limit", 1, 200, EVENTS_DEFAULT_LIMIT);
    p.finish()?;
    let request = EventsRequest {
        after_id: None,
        before_id,
        event_type: None,
        sys_id: None,
        limit,
        incident_key: key,
    };
    Ok(PyJson(events(&state, request).await?))
}

// ------------------------------------------------------------------ /events (API.md §2)

pub(crate) struct EventsRequest {
    after_id: Option<String>,
    before_id: Option<String>,
    event_type: Option<ChangeEventType>,
    sys_id: Option<String>,
    limit: i64,
    incident_key: Option<String>,
}

/// `DashboardService.events`.
async fn events(state: &AppState, r: EventsRequest) -> ApiResult<Value> {
    let mut loaded = load(state, false).await?;
    let incident_pk = match &r.incident_key {
        Some(key) => Some(
            read_repo::incident_by_key(loaded.conn(), key)
                .await?
                .ok_or_else(incident_not_found)?
                .pk,
        ),
        None => None,
    };
    let limit = r.limit.clamp(1, EVENTS_MAX_LIMIT);
    let before_id = parse_cursor(r.before_id.as_deref(), "beforeId").map_err(invalid_cursor)?;
    let after_id = parse_cursor(r.after_id.as_deref(), "afterId").map_err(invalid_cursor)?;
    let rows = read_repo::change_events(
        loaded.conn(),
        &ChangeEventFilter {
            limit: limit + 1,
            before_id,
            after_id,
            event_type: r.event_type.map(ChangeEventType::as_str),
            incident_pk,
            sys_id: r.sys_id.as_deref(),
        },
    )
    .await?;
    let has_more = rows.len() as i64 > limit;
    let items: Vec<_> = rows.iter().take(limit as usize).collect();
    let next = match items.last() {
        Some(last) if has_more => Value::String(last.id.to_string()),
        _ => Value::Null,
    };
    Ok(json!({
        "items": items.into_iter().map(proj::change_event_view).collect::<Vec<_>>(),
        "nextCursor": next,
    }))
}

pub(crate) async fn list_events(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    session: Option<Extension<ActiveSession>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let mut p = Params::new(&uri);
    let after_id = p.query_str("afterId", None);
    let before_id = p.query_str("beforeId", None);
    let event_type = p.query_enum("type", ChangeEventType::ALL, ChangeEventType::as_str);
    let sys_id = p.query_str("incidentSysId", Some(64));
    let limit = p.query_int("limit", 1, 200, EVENTS_DEFAULT_LIMIT);
    let since = p.query_str("since", Some(32));
    let last_event_id = p.header_str(&headers, "Last-Event-ID", 32);
    let since_given = p.query_raw("since").is_some();
    p.finish()?;

    let accept = params::header(&headers, "accept").unwrap_or_default();
    if since_given || accept.contains("text/event-stream") {
        // Совместимость с ARCHITECTURE.md: поток notify. Только для потока: обычный JSON-запрос
        // опрос журнала не запускает.
        let broadcaster = state.broadcaster();
        let cursor = stream_cursor(last_event_id, since);
        return Ok(sse::notify_stream_with_session(
            &broadcaster,
            cursor,
            state.settings.sse_heartbeat,
            session.map(|Extension(session)| session),
        )
        .into_response());
    }
    let request = EventsRequest {
        after_id,
        before_id,
        event_type,
        sys_id,
        limit,
        incident_key: None,
    };
    Ok(PyJson(events(&state, request).await?).into_response())
}

// ------------------------------------------------------------------ /events/stream (SSE)

/// `last_event_id or since`: пустой `Last-Event-ID` уступает `since`.
fn stream_cursor(last_event_id: Option<String>, since: Option<String>) -> Option<String> {
    last_event_id.filter(|v| !v.is_empty()).or(since)
}

pub const STREAM_MODES: &[&str] = &["snapshot", "notify"];

pub(crate) async fn stream_events(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    session: Option<Extension<ActiveSession>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    // Зависимость get_broadcaster FastAPI выполняет раньше проверки параметров.
    let broadcaster = state.broadcaster();
    let mut p = Params::new(&uri);
    let mode = p.query_literal("mode", STREAM_MODES).unwrap_or("snapshot");
    let web = p.query_raw("view") == Some("web");
    let since = p.query_str("since", Some(32));
    let last_event_id = p.header_str(&headers, "Last-Event-ID", 32);
    p.finish()?;
    let heartbeat = state.settings.sse_heartbeat;
    let session = session.map(|Extension(session)| session);
    let stream = if mode == "notify" {
        sse::notify_stream_with_session(
            &broadcaster,
            stream_cursor(last_event_id, since),
            heartbeat,
            session,
        )
    } else {
        sse::snapshot_stream_with_view(&broadcaster, heartbeat, web, session)
    };
    Ok(stream.into_response())
}
