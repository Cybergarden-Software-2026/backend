//! Проекции строк БД в JSON-ответы API — порт `app/services/projection.py` и схем
//! `app/schemas/**` (порядок полей, camelCase и формат времени — как у pydantic).
//!
//! Чистая логика: строки приходят из `store` уже разобранными ([`IncidentRow`],
//! [`ChangeEventRow`], [`SourceState`], [`NotificationRow`]). Ответы собираются в
//! `serde_json::Value` с сохранением порядка ключей (`preserve_order`), поэтому JSON совпадает с
//! Python-версией и по порядку полей.

use chrono::{DateTime, Datelike, TimeDelta, Timelike, Utc};
use serde_json::{Map, Value, json};

use crate::constants::{GroupKey, NormalizedState, ResolutionPhase, SourceStatus};
use crate::grouping::{
    GroupResult, Groupable, ResolutionInfo, assignee_of, build_groups, describe_resolution,
};
use crate::pycompat;
use crate::sla::{SlaResult, compute_sla};

pub const DEFAULT_SOURCE_LABEL: &str = "ITSM / источник инцидентов";

/// `format_utc`: `2026-11-20T14:30:00.123Z` — миллисекунды отбрасываются, не округляются.
/// Год — без дополнения нулями, как `strftime("%Y")` glibc в Python (`999-01-02T…`).
pub fn format_utc(value: &DateTime<Utc>) -> String {
    format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        value.year(),
        value.month(),
        value.day(),
        value.hour(),
        value.minute(),
        value.second(),
        value.timestamp_subsec_micros() / 1000
    )
}

fn time(value: &DateTime<Utc>) -> Value {
    Value::String(format_utc(value))
}

fn opt_time(value: Option<DateTime<Utc>>) -> Value {
    value.as_ref().map_or(Value::Null, time)
}

fn opt_str(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |s| Value::String(s.to_owned()))
}

// ------------------------------------------------------------------ строки БД

/// Строка `incidents` (ORM-модель `Incident`), все колонки.
#[derive(Debug, Clone, PartialEq)]
pub struct IncidentRow {
    pub pk: i64,
    pub id: String,
    pub sys_id: String,
    pub header: Option<String>,
    pub description: Option<String>,
    pub short_description: Option<String>,
    pub solution: Option<String>,
    pub product_name: Option<String>,
    pub product_id: Option<String>,
    pub product_master_id: Option<String>,
    pub product_url: Option<String>,
    pub service: Option<String>,
    pub service_id: Option<String>,
    pub scenario_name: Option<String>,
    pub service_type: Option<String>,
    pub service_type_id: Option<i32>,
    pub inc_type: Option<String>,
    pub receiving_channel: Option<String>,
    pub state_label: Option<String>,
    pub state_enum: Option<String>,
    pub state_id: Option<i32>,
    pub state_reason: Option<String>,
    pub state_reason_id: Option<String>,
    pub normalized_state: NormalizedState,
    pub status_mismatch: bool,
    pub priority_id: i16,
    pub priority: Option<String>,
    pub urgency_id: Option<i16>,
    pub urgency: Option<String>,
    pub impact_id: Option<i16>,
    pub impact: Option<String>,
    pub assigned_group_name: Option<String>,
    pub assigned_employee: Option<String>,
    pub assigned_employee_login: Option<String>,
    pub initiator_name: Option<String>,
    pub initiator_login: Option<String>,
    pub submit_date: DateTime<Utc>,
    pub deadline: Option<DateTime<Utc>>,
    pub completion_date: Option<DateTime<Utc>>,
    pub last_assignment_to_team_at: Option<DateTime<Utc>>,
    pub waiting_exit_date: Option<DateTime<Utc>>,
    pub is_effect: Option<bool>,
    pub is_reason: Option<bool>,
    pub is_double: Option<bool>,
    pub is_master: Option<bool>,
    pub is_root_incident: Option<bool>,
    pub is_need_up: Option<bool>,
    pub elimination_type: Option<String>,
    pub elimination_type_id: Option<String>,
    /// Исходная запись выгрузки (порядок ключей — как его отдаёт JSONB).
    pub raw: Map<String, Value>,
    pub incident_revision: i32,
    pub is_present: bool,
    pub removed_at: Option<DateTime<Utc>>,
    pub resolution_phase: Option<ResolutionPhase>,
    pub resolved_detected_at: Option<DateTime<Utc>>,
    pub recently_resolved_until: Option<DateTime<Utc>>,
    pub first_seen_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Groupable for IncidentRow {
    fn id(&self) -> &str {
        &self.id
    }
    fn sys_id(&self) -> &str {
        &self.sys_id
    }
    fn normalized_state(&self) -> NormalizedState {
        self.normalized_state
    }
    fn priority_id(&self) -> i64 {
        i64::from(self.priority_id)
    }
    fn assigned_group_name(&self) -> Option<&str> {
        self.assigned_group_name.as_deref()
    }
    fn assigned_employee(&self) -> Option<&str> {
        self.assigned_employee.as_deref()
    }
    fn deadline(&self) -> Option<DateTime<Utc>> {
        self.deadline
    }
    fn submit_date(&self) -> DateTime<Utc> {
        self.submit_date
    }
    fn completion_date(&self) -> Option<DateTime<Utc>> {
        self.completion_date
    }
    fn resolution_phase(&self) -> Option<ResolutionPhase> {
        self.resolution_phase
    }
    fn resolved_detected_at(&self) -> Option<DateTime<Utc>> {
        self.resolved_detected_at
    }
    fn recently_resolved_until(&self) -> Option<DateTime<Utc>> {
        self.recently_resolved_until
    }
}

/// Строка `change_events`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeEventRow {
    pub id: i64,
    pub revision: i64,
    pub incident_pk: i64,
    pub incident_revision: i32,
    pub sys_id: String,
    pub incident_id: String,
    pub header: Option<String>,
    pub types: Vec<String>,
    pub fields: Vec<String>,
    pub at: DateTime<Utc>,
    /// JSON `null` и SQL NULL в ответе неразличимы (`null`).
    pub previous: Option<Value>,
    pub current: Option<Value>,
    pub contact: Option<Value>,
}

/// Поля `import_state`, которые нужны API.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceState {
    pub status: String,
    pub source_path: Option<String>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_change_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub error_since: Option<DateTime<Utc>>,
    pub incident_count: Option<i32>,
    pub warnings: Value,
}

/// Уведомление и отметка прочтения текущего пользователя (`NotificationRepository.page`).
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationRow {
    pub id: i64,
    pub r#type: String,
    pub title: String,
    pub body: String,
    pub incident_id: Option<String>,
    pub sys_id: Option<String>,
    pub incident_revision: Option<i32>,
    pub change_event_id: Option<i64>,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
    pub read_at: Option<DateTime<Utc>>,
}

// ------------------------------------------------------------------ проекция инцидента

#[derive(Debug, Clone)]
pub struct ProjectionContext {
    pub now: DateTime<Utc>,
    pub coordinator_group: String,
    pub warning_window: TimeDelta,
    pub itsm_url_template: Option<String>,
}

/// Все производные данные одного инцидента, вычисленные один раз.
#[derive(Debug, Clone)]
pub struct IncidentProjection<'a> {
    pub row: &'a IncidentRow,
    pub groups: Vec<GroupKey>,
    pub sla: SlaResult,
    pub resolution: Option<ResolutionInfo>,
    pub itsm_url: Option<String>,
}

impl IncidentProjection<'_> {
    pub fn state(&self) -> NormalizedState {
        self.row.normalized_state
    }
}

/// `build_itsm_url`: URL-encoded `{sysId}` и `{id}`; без шаблона — `None`.
pub fn build_itsm_url(template: Option<&str>, incident_id: &str, sys_id: &str) -> Option<String> {
    let template = template.filter(|t| !t.is_empty())?;
    Some(
        template
            .replace("{sysId}", &pycompat::url_quote(sys_id))
            .replace("{id}", &pycompat::url_quote(incident_id)),
    )
}

pub fn project_incident<'a>(
    row: &'a IncidentRow,
    ctx: &ProjectionContext,
    groups: Vec<GroupKey>,
) -> IncidentProjection<'a> {
    IncidentProjection {
        row,
        groups,
        sla: compute_sla(
            row.deadline,
            row.normalized_state,
            ctx.now,
            ctx.warning_window,
            row.completion_date,
        ),
        resolution: describe_resolution(row, ctx.now),
        itsm_url: build_itsm_url(ctx.itsm_url_template.as_deref(), &row.id, &row.sys_id),
    }
}

pub fn project_all<'a>(
    rows: &'a [IncidentRow],
    ctx: &ProjectionContext,
) -> (Vec<GroupResult<'a, IncidentRow>>, Vec<IncidentProjection<'a>>) {
    let (groups, mut membership) = build_groups(rows.iter(), &ctx.coordinator_group, ctx.now);
    let projections = rows
        .iter()
        .map(|r| {
            let keys = membership.remove(&r.id).unwrap_or_default();
            project_incident(r, ctx, keys)
        })
        .collect();
    (groups, projections)
}

// ------------------------------------------------------------------ схемы API.md

fn groups_json(groups: &[GroupKey]) -> Value {
    groups.iter().map(|g| Value::from(g.as_str())).collect()
}

/// `SlaInfo`.
pub fn sla_json(sla: &SlaResult) -> Value {
    json!({
        "status": sla.status.as_str(),
        "label": sla.label,
        "deadlineAt": opt_time(sla.deadline_at),
        "remainingSeconds": sla.remaining_seconds,
        "computedAt": time(&sla.computed_at),
    })
}

/// `Resolution`.
pub fn resolution_json(res: Option<&ResolutionInfo>) -> Value {
    let Some(res) = res else {
        return Value::Null;
    };
    json!({
        "phase": res.phase.as_str(),
        "label": res.label,
        "resolvedAt": time(&res.resolved_at),
        "resolvedAtSource": res.resolved_at_source.as_str(),
        "detectedAt": time(&res.detected_at),
        "recentlyResolvedUntil": opt_time(res.recently_resolved_until),
    })
}

/// `DashboardMeta` — серверные расчёты, поле `dashboard` исходной записи.
pub fn dashboard_meta(p: &IncidentProjection<'_>) -> Value {
    let r = p.row;
    json!({
        "incidentRevision": r.incident_revision,
        "normalizedState": p.state().as_str(),
        "statusRecognized": p.state() != NormalizedState::Unknown,
        "statusMismatch": r.status_mismatch,
        "attention": p.state().is_attention(),
        "groups": groups_json(&p.groups),
        "assignee": opt_str(assignee_of(r)),
        "sla": sla_json(&p.sla),
        "resolution": resolution_json(p.resolution.as_ref()),
        "itsmUrl": p.itsm_url,
        "firstSeenAt": time(&r.first_seen_at),
        "updatedAt": time(&r.updated_at),
    })
}

/// `IncidentItem`: `sysId`, `id`, `dashboard`, затем остальные поля исходной записи в исходном
/// порядке (`extra="allow"`; ключ `dashboard` исходной записи заменяется расчётом).
pub fn incident_item(p: &IncidentProjection<'_>) -> Value {
    let raw = &p.row.raw;
    let mut item = Map::with_capacity(raw.len() + 1);
    item.insert("sysId".into(), raw.get("sysId").cloned().unwrap_or(Value::Null));
    item.insert("id".into(), raw.get("id").cloned().unwrap_or(Value::Null));
    item.insert("dashboard".into(), dashboard_meta(p));
    for (k, v) in raw {
        if !matches!(k.as_str(), "sysId" | "id" | "dashboard") {
            item.insert(k.clone(), v.clone());
        }
    }
    Value::Object(item)
}

/// Preserve source fields and supply authoritative UTC instants for browser-side clocks.
pub fn web_incident_item(p: &IncidentProjection<'_>) -> Value {
    let mut item = incident_item(p);
    let r = p.row;
    item["dashboard"]["sourceDates"] = json!({
        "submitDate": r.submit_date,
        "deadline": r.deadline,
        "completionDate": r.completion_date,
        "lastAssignmentToTeamDateTime": r.last_assignment_to_team_at,
    });
    item
}

/// `GroupView`.
pub fn group_views<I: Groupable + ?Sized>(groups: &[GroupResult<'_, I>]) -> Value {
    groups
        .iter()
        .map(|g| {
            json!({
                "key": g.key.as_str(),
                "title": g.title,
                "incidentIds": g.incident_ids(),
                "sysIds": g.sys_ids(),
                "count": g.incidents.len(),
            })
        })
        .collect()
}

/// `ChangeEventView`.
pub fn change_event_view(ce: &ChangeEventRow) -> Value {
    let mut value = json!({
        "eventId": ce.id,
        "revision": ce.revision,
        "sysId": ce.sys_id,
        "incidentId": ce.incident_id,
        "header": ce.header,
        "types": ce.types,
        "fields": ce.fields,
        "at": time(&ce.at),
        "previous": ce.previous.clone().unwrap_or(Value::Null),
        "current": ce.current.clone().unwrap_or(Value::Null),
        "incidentRevision": ce.incident_revision,
    });
    if let Some(contact) = &ce.contact {
        value["contact"] = contact.clone();
    }
    value
}

/// `PurePosixPath(path).name`: последний компонент без `.` и пустых (`..` сохраняется).
fn pure_path_name(path: &str) -> &str {
    path.rsplit('/')
        .find(|part| !part.is_empty() && *part != ".")
        .unwrap_or("")
}

/// `SourceInfo`: имя источника без серверного пути; ошибка — только в состоянии `error`.
pub fn source_info(state: &SourceState) -> Value {
    let path = match state.source_path.as_deref() {
        Some(p) if !p.is_empty() => pure_path_name(p),
        _ => DEFAULT_SOURCE_LABEL,
    };
    let error = if state.status == SourceStatus::Error.as_str() {
        opt_str(state.last_error.as_deref())
    } else {
        Value::Null
    };
    let warnings = match &state.warnings {
        Value::Null => Value::Array(Vec::new()),
        other => other.clone(),
    };
    json!({
        "path": path,
        "readAt": opt_time(state.last_success_at),
        "error": error,
        "status": state.status,
        "lastSuccessfulImportAt": opt_time(state.last_success_at),
        "lastCheckedAt": opt_time(state.last_checked_at),
        "errorSince": opt_time(state.error_since),
        "incidentCount": state.incident_count,
        "warnings": warnings,
    })
}

// ------------------------------------------------------------------ схемы ARCHITECTURE.md

/// Статус для контракта ARCHITECTURE.md (`ARCH_STATUS`).
pub const fn arch_status(state: NormalizedState) -> &'static str {
    match state {
        NormalizedState::Assigned => "assigned",
        NormalizedState::WorkInProgress => "in_progress",
        NormalizedState::Resolved => "resolved",
        NormalizedState::Closed => "closed",
        NormalizedState::Research => "research",
        NormalizedState::Unknown => "unknown",
    }
}

fn dashboard_fields(p: &IncidentProjection<'_>) -> Map<String, Value> {
    let r = p.row;
    let Value::Object(map) = json!({
        "id": r.id,
        "sysId": r.sys_id,
        "incidentRevision": r.incident_revision,
        "header": r.header,
        "shortDescription": r.short_description,
        "productName": r.product_name,
        "service": r.service,
        "status": arch_status(p.state()),
        "stateEnum": r.state_enum,
        "stateLabel": r.state_label,
        "priorityId": r.priority_id,
        "priority": r.priority,
        "assignedGroupName": r.assigned_group_name,
        "assignedEmployee": r.assigned_employee,
        "assignee": opt_str(assignee_of(r)),
        "registeredAt": time(&r.submit_date),
        "deadlineAt": opt_time(r.deadline),
        "completedAt": opt_time(r.completion_date),
        "recentlyResolvedUntil": opt_time(p.resolution.as_ref().and_then(|x| x.recently_resolved_until)),
        "resolution": resolution_json(p.resolution.as_ref()),
        "sla": sla_json(&p.sla),
        "groups": groups_json(&p.groups),
        "itsmUrl": p.itsm_url,
    }) else {
        unreachable!()
    };
    map
}

/// `DashboardIncident` — карточка `/dashboard`.
pub fn dashboard_incident(p: &IncidentProjection<'_>) -> Value {
    Value::Object(dashboard_fields(p))
}

/// `IncidentDetail` — поля `DashboardIncident`, затем полная нормализованная карточка.
pub fn incident_detail(p: &IncidentProjection<'_>) -> Value {
    let r = p.row;
    let mut map = dashboard_fields(p);
    let Value::Object(extra) = json!({
        "description": r.description,
        "solution": r.solution,
        "stateId": r.state_id,
        "stateReason": r.state_reason,
        "stateReasonId": r.state_reason_id,
        "normalizedState": p.state().as_str(),
        "statusRecognized": p.state() != NormalizedState::Unknown,
        "statusMismatch": r.status_mismatch,
        "urgencyId": r.urgency_id,
        "urgency": r.urgency,
        "impactId": r.impact_id,
        "impact": r.impact,
        "productId": r.product_id,
        "productMasterId": r.product_master_id,
        "productUrl": r.product_url,
        "serviceId": r.service_id,
        "scenarioName": r.scenario_name,
        "serviceType": r.service_type,
        "serviceTypeId": r.service_type_id,
        "incType": r.inc_type,
        "receivingChannel": r.receiving_channel,
        "assignedEmployeeLogin": r.assigned_employee_login,
        "initiatorName": r.initiator_name,
        "initiatorLogin": r.initiator_login,
        "lastAssignmentToTeamAt": opt_time(r.last_assignment_to_team_at),
        "waitingExitDate": opt_time(r.waiting_exit_date),
        "isEffect": r.is_effect,
        "isReason": r.is_reason,
        "isDouble": r.is_double,
        "isMaster": r.is_master,
        "isRootIncident": r.is_root_incident,
        "isNeedUp": r.is_need_up,
        "eliminationType": r.elimination_type,
        "eliminationTypeId": r.elimination_type_id,
        "isPresent": r.is_present,
        "removedAt": opt_time(r.removed_at),
        "firstSeenAt": time(&r.first_seen_at),
        "updatedAt": time(&r.updated_at),
    }) else {
        unreachable!()
    };
    map.extend(extra);
    Value::Object(map)
}

// ------------------------------------------------------------------ уведомления

/// `NotificationView`.
pub fn notification_view(n: &NotificationRow) -> Value {
    let payload = match &n.payload {
        Value::Null => Value::Object(Map::new()),
        Value::Object(m) if m.is_empty() => Value::Object(Map::new()),
        other => other.clone(),
    };
    json!({
        "id": n.id,
        "type": n.r#type,
        "title": n.title,
        "body": n.body,
        "incidentId": n.incident_id,
        "sysId": n.sys_id,
        "incidentRevision": n.incident_revision,
        "changeEventId": n.change_event_id,
        "payload": payload,
        "createdAt": time(&n.created_at),
        "isRead": n.read_at.is_some(),
        "readAt": opt_time(n.read_at),
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn utc_format_truncates_to_millis() {
        let t = Utc.with_ymd_and_hms(2026, 11, 20, 14, 30, 0).unwrap() + TimeDelta::microseconds(123_999);
        assert_eq!(format_utc(&t), "2026-11-20T14:30:00.123Z");
        let old = Utc.with_ymd_and_hms(999, 1, 2, 3, 4, 5).unwrap();
        assert_eq!(format_utc(&old), "999-01-02T03:04:05.000Z");
    }

    #[test]
    fn itsm_url_is_url_encoded() {
        assert_eq!(
            build_itsm_url(Some("https://itsm/x/{sysId}?n={id}"), "INC 1/2", "17").as_deref(),
            Some("https://itsm/x/17?n=INC%201%2F2")
        );
        assert_eq!(build_itsm_url(None, "INC1", "1"), None);
        assert_eq!(build_itsm_url(Some(""), "INC1", "1"), None);
    }

    #[test]
    fn path_name_like_pure_path() {
        assert_eq!(
            pure_path_name("/data/character_problems.json"),
            "character_problems.json"
        );
        assert_eq!(pure_path_name("data/x.json/"), "x.json");
        assert_eq!(pure_path_name("a/.."), "..");
        assert_eq!(pure_path_name("a/."), "a");
        assert_eq!(pure_path_name("/"), "");
        assert_eq!(pure_path_name("."), "");
    }
}
