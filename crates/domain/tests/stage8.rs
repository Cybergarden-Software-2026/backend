//! Порт `backend/tests/unit/test_stage8_projection.py`: проекции в ответы API, ссылка ITSM,
//! фильтры, сортировка и курсоры `/incidents` (без БД).

mod common;

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Europe::Moscow;
use dashboard_domain::constants::{GroupKey as G, NormalizedState as S, ResolutionPhase};
use dashboard_domain::incident_query::{
    IncidentFilters, QueryError, decode_cursor, encode_cursor, query_incidents,
};
use dashboard_domain::normalizer::NormalizedIncident;
use dashboard_domain::projection::{
    ChangeEventRow, IncidentProjection, IncidentRow, ProjectionContext, SourceState, build_itsm_url,
    change_event_view, dashboard_incident, group_views, incident_detail, incident_item, project_all,
    source_info,
};
use dashboard_domain::validator::validate_snapshot;
use serde_json::{Value, json};

const COORD: &str = "КЦ ИТ Координаторы Массовых Инцидентов";
const DEMO_ID: &str = "INC000021938167";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
}

fn ctx() -> ProjectionContext {
    ProjectionContext {
        now: now(),
        coordinator_group: COORD.to_owned(),
        warning_window: TimeDelta::minutes(60),
        itsm_url_template: Some("https://itsm.example/incidents/{sysId}".to_owned()),
    }
}

fn int<T: TryFrom<i64>>(v: Option<i64>) -> Option<T> {
    v.map(|v| T::try_from(v).ok().expect("в диапазоне колонки"))
}

/// Строка `incidents`, собранная так же, как её записывает импортёр.
fn row(i: &NormalizedIncident, pk: i64) -> IncidentRow {
    let c = &i.columns;
    IncidentRow {
        pk,
        id: i.id.clone(),
        sys_id: i.sys_id.clone(),
        header: c.header.clone(),
        description: c.description.clone(),
        short_description: c.short_description.clone(),
        solution: c.solution.clone(),
        product_name: c.product_name.clone(),
        product_id: c.product_id.clone(),
        product_master_id: c.product_master_id.clone(),
        product_url: c.product_url.clone(),
        service: c.service.clone(),
        service_id: c.service_id.clone(),
        scenario_name: c.scenario_name.clone(),
        service_type: c.service_type.clone(),
        service_type_id: int(c.service_type_id),
        inc_type: c.inc_type.clone(),
        receiving_channel: c.receiving_channel.clone(),
        state_label: c.state_label.clone(),
        state_enum: c.state_enum.clone(),
        state_id: int(c.state_id),
        state_reason: c.state_reason.clone(),
        state_reason_id: c.state_reason_id.clone(),
        normalized_state: c.normalized_state,
        status_mismatch: c.status_mismatch,
        priority_id: int(c.priority_id).unwrap(),
        priority: c.priority.clone(),
        urgency_id: int(c.urgency_id),
        urgency: c.urgency.clone(),
        impact_id: int(c.impact_id),
        impact: c.impact.clone(),
        assigned_group_name: c.assigned_group_name.clone(),
        assigned_employee: c.assigned_employee.clone(),
        assigned_employee_login: c.assigned_employee_login.clone(),
        initiator_name: c.initiator_name.clone(),
        initiator_login: c.initiator_login.clone(),
        submit_date: c.submit_date.unwrap(),
        deadline: c.deadline,
        completion_date: c.completion_date,
        last_assignment_to_team_at: c.last_assignment_to_team_at,
        waiting_exit_date: c.waiting_exit_date,
        is_effect: c.is_effect,
        is_reason: c.is_reason,
        is_double: c.is_double,
        is_master: c.is_master,
        is_root_incident: c.is_root_incident,
        is_need_up: c.is_need_up,
        elimination_type: c.elimination_type.clone(),
        elimination_type_id: c.elimination_type_id.clone(),
        raw: i.raw.clone(),
        incident_revision: 1,
        is_present: true,
        removed_at: None,
        resolution_phase: None,
        resolved_detected_at: None,
        recently_resolved_until: None,
        first_seen_at: now() - TimeDelta::days(1),
        updated_at: now() - TimeDelta::hours(1),
    }
}

fn rows() -> Vec<IncidentRow> {
    let snapshot = validate_snapshot(common::values(common::sample()), Moscow).unwrap();
    snapshot
        .incidents
        .iter()
        .enumerate()
        .map(|(n, i)| row(i, n as i64 + 1))
        .collect()
}

fn state() -> SourceState {
    SourceState {
        status: "ok".into(),
        source_path: Some("/data/character_problems.json".into()),
        last_checked_at: Some(now()),
        last_success_at: Some(now()),
        last_change_at: None,
        last_error: None,
        error_since: None,
        incident_count: Some(44),
        warnings: json!([]),
    }
}

// ================================================================ ITSM

#[test]
fn itsm_url_is_url_encoded() {
    assert_eq!(
        build_itsm_url(Some("https://itsm/x/{sysId}?n={id}"), "INC 1/2", "17").as_deref(),
        Some("https://itsm/x/17?n=INC%201%2F2")
    );
    assert_eq!(build_itsm_url(None, "INC1", "1"), None);
}

// ================================================================ проекции

#[test]
fn snapshot_items_keep_source_record_and_add_dashboard() {
    let rows = rows();
    let (groups, projections) = project_all(&rows, &ctx());
    let items: Vec<Value> = projections.iter().map(incident_item).collect();
    let mut demo = items.iter().find(|i| i["id"] == DEMO_ID).unwrap().clone();
    let dashboard = demo.as_object_mut().unwrap().shift_remove("dashboard").unwrap();
    let source = common::sample().into_iter().find(|r| r["id"] == DEMO_ID).unwrap();
    assert_eq!(demo, Value::Object(source)); // исходная запись без изменений
    assert_eq!(dashboard["groups"], json!(["high"]));
    assert_eq!(dashboard["sla"]["status"], "overdue");
    assert_eq!(
        dashboard["itsmUrl"],
        "https://itsm.example/incidents/179429233402440192"
    );
    assert_eq!(dashboard["assignee"], "Медведь Барсик (barsik)");
    assert_eq!(dashboard["attention"], true);
    assert_eq!(dashboard["statusRecognized"], true);
    let counts: Vec<(Value, Value)> = group_views(&groups)
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["key"].clone(), g["count"].clone()))
        .collect();
    assert_eq!(
        counts,
        [("critical", 0), ("coordinators", 2), ("high", 3), ("research", 5)]
            .map(|(k, c)| (json!(k), json!(c)))
    );
}

/// `IncidentItem` сериализуется с `sysId` первым полем, затем `id`, `dashboard` и остальные.
#[test]
fn incident_item_field_order() {
    let rows = rows();
    let (_, projections) = project_all(&rows, &ctx());
    let item = incident_item(&projections[0]);
    let keys: Vec<&str> = item
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .take(4)
        .collect();
    assert_eq!(keys[..3], ["sysId", "id", "dashboard"]);
    assert!(!item.as_object().unwrap().contains_key("sys_id"));
}

fn demo_projection<'a>(projections: &'a [IncidentProjection<'a>]) -> &'a IncidentProjection<'a> {
    projections.iter().find(|p| p.row.id == DEMO_ID).unwrap()
}

#[test]
fn dashboard_incident_matches_architecture_names() {
    let rows = rows();
    let (_, projections) = project_all(&rows, &ctx());
    let card = dashboard_incident(demo_projection(&projections));
    assert_eq!(card["status"], "in_progress");
    assert_eq!(card["registeredAt"], "2026-11-10T06:32:14.000Z");
    assert_eq!(card["deadlineAt"], "2026-11-10T12:32:14.000Z");
    assert_eq!(card["productName"], "Вечная зажигалка");
    assert_eq!(card["priorityId"], 3);
    assert_eq!(card["recentlyResolvedUntil"], Value::Null);
    assert_eq!(card["groups"], json!(["high"]));
}

#[test]
fn incident_detail_has_all_normalized_fields() {
    let rows = rows();
    let (_, projections) = project_all(&rows, &ctx());
    let detail = incident_detail(demo_projection(&projections));
    assert!(
        detail["description"]
            .as_str()
            .unwrap()
            .starts_with("В системе Вечная зажигалка")
    );
    assert_eq!(detail["waitingExitDate"], Value::Null); // заглушка → null
    assert_eq!(detail["lastAssignmentToTeamAt"], "2026-11-11T03:03:15.000Z");
    assert_eq!(detail["normalizedState"], "WorkInProgress");
    assert_eq!(detail["isPresent"], true);
}

#[test]
fn resolution_mark_in_projection() {
    let mut rows = rows();
    let demo = rows.iter_mut().find(|r| r.id == DEMO_ID).unwrap();
    demo.priority_id = 4;
    demo.normalized_state = S::Research;
    demo.completion_date = Some(now() - TimeDelta::minutes(5));
    demo.resolution_phase = Some(ResolutionPhase::Research);
    demo.resolved_detected_at = Some(now() - TimeDelta::minutes(1));
    let (_, projections) = project_all(&rows, &ctx());
    let meta = incident_item(demo_projection(&projections))["dashboard"].clone();
    assert_eq!(meta["groups"], json!(["critical", "research"]));
    assert_eq!(meta["resolution"]["label"], "Решён · исследование");
    assert_eq!(meta["resolution"]["resolvedAtSource"], "completionDate");
    assert_eq!(meta["sla"]["status"], "not_applicable");
}

#[test]
fn unknown_status_projection() {
    let mut rows = rows();
    rows[0].normalized_state = S::Unknown;
    let (_, projections) = project_all(&rows, &ctx());
    let meta = incident_item(&projections[0])["dashboard"].clone();
    assert_eq!(meta["statusRecognized"], false);
    assert_eq!(meta["groups"], json!([]));
    assert_eq!(meta["attention"], false);
}

#[test]
fn source_info_shapes() {
    let ok = source_info(&state());
    assert_eq!(ok["path"], "character_problems.json"); // без серверного пути
    assert_eq!(ok["error"], Value::Null);
    assert_eq!(ok["readAt"], "2026-11-20T12:00:00.000Z");
    let err = source_info(&SourceState {
        status: "error".into(),
        last_error: Some("некорректный JSON".into()),
        error_since: Some(now()),
        ..state()
    });
    assert_eq!(err["error"], "некорректный JSON");
    assert_eq!(err["status"], "error");
    let hidden = source_info(&SourceState {
        last_error: Some("старая ошибка".into()),
        ..state()
    });
    assert_eq!(hidden["error"], Value::Null); // прошлая ошибка не показывается
    let unnamed = source_info(&SourceState {
        source_path: None,
        ..state()
    });
    assert_eq!(unnamed["path"], "ITSM / источник инцидентов");
}

#[test]
fn change_event_view_shape() {
    let ce = ChangeEventRow {
        id: 105,
        revision: 42,
        incident_pk: 1,
        incident_revision: 2,
        sys_id: "1".into(),
        incident_id: "INC1".into(),
        header: Some("h".into()),
        types: vec!["critical".into(), "priority".into()],
        fields: vec!["priorityId".into()],
        at: now(),
        previous: None,
        current: Some(json!({"id": "INC1"})),
    };
    let view = change_event_view(&ce);
    assert_eq!(view["eventId"], 105);
    assert_eq!(view["types"], json!(["critical", "priority"]));
    assert_eq!(view["previous"], Value::Null);
    let keys: Vec<&str> = view.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "eventId",
            "revision",
            "sysId",
            "incidentId",
            "header",
            "types",
            "fields",
            "at",
            "previous",
            "current",
            "incidentRevision"
        ]
    );
}

// ================================================================ /incidents: фильтры и курсоры

fn ids(page: &[&IncidentProjection<'_>]) -> Vec<String> {
    page.iter().map(|p| p.row.id.clone()).collect()
}

struct Run<'a> {
    projections: &'a [IncidentProjection<'a>],
}

impl<'a> Run<'a> {
    fn page(&self, f: IncidentFilters) -> Vec<&'a IncidentProjection<'a>> {
        query_incidents(self.projections, &f, "deadline", 100, None, 7)
            .unwrap()
            .0
    }

    fn sorted(&self, sort: &str) -> Vec<&'a IncidentProjection<'a>> {
        query_incidents(self.projections, &IncidentFilters::default(), sort, 100, None, 7)
            .unwrap()
            .0
    }
}

#[test]
fn filters() {
    let rows = rows();
    let (_, projections) = project_all(&rows, &ctx());
    let run = Run {
        projections: &projections,
    };
    let f = IncidentFilters::default;
    assert_eq!(run.page(f()).len(), 44);
    assert_eq!(
        run.page(IncidentFilters {
            group: Some(G::Research),
            ..f()
        })
        .len(),
        5
    );
    assert_eq!(
        ids(&run.page(IncidentFilters {
            group: Some(G::Coordinators),
            ..f()
        })),
        ids(&run.page(IncidentFilters {
            team: Some(COORD.to_lowercase()),
            state: Some(S::Assigned),
            ..f()
        }))
    );
    assert!(
        run.page(IncidentFilters {
            priority_id: Some(3),
            ..f()
        })
        .iter()
        .all(|p| p.row.priority_id == 3)
    );
    let q = |q: &str| {
        ids(&run.page(IncidentFilters {
            q: Some(q.into()),
            ..f()
        }))
    };
    assert_eq!(q("вечная ЗАЖИГАЛКА").iter().filter(|i| *i == DEMO_ID).count(), 1);
    assert_eq!(q("inc000021938167"), [DEMO_ID]);
}

#[test]
fn sort_deadline_nulls_last() {
    let rows = rows();
    let (_, projections) = project_all(&rows, &ctx());
    let run = Run {
        projections: &projections,
    };
    let deadlines: Vec<_> = run.sorted("deadline").iter().map(|p| p.row.deadline).collect();
    assert_eq!(deadlines[41..], [None, None, None]);
    assert!(deadlines[..41].windows(2).all(|w| w[0] <= w[1]));
    let desc: Vec<_> = run.sorted("-deadline").iter().map(|p| p.row.deadline).collect();
    assert!(desc[..41].windows(2).all(|w| w[0] >= w[1]));
    assert_eq!(desc[41..], [None, None, None]);
    let prio: Vec<_> = run.sorted("priority").iter().map(|p| p.row.priority_id).collect();
    assert!(prio.windows(2).all(|w| w[0] >= w[1]));
}

#[test]
fn cursor_pagination_covers_everything_once() {
    let rows = rows();
    let (_, projections) = project_all(&rows, &ctx());
    let f = IncidentFilters::default();
    let (mut seen, mut cursor) = (Vec::new(), None::<String>);
    loop {
        let (page, next) = query_incidents(&projections, &f, "deadline", 10, cursor.as_deref(), 7).unwrap();
        seen.extend(ids(&page));
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!((seen.len(), unique.len()), (44, 44));
    let all = query_incidents(&projections, &f, "deadline", 100, None, 7)
        .unwrap()
        .0;
    assert_eq!(seen, ids(&all));
}

#[test]
fn cursor_tied_to_revision() {
    let rows = rows();
    let (_, projections) = project_all(&rows, &ctx());
    let f = IncidentFilters::default();
    let (_, cursor) = query_incidents(&projections, &f, "deadline", 10, None, 7).unwrap();
    let cursor = cursor.unwrap();
    assert_eq!(decode_cursor(Some(&cursor), 7), Ok(10));
    let err = query_incidents(&projections, &f, "deadline", 10, Some(&cursor), 8).unwrap_err();
    assert_eq!(err, QueryError::CursorExpired { current_revision: 8 });
    assert_eq!(err.code(), "CURSOR_EXPIRED");
}

#[test]
fn bad_cursor() {
    let rows = rows();
    let (_, projections) = project_all(&rows, &ctx());
    for cursor in ["not-base64!", &encode_cursor(7, -1), "Zm9v"] {
        let err = query_incidents(
            &projections,
            &IncidentFilters::default(),
            "deadline",
            100,
            Some(cursor),
            7,
        )
        .unwrap_err();
        assert_eq!(err, QueryError::InvalidCursor, "{cursor}");
    }
}

#[test]
fn bad_sort() {
    let rows = rows();
    let (_, projections) = project_all(&rows, &ctx());
    let err = query_incidents(&projections, &IncidentFilters::default(), "color", 100, None, 7).unwrap_err();
    assert_eq!(err.code(), "INVALID_SORT");
    assert!(err.to_string().contains("sort"));
}
