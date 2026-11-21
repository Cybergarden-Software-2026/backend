//! Порт `backend/tests/unit/test_stage6_grouping_sla.py` (без БД): четыре группы,
//! мультичленство, отметка «Решён», окно 24 ч, сортировка, SLA.

mod common;

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use dashboard_domain::constants::{GroupKey as G, NormalizedState as S, ResolutionPhase as P};
use dashboard_domain::grouping::{
    Groupable, ResolutionState, ResolvedAtSource, assignee_of, build_groups, compute_groups,
    describe_resolution, next_resolution,
};
use dashboard_domain::sla::{SlaResult, SlaStatus, compute_sla, format_duration};
use dashboard_domain::validator::validate_snapshot;
use serde_json::{Map, Value, json};

use common::{sample, values};

const COORD: &str = "КЦ ИТ Координаторы Массовых Инцидентов";
const DEMO_ID: &str = "INC000021938167";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
}

fn window() -> TimeDelta {
    TimeDelta::hours(24)
}

fn h(hours: i64) -> TimeDelta {
    TimeDelta::hours(hours)
}

/// Аналог dataclass `Inc` из Python-теста (значения по умолчанию те же).
#[derive(Debug, Clone, PartialEq)]
struct Inc {
    id: String,
    sys_id: String,
    normalized_state: S,
    priority_id: i64,
    assigned_group_name: Option<String>,
    assigned_employee: Option<String>,
    deadline: Option<DateTime<Utc>>,
    submit_date: DateTime<Utc>,
    completion_date: Option<DateTime<Utc>>,
    resolution_phase: Option<P>,
    resolved_detected_at: Option<DateTime<Utc>>,
    recently_resolved_until: Option<DateTime<Utc>>,
}

impl Default for Inc {
    fn default() -> Self {
        Self {
            id: "INC1".into(),
            sys_id: "1".into(),
            normalized_state: S::WorkInProgress,
            priority_id: 2,
            assigned_group_name: Some("ОГЭ Приморье".into()),
            assigned_employee: Some("Медведь Барсик (barsik)".into()),
            deadline: None,
            submit_date: now() - TimeDelta::days(1),
            completion_date: None,
            resolution_phase: None,
            resolved_detected_at: None,
            recently_resolved_until: None,
        }
    }
}

impl Inc {
    fn with_mark(mut self, r: ResolutionState) -> Self {
        self.resolution_phase = r.phase;
        self.resolved_detected_at = r.detected_at;
        self.recently_resolved_until = r.recently_resolved_until;
        self
    }
}

impl Groupable for Inc {
    fn id(&self) -> &str {
        &self.id
    }
    fn sys_id(&self) -> &str {
        &self.sys_id
    }
    fn normalized_state(&self) -> S {
        self.normalized_state
    }
    fn priority_id(&self) -> i64 {
        self.priority_id
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
    fn resolution_phase(&self) -> Option<P> {
        self.resolution_phase
    }
    fn resolved_detected_at(&self) -> Option<DateTime<Utc>> {
        self.resolved_detected_at
    }
    fn recently_resolved_until(&self) -> Option<DateTime<Utc>> {
        self.recently_resolved_until
    }
}

fn groups(inc: &Inc) -> Vec<G> {
    compute_groups(inc, COORD, now())
}

fn groups_at(inc: &Inc, at: DateTime<Utc>) -> Vec<G> {
    compute_groups(inc, COORD, at)
}

fn from_records(records: Vec<Map<String, Value>>) -> Vec<Inc> {
    validate_snapshot(values(records), Tz::UTC)
        .unwrap()
        .incidents
        .into_iter()
        .map(|n| {
            let c = n.columns;
            Inc {
                id: n.id,
                sys_id: n.sys_id,
                normalized_state: c.normalized_state,
                priority_id: c.priority_id.unwrap(),
                assigned_group_name: c.assigned_group_name,
                assigned_employee: c.assigned_employee,
                deadline: c.deadline,
                submit_date: c.submit_date.unwrap(),
                completion_date: c.completion_date,
                ..Inc::default()
            }
        })
        .collect()
}

fn set(record: &mut Map<String, Value>, patch: Value) {
    for (k, v) in patch.as_object().unwrap() {
        record.insert(k.clone(), v.clone());
    }
}

// ================================================================ правила групп

/// Стартовый экран по правилам API.md: 0 / 2 / 3 / 5.
#[test]
fn test_start_screen_counts_on_source_file() {
    let incidents = from_records(sample());
    let (result, membership) = build_groups(&incidents, COORD, now());
    let counts: Vec<_> = result.iter().map(|g| (g.key, g.incidents.len())).collect();
    assert_eq!(
        counts,
        [
            (G::Critical, 0),
            (G::Coordinators, 2),
            (G::High, 3),
            (G::Research, 5)
        ]
    );
    let titles: Vec<_> = result.iter().map(|g| g.title).collect();
    assert_eq!(
        titles,
        [
            "Критические",
            "На координаторах",
            "Высокий приоритет",
            "Исследование ситуации"
        ]
    );
    let high = result.iter().find(|g| g.key == G::High).unwrap();
    let mut ids = high.incident_ids();
    ids.sort();
    assert_eq!(ids, ["INC000021932639", DEMO_ID, "INC000021939297"]);
    assert_eq!(membership[DEMO_ID], [G::High]);
    assert_eq!(membership.len(), 44);
}

#[test]
fn test_group_rules() {
    let coord_spaced = format!("  {} ", COORD.to_uppercase()); // регистр/пробелы
    let cases: Vec<(Inc, Vec<G>)> = vec![
        (
            Inc {
                priority_id: 4,
                ..Inc::default()
            },
            vec![G::Critical],
        ),
        (
            Inc {
                priority_id: 4,
                normalized_state: S::Assigned,
                ..Inc::default()
            },
            vec![G::Critical],
        ),
        (
            Inc {
                priority_id: 4,
                normalized_state: S::Research,
                ..Inc::default()
            },
            vec![G::Critical, G::Research],
        ),
        (
            Inc {
                priority_id: 4,
                normalized_state: S::Resolved,
                ..Inc::default()
            },
            vec![],
        ),
        (
            Inc {
                priority_id: 4,
                normalized_state: S::Closed,
                ..Inc::default()
            },
            vec![],
        ),
        (
            Inc {
                priority_id: 3,
                ..Inc::default()
            },
            vec![G::High],
        ),
        (
            Inc {
                priority_id: 3,
                normalized_state: S::Research,
                ..Inc::default()
            },
            vec![G::High, G::Research],
        ),
        (
            Inc {
                priority_id: 3,
                normalized_state: S::Resolved,
                ..Inc::default()
            },
            vec![],
        ),
        (
            Inc {
                assigned_group_name: Some(COORD.into()),
                ..Inc::default()
            },
            vec![G::Coordinators],
        ),
        (
            Inc {
                assigned_group_name: Some(coord_spaced),
                ..Inc::default()
            },
            vec![G::Coordinators],
        ),
        (
            Inc {
                assigned_group_name: Some(COORD.into()),
                normalized_state: S::Research,
                ..Inc::default()
            },
            vec![G::Coordinators, G::Research],
        ),
        (
            Inc {
                assigned_group_name: Some(COORD.into()),
                normalized_state: S::Closed,
                ..Inc::default()
            },
            vec![],
        ),
        (
            Inc {
                normalized_state: S::Research,
                priority_id: 1,
                ..Inc::default()
            },
            vec![G::Research],
        ),
        (
            Inc {
                priority_id: 2,
                ..Inc::default()
            },
            vec![],
        ),
        (
            Inc {
                priority_id: 4,
                assigned_group_name: Some(COORD.into()),
                normalized_state: S::Unknown,
                ..Inc::default()
            },
            vec![],
        ),
    ];
    for (inc, expected) in cases {
        assert_eq!(groups(&inc), expected, "{inc:?}");
    }
}

/// PDF сл.4 / API.md §10 проверка 4: критический на координаторах виден в двух группах.
#[test]
fn test_incident_in_several_groups_at_once() {
    let inc = Inc {
        priority_id: 4,
        assigned_group_name: Some(COORD.into()),
        ..Inc::default()
    };
    assert_eq!(groups(&inc), [G::Critical, G::Coordinators]);
}

/// API.md §10 проверка 3: urgencyId=4 при priorityId=2 — не критический.
#[test]
fn test_urgency_and_impact_do_not_make_critical() {
    assert!(
        !groups(&Inc {
            priority_id: 2,
            ..Inc::default()
        })
        .contains(&G::Critical)
    );
}

// ================================================================ отметка «Решён»

struct Res {
    prev: ResolutionState,
    ps: S,
    pp: i64,
    cs: S,
    now: DateTime<Utc>,
}

impl Default for Res {
    fn default() -> Self {
        Self {
            prev: ResolutionState::default(),
            ps: S::WorkInProgress,
            pp: 4,
            cs: S::Resolved,
            now: now(),
        }
    }
}

fn res(r: Res) -> ResolutionState {
    next_resolution(&r.prev, r.ps, r.pp, r.cs, r.now, window())
}

fn mark(phase: P, detected: DateTime<Utc>, until: Option<DateTime<Utc>>) -> ResolutionState {
    ResolutionState {
        phase: Some(phase),
        detected_at: Some(detected),
        recently_resolved_until: until,
    }
}

#[test]
fn test_active_critical_resolved_gets_24h_window() {
    assert_eq!(
        res(Res::default()),
        mark(P::Resolved, now(), Some(now() + window()))
    );
}

#[test]
fn test_active_critical_closed_gets_window() {
    assert_eq!(
        res(Res {
            cs: S::Closed,
            ..Res::default()
        })
        .phase,
        Some(P::Closed)
    );
}

#[test]
fn test_active_critical_to_research_marked_without_time_limit() {
    assert_eq!(
        res(Res {
            cs: S::Research,
            ..Res::default()
        }),
        mark(P::Research, now(), None)
    );
}

/// Повышение до решения учитывает предыдущий приоритет (ARCHITECTURE.md).
#[test]
fn test_previous_priority_counts_when_priority_lowered_together() {
    let r = res(Res {
        pp: 4,
        ..Res::default()
    }); // одновременно priorityId 4 → 2 и Resolved
    let inc = Inc {
        priority_id: 2,
        normalized_state: S::Resolved,
        ..Inc::default()
    }
    .with_mark(r);
    assert_eq!(groups(&inc), [G::Critical]);
}

#[test]
fn test_non_critical_resolution_has_no_mark() {
    assert_eq!(
        res(Res {
            pp: 3,
            ..Res::default()
        }),
        ResolutionState::default()
    );
}

#[test]
fn test_research_then_resolved_starts_window() {
    let marked = res(Res {
        cs: S::Research,
        ..Res::default()
    });
    let later = now() + h(5);
    let r = res(Res {
        prev: marked,
        ps: S::Research,
        cs: S::Resolved,
        now: later,
        ..Res::default()
    });
    assert_eq!(r, mark(P::Resolved, now(), Some(later + window())));
}

#[test]
fn test_resolved_then_closed_keeps_original_window() {
    let marked = res(Res::default());
    let r = res(Res {
        prev: marked,
        ps: S::Resolved,
        cs: S::Closed,
        now: now() + h(3),
        ..Res::default()
    });
    assert_eq!(
        (r.phase, r.recently_resolved_until),
        (Some(P::Closed), Some(now() + window()))
    );
}

#[test]
fn test_reactivation_removes_mark_immediately() {
    for back_to in [S::Assigned, S::WorkInProgress] {
        let r = res(Res {
            prev: res(Res::default()),
            ps: S::Resolved,
            cs: back_to,
            ..Res::default()
        });
        assert_eq!(r, ResolutionState::default());
    }
}

#[test]
fn test_unknown_status_keeps_mark() {
    let marked = res(Res::default());
    assert_eq!(
        res(Res {
            prev: marked,
            ps: S::Resolved,
            cs: S::Unknown,
            ..Res::default()
        }),
        marked
    );
}

#[test]
fn test_resolved_critical_stays_24h_then_leaves() {
    let r = res(Res::default());
    let inc = Inc {
        priority_id: 4,
        normalized_state: S::Resolved,
        ..Inc::default()
    }
    .with_mark(r);
    assert_eq!(
        groups_at(&inc, now() + h(23) + TimeDelta::minutes(59)),
        [G::Critical]
    );
    assert_eq!(groups_at(&inc, now() + window()), []);
}

#[test]
fn test_describe_resolution_prefers_completion_date() {
    let r = res(Res {
        cs: S::Research,
        ..Res::default()
    });
    let completed = now() - TimeDelta::minutes(10);
    let inc = Inc {
        priority_id: 4,
        normalized_state: S::Research,
        completion_date: Some(completed),
        resolution_phase: r.phase,
        resolved_detected_at: r.detected_at,
        ..Inc::default()
    };
    let d = describe_resolution(&inc, now()).unwrap();
    assert_eq!((d.label, d.resolved_at), ("Решён · исследование", completed));
    assert_eq!(
        (d.resolved_at_source, d.recently_resolved_until),
        (ResolvedAtSource::CompletionDate, None)
    );
    assert_eq!(d.resolved_at_source.as_str(), "completionDate");
    let d2 = describe_resolution(
        &Inc {
            completion_date: None,
            ..inc
        },
        now(),
    )
    .unwrap();
    assert_eq!(
        (d2.resolved_at, d2.resolved_at_source),
        (now(), ResolvedAtSource::Detected)
    );
    assert_eq!(describe_resolution(&Inc::default(), now()), None);
}

/// Сценарий демонстрации на INC000021938167 (ARCHITECTURE.md / ТЗ §19).
#[test]
fn test_demo_scenario_groups() {
    let mut records = sample();
    let demo = records.iter_mut().find(|r| r["id"] == DEMO_ID).unwrap();

    // 1. priority 3 → 4 и назначение на координаторов: из «Высоких» в «Критические» и «На координаторах»
    set(
        demo,
        json!({"priorityId": 4, "priority": "Наивысший", "assignedGroupName": COORD}),
    );
    let inc = from_records(records)
        .into_iter()
        .find(|i| i.id == DEMO_ID)
        .unwrap();
    assert_eq!(groups(&inc), [G::Critical, G::Coordinators]);

    // 3. Research / 20 + completionDate, назначение на координаторов сохраняется
    let r = res(Res {
        ps: S::WorkInProgress,
        pp: 4,
        cs: S::Research,
        ..Res::default()
    });
    let inc = Inc {
        normalized_state: S::Research,
        completion_date: Some(now()),
        resolution_phase: r.phase,
        resolved_detected_at: r.detected_at,
        ..inc
    };
    assert_eq!(groups(&inc), [G::Critical, G::Coordinators, G::Research]);
    assert_eq!(
        describe_resolution(&inc, now()).unwrap().label,
        "Решён · исследование"
    );

    // 4. Переназначение инженерам: уходит из координаторов, остаётся в исследовании
    let inc = Inc {
        assigned_group_name: Some("ОГЭ Приморье".into()),
        ..inc
    };
    assert_eq!(groups(&inc), [G::Critical, G::Research]);
}

// ================================================================ сортировка

#[test]
fn test_sorting_by_nearest_deadline_without_deadline_last() {
    let inc = |id: &str, deadline| Inc {
        id: id.into(),
        priority_id: 3,
        deadline,
        ..Inc::default()
    };
    let all = [
        inc("A", Some(now() + h(5))),
        inc("B", None),
        inc("C", Some(now() - h(1))), // просроченный — раньше всех
        inc("D", Some(now() + h(1))),
    ];
    let (result, _) = build_groups(&all, COORD, now());
    let high = result.iter().find(|g| g.key == G::High).unwrap();
    assert_eq!(high.incident_ids(), ["C", "D", "A", "B"]);
}

#[test]
fn test_active_critical_before_resolved_in_critical_group() {
    let r = res(Res::default());
    let resolved = Inc {
        id: "R".into(),
        priority_id: 4,
        normalized_state: S::Resolved,
        deadline: Some(now() - TimeDelta::days(1)),
        resolution_phase: r.phase,
        resolved_detected_at: Some(now()),
        recently_resolved_until: r.recently_resolved_until,
        ..Inc::default()
    };
    let active = Inc {
        id: "A".into(),
        priority_id: 4,
        deadline: Some(now() + TimeDelta::days(2)),
        ..Inc::default()
    };
    let all = [resolved, active];
    let (result, _) = build_groups(&all, COORD, now());
    assert_eq!(result[0].incident_ids(), ["A", "R"]);
}

#[test]
fn test_assignee_falls_back_to_group_name() {
    assert_eq!(assignee_of(&Inc::default()), Some("Медведь Барсик (barsik)"));
    assert_eq!(
        assignee_of(&Inc {
            assigned_employee: None,
            ..Inc::default()
        }),
        Some("ОГЭ Приморье")
    );
}

// ================================================================ SLA

fn sla(deadline: Option<DateTime<Utc>>) -> SlaResult {
    sla_full(deadline, S::WorkInProgress, None)
}

fn sla_full(deadline: Option<DateTime<Utc>>, state: S, completion: Option<DateTime<Utc>>) -> SlaResult {
    compute_sla(deadline, state, now(), TimeDelta::minutes(60), completion)
}

#[test]
fn test_sla_no_deadline() {
    let r = sla(None);
    assert_eq!(
        (r.status, r.label.as_str(), r.remaining_seconds),
        (SlaStatus::NoDeadline, "Срок не задан", None)
    );
    assert_eq!(r.status.as_str(), "no_deadline");
}

#[test]
fn test_sla_ok_warning_overdue() {
    assert_eq!(
        sla(Some(now() + TimeDelta::days(1) + h(3))).label,
        "Осталось 1 д 3 ч"
    );
    let w = sla(Some(now() + TimeDelta::minutes(25)));
    assert_eq!(
        (w.status, w.label.as_str(), w.remaining_seconds),
        (SlaStatus::Warning, "Осталось 25 мин", Some(1500))
    );
    let o = sla(Some(now() - h(2) - TimeDelta::minutes(5)));
    assert_eq!(
        (o.status, o.label.as_str(), o.remaining_seconds),
        (SlaStatus::Overdue, "Просрочен на 2 ч 5 мин", Some(-7500))
    );
}

#[test]
fn test_sla_demo_incident_is_overdue_on_source_data() {
    let inc = from_records(sample())
        .into_iter()
        .find(|i| i.id == DEMO_ID)
        .unwrap();
    assert_eq!(sla(inc.deadline).status, SlaStatus::Overdue);
}

#[test]
fn test_sla_not_applicable_for_resolved_states() {
    let deadline = Some(now() + h(1));
    assert_eq!(sla_full(deadline, S::Resolved, Some(now())).label, "Решён в срок");
    assert_eq!(
        sla_full(deadline, S::Closed, Some(now() + h(2))).label,
        "Решён после срока"
    );
    let r = sla_full(deadline, S::Research, None);
    assert_eq!(
        (r.status, r.remaining_seconds, r.deadline_at),
        (SlaStatus::NotApplicable, None, deadline)
    );
    assert_eq!(r.label, "Инцидент решён");
}

#[test]
fn test_sla_does_not_mutate_deadline() {
    let deadline = Some(now() + h(3));
    assert_eq!(sla(deadline).deadline_at, deadline);
}

#[test]
fn test_format_duration() {
    let cases = [
        (30, "меньше минуты"),
        (60, "1 мин"),
        (3600, "1 ч"),
        (3660, "1 ч 1 мин"),
        (86_400, "1 д"),
        (90_000, "1 д 1 ч"),
    ];
    for (seconds, text) in cases {
        assert_eq!(format_duration(seconds), text);
    }
}
