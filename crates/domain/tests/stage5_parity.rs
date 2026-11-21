//! Паритет с Python-версией на корпусе входов: `tests/fixtures/stage5_python.json` построен
//! настоящим Python-кодом (`gen_stage5.py`) — `datetime.fromisoformat`, `parse_source_datetime`,
//! `normalize_record` (колонки, `content_hash`), `parse_and_validate` (тексты ошибок) и
//! `build_import_plan` (виды, изменения, типы событий, отметка «Решён», уведомления).
//!
//! Известные отличия, которые тест допускает:
//! * текст синтаксической ошибки JSON в скобках и позиция (`serde_json` vs модуль `json`);
//! * там, где Python падает необработанным исключением (`crash`), Rust возвращает ошибку поля.

mod common;

use chrono_tz::Tz;
use dashboard_domain::change_detector::{ImportPlan, PlanOptions, StoredIncident, build_import_plan};
use dashboard_domain::grouping::ResolutionState;
use dashboard_domain::normalizer::{Columns, iso_z, normalize_record, parse_source_datetime};
use dashboard_domain::pydatetime::fromisoformat;
use dashboard_domain::validator::{parse_and_validate, validate_snapshot};
use serde_json::{Map, Value, json};

use common::{decode, sample};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/stage5_python.json")).expect("stage5_python.json")
}

fn tz(name: &str) -> Tz {
    name.parse().expect("зона")
}

fn opt_iso(d: Option<chrono::DateTime<chrono::Utc>>) -> Value {
    d.as_ref().map_or(Value::Null, |d| Value::String(iso_z(d)))
}

/// Сравнение с накоплением расхождений: печатаются первые, тест падает со счётчиком.
struct Diffs {
    name: &'static str,
    total: usize,
    failed: Vec<String>,
}

impl Diffs {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            total: 0,
            failed: Vec::new(),
        }
    }

    fn check(&mut self, ok: bool, what: impl FnOnce() -> String) {
        self.total += 1;
        if !ok {
            self.failed.push(what());
        }
    }

    fn finish(self) {
        assert!(self.total > 0, "{}: пустой корпус", self.name);
        if !self.failed.is_empty() {
            let shown: Vec<_> = self.failed.iter().take(15).cloned().collect();
            panic!(
                "{}: {} из {} расходятся с Python:\n{}",
                self.name,
                self.failed.len(),
                self.total,
                shown.join("\n")
            );
        }
    }
}

#[test]
fn fromisoformat_matches_cpython() {
    let data = fixture();
    let mut diffs = Diffs::new("datetime.fromisoformat");
    for case in data["dates"]["fromisoformat"].as_array().unwrap() {
        let text = case[0].as_str().unwrap();
        let expected = &case[1];
        let actual = match fromisoformat(text) {
            Ok(p) => json!([p.naive.format("%Y-%m-%dT%H:%M:%S%.6f").to_string(), p.offset_us]),
            Err(_) => Value::Null,
        };
        diffs.check(&actual == expected, || {
            format!("{text:?}: Rust {actual}, Python {expected}")
        });
    }
    diffs.finish();
}

#[test]
fn parse_source_datetime_matches_python() {
    let data = fixture();
    let mut diffs = Diffs::new("parse_source_datetime");
    for case in data["dates"]["parse"].as_array().unwrap() {
        let (value, zone, expected) = (&case[0], tz(case[1].as_str().unwrap()), &case[2]);
        let actual = parse_source_datetime(value, zone);
        let ok = match (&actual, expected) {
            (Ok(d), e) if e.get("ok").is_some() => opt_iso(*d) == e["ok"],
            (Err(msg), e) if e.get("err").is_some() => e["err"] == msg.as_str(),
            (Err(_), e) => e.get("crash").is_some(), // Python падал — у нас ошибка поля
            _ => false,
        };
        diffs.check(ok, || {
            format!("{value} [{}]: Rust {actual:?}, Python {expected}", case[1])
        });
    }
    diffs.finish();
}

fn columns_json(c: &Columns) -> Map<String, Value> {
    let s = |v: &Option<String>| v.clone().map_or(Value::Null, Value::String);
    let i = |v: &Option<i64>| v.map_or(Value::Null, Value::from);
    let b = |v: &Option<bool>| v.map_or(Value::Null, Value::Bool);
    let mut m = Map::new();
    macro_rules! put {
        ($f:ident, $conv:expr) => {
            m.insert(stringify!($f).into(), $conv(&c.$f));
        };
    }
    put!(sys_id, s);
    put!(id, s);
    put!(state_label, s);
    put!(state_enum, s);
    put!(state_id, i);
    put!(state_reason, s);
    put!(state_reason_id, s);
    put!(submit_date, |d: &Option<_>| opt_iso(*d));
    put!(deadline, |d: &Option<_>| opt_iso(*d));
    put!(completion_date, |d: &Option<_>| opt_iso(*d));
    put!(last_assignment_to_team_at, |d: &Option<_>| opt_iso(*d));
    put!(header, s);
    put!(description, s);
    put!(short_description, s);
    put!(assigned_group_name, s);
    put!(assigned_employee, s);
    put!(assigned_employee_login, s);
    put!(initiator_name, s);
    put!(initiator_login, s);
    put!(is_effect, b);
    put!(is_reason, b);
    put!(is_double, b);
    put!(is_master, b);
    put!(inc_type, s);
    put!(service_type_id, i);
    put!(service_type, s);
    put!(priority_id, i);
    put!(priority, s);
    put!(urgency_id, i);
    put!(urgency, s);
    put!(impact_id, i);
    put!(impact, s);
    put!(waiting_exit_date, |d: &Option<_>| opt_iso(*d));
    put!(is_root_incident, b);
    put!(product_name, s);
    put!(product_id, s);
    put!(product_master_id, s);
    put!(product_url, s);
    put!(service, s);
    put!(service_id, s);
    put!(scenario_name, s);
    put!(is_need_up, b);
    put!(elimination_type, s);
    put!(elimination_type_id, s);
    put!(receiving_channel, s);
    put!(solution, s);
    m.insert("normalized_state".into(), json!(c.normalized_state.as_str()));
    m.insert("status_mismatch".into(), json!(c.status_mismatch));
    m
}

#[test]
fn normalize_record_matches_python() {
    let data = fixture();
    let sample = sample();
    let mut diffs = Diffs::new("normalize_record");
    for (n, case) in data["records"].as_array().unwrap().iter().enumerate() {
        let Value::Object(raw) = decode(&case["record"], &sample) else {
            panic!("запись")
        };
        let result = normalize_record(raw, tz(case["tz"].as_str().unwrap()));
        match (&result, case.get("error")) {
            (Err(e), Some(expected)) => {
                diffs.check(expected == &e.to_string(), || format!("#{n}: {e} vs {expected}"))
            }
            (Err(_), None) if case.get("crash").is_some() => diffs.check(true, String::new),
            (Ok(inc), None) if case.get("crash").is_none() => {
                diffs.check(inc.content_hash == case["hash"], || {
                    let tracked = Value::Object(inc.tracked.clone());
                    format!(
                        "#{n}: hash; tracked {tracked} vs {}",
                        case.get("tracked").unwrap_or(&Value::Null)
                    )
                });
                if let Some(tracked) = case.get("tracked") {
                    diffs.check(&Value::Object(inc.tracked.clone()) == tracked, || {
                        format!("#{n}: tracked")
                    });
                }
                let columns = Value::Object(columns_json(&inc.columns));
                diffs.check(columns == case["columns"], || {
                    format!("#{n}: columns {columns} vs {}", case["columns"])
                });
                let state = json!([
                    inc.state.state.as_str(),
                    inc.state.recognized,
                    inc.state.mismatch,
                    inc.state.warning
                ]);
                diffs.check(state == case["state"], || {
                    format!("#{n}: state {state} vs {}", case["state"])
                });
            }
            (r, e) => diffs.check(false, || format!("#{n}: Rust {r:?}, Python {e:?}")),
        }
    }
    diffs.finish();
}

#[test]
fn validation_matches_python() {
    let data = fixture();
    let sample = sample();
    let mut diffs = Diffs::new("parse_and_validate");
    let msk = tz("Europe/Moscow");
    for (n, case) in data["validation"].as_array().unwrap().iter().enumerate() {
        let result = if let Some(records) = case.get("records") {
            let items = records
                .as_array()
                .unwrap()
                .iter()
                .map(|s| decode(s, &sample))
                .collect();
            validate_snapshot(Value::Array(items), msk)
        } else if let Some(text) = case.get("text") {
            parse_and_validate(text.as_str().unwrap().as_bytes(), msk)
        } else {
            let bytes = base64_decode(case["base64"].as_str().unwrap());
            parse_and_validate(&bytes, msk)
        };
        match (&result, case.get("rejected")) {
            (Err(e), Some(expected)) => {
                let expected = expected.as_str().unwrap();
                let actual = e.summary();
                // Синтаксис JSON описывают разные парсеры — сравнивается только начало.
                let ok = if expected.starts_with("некорректный JSON") {
                    actual.starts_with("некорректный JSON")
                } else {
                    actual == expected && case["issues"] == e.issues.len()
                };
                diffs.check(ok, || format!("#{n}:\n  Rust   {actual}\n  Python {expected}"));
            }
            (Ok(snap), None) => {
                let ids: Vec<_> = snap.incidents.iter().map(|i| i.id.as_str()).collect();
                let hashes: Vec<_> = snap.incidents.iter().map(|i| i.content_hash.as_str()).collect();
                diffs.check(
                    json!(ids) == case["ids"] && json!(hashes) == case["hashes"],
                    || format!("#{n}: ids/hashes"),
                );
                diffs.check(json!(snap.warnings) == case["warnings"], || {
                    format!("#{n}: warnings {:?} vs {}", snap.warnings, case["warnings"])
                });
            }
            (r, e) => diffs.check(false, || {
                format!(
                    "#{n}: Rust {:?}, Python {e:?}",
                    r.as_ref().map(|s| s.incidents.len())
                )
            }),
        }
    }
    diffs.finish();
}

fn base64_decode(s: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes().filter(|c| *c != b'=') {
        buf = (buf << 6) | ALPHABET.iter().position(|a| *a == c).expect("base64") as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    out
}

fn parse_dt(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    v.as_str()
        .map(|s| chrono::DateTime::parse_from_rfc3339(s).expect("дата").to_utc())
}

fn plan_json(plan: &ImportPlan) -> Value {
    let res = |r: &Option<ResolutionState>| match r {
        None => Value::Null,
        Some(r) => json!([
            r.phase.map(|p| p.as_str()),
            opt_iso(r.detected_at),
            opt_iso(r.recently_resolved_until)
        ]),
    };
    json!({
        "is_initial": plan.is_initial,
        "hash_updates": plan.hash_updates.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<_, _>>(),
        "items": plan.items.iter().map(|i| json!({
            "kind": i.kind.as_str(),
            "id": i.incident_id,
            "pk": i.pk,
            "rev": i.new_revision,
            "prev": i.previous_state.map(|s| s.as_str()),
            "cur": i.current_state.map(|s| s.as_str()),
            "changes": i.changes.iter().map(|c| json!([c.field, c.old, c.new])).collect::<Vec<_>>(),
            "types": i.event_types.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            "resolution": res(&i.resolution),
            "transitions": [i.transitions.entered_critical, i.transitions.entered_coordinators, i.transitions.critical_resolved],
            "notifications": i.notifications.iter()
                .map(|d| json!([d.kind.as_str(), d.title, d.body, d.dedup_key, d.payload]))
                .collect::<Vec<_>>(),
            "header": i.header(),
        })).collect::<Vec<_>>(),
    })
}

#[test]
fn import_plans_match_python() {
    let data = fixture();
    let sample = sample();
    let now = parse_dt(&data["now"]).unwrap();
    let mut diffs = Diffs::new("build_import_plan");
    for (n, scenario) in data["plans"].as_array().unwrap().iter().enumerate() {
        let stored: Vec<StoredIncident> = scenario["stored"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(k, spec)| {
                let Value::Object(raw) = decode(spec, &sample) else {
                    panic!("запись")
                };
                let inc = normalize_record(raw.clone(), Tz::UTC).expect("сохранённая запись");
                let meta = &spec["meta"];
                let mut s = StoredIncident::new(
                    k as i64 + 1,
                    &inc.id,
                    &inc.sys_id,
                    raw,
                    meta["content_hash"].as_str().unwrap_or(&inc.content_hash),
                    meta["revision"].as_i64().unwrap(),
                    meta["present"].as_bool().unwrap(),
                    inc.state.state,
                );
                s.priority_id = inc.columns.priority_id.unwrap();
                s.assigned_group_name = inc.columns.assigned_group_name.clone();
                s.resolution_phase = meta["phase"].as_str().map(|p| p.parse().unwrap());
                s.resolved_detected_at = parse_dt(&meta["detected"]);
                s.recently_resolved_until = parse_dt(&meta["until"]);
                s
            })
            .collect();
        let records = scenario["incoming"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| decode(s, &sample))
            .collect();
        let incoming = validate_snapshot(Value::Array(records), Tz::UTC)
            .expect("снимок сценария")
            .incidents;
        let mut options = PlanOptions::new(scenario["is_initial"].as_bool().unwrap());
        options.now = Some(now);
        let plan = build_import_plan(&stored, incoming, &options);
        let actual = plan_json(&plan);
        let expected = &scenario["plan"];
        diffs.check(&actual == expected, || {
            let a = actual["items"].as_array().unwrap();
            let e = expected["items"].as_array().unwrap();
            let first = a.iter().zip(e).find(|(x, y)| x != y);
            format!(
                "сценарий #{n}: {} vs {} элементов; первое расхождение {first:#?}",
                a.len(),
                e.len()
            )
        });
    }
    diffs.finish();
}
