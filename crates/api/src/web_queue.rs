//! Страницы очереди веб-клиента. Старые клиенты по-прежнему могут использовать limit/cursor.

use std::cmp::Reverse;

use chrono::{DateTime, TimeDelta, Utc};
use dashboard_domain::constants::NormalizedState;
use dashboard_domain::grouping::compute_groups;
use dashboard_domain::projection::{IncidentRow, project_incident, web_incident_item};
use serde_json::json;

use crate::dashboard::{context, load};
use crate::params::Params;
use crate::response::PyJson;
use crate::{ApiResult, AppError, AppState};

const GROUPS: &[&str] = &[
    "all",
    "critical",
    "coordinators",
    "research",
    "high",
    "resolved",
    "closed",
];
const SLA: &[&str] = &[
    "late", "soon", "normal", "unknown", "research", "restored", "closed",
];

pub(crate) fn is_page_request(p: &Params) -> bool {
    ["page", "pageSize", "priority", "status", "sla"]
        .iter()
        .any(|key| p.query_raw(key).is_some())
        || matches!(p.query_raw("group"), Some("all" | "resolved" | "closed"))
        || p.query_raw("sort") == Some("newest")
        || p.query_raw("team") == Some("__unassigned__")
}

/// Как stateOf в веб-клиенте: неизвестный stateEnum сохраняет статус Unknown.
pub(crate) fn state_of(row: &IncidentRow) -> NormalizedState {
    match row.state_enum.as_deref() {
        Some(name) if !name.is_empty() => name.parse().unwrap_or(NormalizedState::Unknown),
        _ => row
            .state_id
            .and_then(|id| NormalizedState::from_state_id(i64::from(id)))
            .unwrap_or(NormalizedState::Unknown),
    }
}

pub(crate) fn sla_kind(row: &IncidentRow, now: DateTime<Utc>) -> &'static str {
    use NormalizedState::*;
    match state_of(row) {
        Research => "research",
        Resolved => "restored",
        Closed => "closed",
        Assigned | WorkInProgress => match row.deadline {
            None => "unknown",
            Some(deadline) if deadline <= now => "late",
            Some(deadline) if deadline - now <= TimeDelta::minutes(30) => "soon",
            Some(_) => "normal",
        },
        Unknown => "unknown",
    }
}

struct Filters {
    group: &'static str,
    query: String,
    team: Option<String>,
    priority: Option<i64>,
    status: Option<NormalizedState>,
    sla: Option<&'static str>,
}

impl Filters {
    fn matches(&self, row: &IncidentRow, coordinator: &str, now: DateTime<Utc>) -> bool {
        let state = state_of(row);
        let team = row.assigned_group_name.as_deref().unwrap_or("");
        let in_group = match self.group {
            "critical" => state.is_active() && row.priority_id == 4,
            "high" => state.is_active() && row.priority_id == 3,
            "coordinators" => state.is_attention() && team.trim() == coordinator,
            "research" => state == NormalizedState::Research,
            "resolved" => state == NormalizedState::Resolved,
            "closed" => state == NormalizedState::Closed,
            _ => true,
        };
        in_group
            && self.status.is_none_or(|s| state == s)
            && self.priority.is_none_or(|p| i64::from(row.priority_id) == p)
            && self.sla.is_none_or(|s| sla_kind(row, now) == s)
            && match self.team.as_deref() {
                None | Some("") => true,
                Some("__unassigned__") => team.trim().is_empty(),
                Some(t) => team == t,
            }
            && (self.query.is_empty()
                || [
                    Some(row.id.as_str()),
                    row.header.as_deref(),
                    row.description.as_deref(),
                    row.product_name.as_deref(),
                    row.service.as_deref(),
                    row.assigned_group_name.as_deref(),
                ]
                .into_iter()
                .flatten()
                .any(|v| v.to_lowercase().contains(&self.query)))
    }
}

pub(crate) async fn list(state: &AppState, mut p: Params) -> ApiResult<PyJson> {
    if p.query_raw("cursor").is_some() || p.query_raw("limit").is_some() {
        return Err(AppError::validation(
            "Параметры limit и cursor несовместимы с постраничной очередью",
        ));
    }
    let page = p.query_int("page", 1, i128::from(i64::MAX), 1) as usize;
    let page_size = p.query_int("pageSize", 1, 100, 9) as usize;
    let filters = Filters {
        group: p.query_literal("group", GROUPS).unwrap_or("all"),
        query: p
            .query_str("q", Some(200))
            .unwrap_or_default()
            .trim()
            .to_lowercase(),
        team: p.query_str("team", Some(200)),
        priority: p.query_opt_int("priority", 1, 4),
        status: p.query_enum("status", NormalizedState::ALL, NormalizedState::as_str),
        sla: p.query_literal("sla", SLA),
    };
    let sort = p
        .query_literal("sort", &["priority", "deadline", "newest"])
        .unwrap_or("priority");
    p.finish()?;

    let loaded = load(state, true).await?;
    let ctx = context(state, (state.clock)());
    let mut rows: Vec<_> = loaded
        .rows
        .iter()
        .filter(|r| filters.matches(r, &ctx.coordinator_group, ctx.now))
        .collect();
    if sort == "newest" {
        rows.sort_by(|a, b| b.submit_date.cmp(&a.submit_date).then_with(|| a.id.cmp(&b.id)));
    } else {
        rows.sort_by_key(|r| {
            (
                Reverse(state_of(r).is_attention()),
                Reverse(if sort == "priority" { r.priority_id } else { 0 }),
                r.deadline.is_none(),
                r.deadline,
                &r.id,
            )
        });
    }
    let total = rows.len();
    let total_pages = total.div_ceil(page_size).max(1);
    let page = page.min(total_pages);
    let items: Vec<_> = rows
        .into_iter()
        .skip((page - 1) * page_size)
        .take(page_size)
        .map(|r| {
            let groups = compute_groups(r, &ctx.coordinator_group, ctx.now);
            web_incident_item(&project_incident(r, &ctx, groups))
        })
        .collect();
    Ok(PyJson(json!({
        "items": items, "total": total, "page": page, "pageSize": page_size,
        "totalPages": total_pages, "revision": loaded.revision,
    })))
}
