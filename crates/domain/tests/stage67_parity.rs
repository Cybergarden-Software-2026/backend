//! Паритет с Python-версией (этап R3) на корпусе входов: `tests/fixtures/stage67_python.json`
//! построен настоящим Python-кодом (`gen_stage67.py`) — `build_groups` (членство и порядок),
//! `describe_resolution`, `assignee_of`, `compute_sla`, цепочки `next_resolution`,
//! `format_duration`, `parse_cursor`.
//!
//! Известное отличие, которое тест допускает: курсор из цифр без десятичного значения (`"²"`)
//! Python пропускает через `isdigit()` и падает в `int()` (`crash`, 500) — Rust отвечает 400.

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use dashboard_domain::constants::{NormalizedState, ResolutionPhase};
use dashboard_domain::cursors::parse_cursor;
use dashboard_domain::grouping::{
    Groupable, ResolutionState, assignee_of, build_groups, describe_resolution, next_resolution,
};
use dashboard_domain::sla::{compute_sla, format_duration};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/stage67_python.json")).expect("stage67_python.json")
}

fn dt(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str().map(|s| {
        DateTime::parse_from_rfc3339(s)
            .expect("isoformat")
            .with_timezone(&Utc)
    })
}

/// `datetime.isoformat()` для UTC: `+00:00`, микросекунды — только если не ноль.
fn iso(d: Option<DateTime<Utc>>) -> Value {
    d.map_or(Value::Null, |d| {
        let fmt = if d.timestamp_subsec_micros() == 0 {
            "%Y-%m-%dT%H:%M:%S+00:00"
        } else {
            "%Y-%m-%dT%H:%M:%S%.6f+00:00"
        };
        Value::String(d.format(fmt).to_string())
    })
}

fn state(v: &Value) -> NormalizedState {
    v.as_str().unwrap().parse().unwrap()
}

fn phase(v: &Value) -> Option<ResolutionPhase> {
    v.as_str().map(|s| s.parse().unwrap())
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

#[derive(Debug)]
struct Inc {
    id: String,
    sys_id: String,
    state: NormalizedState,
    priority_id: i64,
    group: Option<String>,
    employee: Option<String>,
    deadline: Option<DateTime<Utc>>,
    submit_date: DateTime<Utc>,
    completion_date: Option<DateTime<Utc>>,
    phase: Option<ResolutionPhase>,
    detected_at: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
}

impl Inc {
    fn decode(v: &Value) -> Self {
        Self {
            id: text(&v["id"]).unwrap(),
            sys_id: text(&v["sysId"]).unwrap(),
            state: state(&v["state"]),
            priority_id: v["priorityId"].as_i64().unwrap(),
            group: text(&v["group"]),
            employee: text(&v["employee"]),
            deadline: dt(&v["deadline"]),
            submit_date: dt(&v["submitDate"]).unwrap(),
            completion_date: dt(&v["completionDate"]),
            phase: phase(&v["phase"]),
            detected_at: dt(&v["detectedAt"]),
            until: dt(&v["until"]),
        }
    }
}

impl Groupable for Inc {
    fn id(&self) -> &str {
        &self.id
    }
    fn sys_id(&self) -> &str {
        &self.sys_id
    }
    fn normalized_state(&self) -> NormalizedState {
        self.state
    }
    fn priority_id(&self) -> i64 {
        self.priority_id
    }
    fn assigned_group_name(&self) -> Option<&str> {
        self.group.as_deref()
    }
    fn assigned_employee(&self) -> Option<&str> {
        self.employee.as_deref()
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
        self.phase
    }
    fn resolved_detected_at(&self) -> Option<DateTime<Utc>> {
        self.detected_at
    }
    fn recently_resolved_until(&self) -> Option<DateTime<Utc>> {
        self.until
    }
}

/// Сравнение с накоплением расхождений: печатаются первые, тест падает со счётчиком.
#[derive(Default)]
struct Diffs {
    total: usize,
    failed: Vec<String>,
}

impl Diffs {
    fn check(&mut self, what: impl FnOnce() -> String, rust: &Value, python: &Value) {
        self.total += 1;
        if rust != python {
            self.failed
                .push(format!("{}\n  rust:   {rust}\n  python: {python}", what()));
        }
    }

    fn finish(self, name: &str) {
        for f in self.failed.iter().take(10) {
            eprintln!("{f}");
        }
        assert!(
            self.failed.is_empty(),
            "{name}: {} из {} расходятся с Python",
            self.failed.len(),
            self.total
        );
        assert!(self.total > 0, "{name}: пустой корпус");
        eprintln!("{name}: {} проверок совпали с Python", self.total);
    }
}

#[test]
fn snapshots_match_python() {
    let fx = fixture();
    let mut d = Diffs::default();
    for (n, snap) in fx["snapshots"].as_array().unwrap().iter().enumerate() {
        let now = dt(&snap["now"]).unwrap();
        let coord = snap["coordinatorGroup"].as_str().unwrap();
        let warning = TimeDelta::seconds((snap["warningMinutes"].as_f64().unwrap() * 60.0) as i64);
        let incidents: Vec<Inc> = snap["incidents"]
            .as_array()
            .unwrap()
            .iter()
            .map(Inc::decode)
            .collect();

        let (groups, membership) = build_groups(&incidents, coord, now);
        let groups_json: Vec<Value> = groups
            .iter()
            .map(|g| json!({"key": g.key.as_str(), "title": g.title, "sysIds": g.sys_ids()}))
            .collect();
        d.check(
            || format!("снимок {n}: группы"),
            &Value::from(groups_json),
            &snap["groups"],
        );
        let membership_json: HashMap<&str, Vec<&str>> = membership
            .iter()
            .map(|(k, v)| (k.as_str(), v.iter().map(|g| g.as_str()).collect()))
            .collect();
        d.check(
            || format!("снимок {n}: членство"),
            &json!(membership_json),
            &snap["membership"],
        );

        for (i, inc) in incidents.iter().enumerate() {
            let resolution = describe_resolution(inc, now).map_or(Value::Null, |r| {
                json!({
                    "phase": r.phase.as_str(),
                    "label": r.label,
                    "resolvedAt": iso(Some(r.resolved_at)),
                    "source": r.resolved_at_source.as_str(),
                    "detectedAt": iso(Some(r.detected_at)),
                    "until": iso(r.recently_resolved_until),
                })
            });
            d.check(
                || format!("снимок {n}, {inc:?}: resolution"),
                &resolution,
                &snap["resolution"][i],
            );
            d.check(
                || format!("снимок {n}, {inc:?}: assignee"),
                &json!(assignee_of(inc)),
                &snap["assignee"][i],
            );
            let sla = compute_sla(inc.deadline, inc.state, now, warning, inc.completion_date);
            let sla = json!({
                "status": sla.status.as_str(),
                "label": sla.label,
                "deadlineAt": iso(sla.deadline_at),
                "remainingSeconds": sla.remaining_seconds,
            });
            d.check(
                || format!("снимок {n}, {inc:?} (now {now}): sla"),
                &sla,
                &snap["sla"][i],
            );
        }
    }
    d.finish("снимки");
}

#[test]
fn resolution_transitions_match_python() {
    let fx = fixture();
    let mut d = Diffs::default();
    for (n, chain) in fx["transitions"].as_array().unwrap().iter().enumerate() {
        let mut current = ResolutionState::default();
        for (k, step) in chain["steps"].as_array().unwrap().iter().enumerate() {
            let window = TimeDelta::seconds((step["windowHours"].as_f64().unwrap() * 3600.0) as i64);
            current = next_resolution(
                &current,
                state(&step["prevState"]),
                step["prevPriority"].as_i64().unwrap(),
                state(&step["state"]),
                dt(&step["now"]).unwrap(),
                window,
            );
            let got = json!({
                "phase": current.phase.map(|p| p.as_str()),
                "detectedAt": iso(current.detected_at),
                "until": iso(current.recently_resolved_until),
            });
            d.check(
                || format!("цепочка {n}, шаг {k}: {step}"),
                &got,
                &step["expected"],
            );
        }
    }
    d.finish("переходы «Решён»");
}

#[test]
fn durations_match_python() {
    let fx = fixture();
    let mut d = Diffs::default();
    for case in fx["durations"].as_array().unwrap() {
        let seconds = case[0].as_i64().unwrap();
        d.check(
            || format!("format_duration({seconds})"),
            &json!(format_duration(seconds)),
            &case[1],
        );
    }
    d.finish("format_duration");
}

#[test]
fn cursors_match_python() {
    let fx = fixture();
    let mut d = Diffs::default();
    for case in fx["cursors"].as_array().unwrap() {
        let cursor = case["cursor"].as_str().unwrap();
        let got = match parse_cursor(Some(cursor), "cursor") {
            Ok(v) => json!({"cursor": cursor, "value": v}),
            Err(_) => json!({"cursor": cursor, "error": "invalid"}),
        };
        let mut expected = case.clone();
        if expected["error"] == "crash" {
            expected["error"] = json!("invalid"); // см. заголовок модуля
        }
        d.check(|| format!("parse_cursor({cursor:?})"), &got, &expected);
    }
    d.finish("курсоры");
}
