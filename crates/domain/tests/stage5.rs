//! Порт `backend/tests/unit/test_stage5_importer.py` (без БД): даты и статусы, валидация,
//! нормализация и хэш, план импорта. Тесты чтения файла (`reader`) переносятся в R4 вместе с ним.

mod common;

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use dashboard_domain::change_detector::{
    ImportPlan, PlanOptions, StoredIncident, build_import_plan, classify_event_types,
};
use dashboard_domain::constants::{ChangeEventType as T, ChangeType, NormalizedState as S};
use dashboard_domain::normalizer::{
    NormalizedIncident, normalize_record, parse_source_datetime, resolve_state,
};
use dashboard_domain::validator::{SnapshotRejected, parse_and_validate, validate_snapshot};
use serde_json::{Map, Value, json};

use common::{SAMPLE_JSON, sample, values};

const DEMO_ID: &str = "INC000021938167";
const UTC: Tz = Tz::UTC;

fn by_id<'a>(records: &'a mut [Map<String, Value>], id: &str) -> &'a mut Map<String, Value> {
    records.iter_mut().find(|r| r["id"] == id).expect("запись")
}

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
}

fn set(record: &mut Map<String, Value>, patch: Value) {
    for (k, v) in patch.as_object().unwrap() {
        record.insert(k.clone(), v.clone());
    }
}

fn accepted() -> Vec<NormalizedIncident> {
    validate_snapshot(values(sample()), UTC).unwrap().incidents
}

fn stored(incidents: &[NormalizedIncident], revision: i64, present: bool) -> Vec<StoredIncident> {
    incidents
        .iter()
        .enumerate()
        .map(|(n, i)| {
            StoredIncident::new(
                n as i64 + 1,
                &i.id,
                &i.sys_id,
                i.raw.clone(),
                &i.content_hash,
                revision,
                present,
                i.state.state,
            )
        })
        .collect()
}

fn plan(stored: &[StoredIncident], records: Vec<Map<String, Value>>) -> ImportPlan {
    build_import_plan(
        stored,
        validate_snapshot(values(records), UTC).unwrap().incidents,
        &PlanOptions::new(false),
    )
}

fn rejected(result: Result<impl std::fmt::Debug, SnapshotRejected>) -> SnapshotRejected {
    result.expect_err("снимок должен быть отклонён")
}

// ================================================================ даты и статусы

#[test]
fn test_parse_source_datetime() {
    let cases: [(Value, Option<DateTime<Utc>>); 8] = [
        (json!("2026-11-08T12:02:29Z"), Some(utc(2026, 11, 8, 12, 2, 29))),
        (
            json!("2026-11-10T12:40:02.472263Z"),
            Some(utc(2026, 11, 10, 12, 40, 2) + chrono::TimeDelta::microseconds(472263)),
        ),
        (
            json!("2026-11-10T15:40:02+03:00"),
            Some(utc(2026, 11, 10, 12, 40, 2)),
        ),
        (json!("2026-11-10 12:40:02"), Some(utc(2026, 11, 10, 12, 40, 2))), // формат справочника
        (json!("0001-01-01T00:00:00"), None),                               // заглушка
        (json!("0001-01-01T00:00:00Z"), None),
        (json!("  "), None),
        (Value::Null, None),
    ];
    for (value, expected) in cases {
        assert_eq!(parse_source_datetime(&value, UTC).unwrap(), expected, "{value}");
    }
}

#[test]
fn test_naive_datetime_uses_configured_timezone() {
    let msk: Tz = "Europe/Moscow".parse().unwrap();
    assert_eq!(
        parse_source_datetime(&json!("2026-11-10 15:00:00"), msk).unwrap(),
        Some(utc(2026, 11, 10, 12, 0, 0))
    );
}

#[test]
fn test_parse_source_datetime_rejects_garbage() {
    for value in [
        json!("вчера"),
        json!("2026-13-40T00:00:00Z"),
        json!(1731234567),
        json!("10.11.2026"),
    ] {
        assert!(parse_source_datetime(&value, UTC).is_err(), "{value}");
    }
}

#[test]
fn test_resolve_state() {
    let cases = [
        (Some("Assigned"), Some(0), S::Assigned, false),
        (Some("WorkInProgress"), Some(2), S::WorkInProgress, false),
        (Some("Resolved"), Some(7), S::Resolved, false), // справочник называет 7 «Закрыт» — верим выгрузке
        (Some("Closed"), Some(10), S::Closed, false),
        (Some("Research"), Some(20), S::Research, false), // нет в справочнике
        (None, Some(20), S::Research, false),             // fallback по коду
        (Some("research"), None, S::Research, false),     // регистр не важен
        (Some("Research"), Some(2), S::Research, true),   // противоречие: побеждает stateEnum
        (Some("Paused"), Some(99), S::Unknown, false),
    ];
    for (enum_, code, state, mismatch) in cases {
        let res = resolve_state(enum_, code);
        assert_eq!((res.state, res.mismatch), (state, mismatch), "{enum_:?} {code:?}");
        assert_eq!(res.recognized, state != S::Unknown);
    }
}

// ================================================================ валидация

#[test]
fn test_source_file_is_accepted_with_44_incidents() {
    let snap = parse_and_validate(SAMPLE_JSON.as_bytes(), UTC).unwrap();
    assert_eq!(snap.incidents.len(), 44);
    assert!(snap.warnings.is_empty());
    let mut states: Vec<_> = snap.incidents.iter().map(|i| i.state.state).collect();
    states.sort();
    states.dedup();
    assert_eq!(
        states,
        [
            S::Assigned,
            S::WorkInProgress,
            S::Resolved,
            S::Closed,
            S::Research
        ]
    );
    let demo = snap.incidents.iter().find(|i| i.id == DEMO_ID).unwrap();
    assert_eq!(demo.columns.waiting_exit_date, None); // заглушка
    assert_eq!(demo.columns.deadline, Some(utc(2026, 11, 10, 12, 32, 14)));
    assert_eq!(demo.columns.sys_id.as_deref(), Some("179429233402440192"));
    assert_eq!(demo.raw["waitingExitDate"], "0001-01-01T00:00:00");
}

#[test]
fn test_broken_files_rejected() {
    let cases: [(&[u8], &str); 6] = [
        (b"", "файл пуст"),
        (b"{\"id\": \"INC1\",", "некорректный JSON: строка 1"),
        (b"[{\"id\": \"INC1\",", "некорректный JSON: строка 1"),
        (b"[NaN]", "некорректный JSON"),
        (b"\xff\xfe[]", "UTF-8"),
        (b"[1]", "ожидался объект"),
    ];
    for (content, fragment) in cases {
        let exc = rejected(parse_and_validate(content, UTC));
        assert!(
            exc.summary().contains(fragment),
            "{} !~ {fragment}",
            exc.summary()
        );
    }
    let exc = rejected(parse_and_validate(b"{\"id\": \"INC1\"}", UTC));
    assert!(exc.summary().contains("ожидался JSON-массив"));
}

#[test]
fn test_empty_array_is_valid_empty_snapshot() {
    assert!(parse_and_validate(b"[]", UTC).unwrap().incidents.is_empty());
}

#[test]
fn test_record_errors_reject_whole_snapshot() {
    type Mutate = fn(&mut Map<String, Value>);
    let cases: [(Mutate, &str); 14] = [
        (|r| drop(r.shift_remove("id")), "id: обязательное поле"),
        (|r| set(r, json!({"id": "  "})), "id: обязательное поле"),
        (
            |r| set(r, json!({"sysId": 179413934903718013_i64})),
            "sysId: обязательное поле",
        ),
        (
            |r| drop(r.shift_remove("submitDate")),
            "submitDate: обязательное поле",
        ),
        (
            |r| set(r, json!({"submitDate": "0001-01-01T00:00:00"})),
            "submitDate: дата-заглушка",
        ),
        (
            |r| set(r, json!({"submitDate": "вчера"})),
            "submitDate: нераспознаваемая дата",
        ),
        (
            |r| set(r, json!({"stateEnum": null, "stateId": null})),
            "статус отсутствует",
        ),
        (
            |r| set(r, json!({"priorityId": 5})),
            "priorityId: допустимы значения 1–4",
        ),
        (
            |r| set(r, json!({"priorityId": "4"})),
            "priorityId: допустимы значения 1–4",
        ),
        (|r| set(r, json!({"priorityId": true})), "priorityId"),
        (
            |r| set(r, json!({"deadline": "20.11.2026"})),
            "deadline: нераспознаваемая дата",
        ), // C8
        (|r| set(r, json!({"completionDate": "soon"})), "completionDate"),
        (
            |r| set(r, json!({"stateId": "два"})),
            "stateId: ожидалось целое число",
        ),
        (|r| set(r, json!({"isDouble": "может быть"})), "isDouble"),
    ];
    for (mutate, fragment) in cases {
        let mut records = sample();
        mutate(&mut records[3]);
        let exc = rejected(validate_snapshot(values(records), UTC));
        let summary = exc.summary();
        assert!(summary.contains(fragment), "{summary} !~ {fragment}");
        assert!(
            summary.contains("запись #4") || summary.contains("повторяется"),
            "{summary}"
        );
    }
}

#[test]
fn test_duplicate_ids_rejected() {
    for key in ["id", "sysId"] {
        let mut records = sample();
        records[5][key] = records[2][key].clone();
        let exc = rejected(validate_snapshot(values(records), UTC));
        assert!(exc.summary().contains("повторяется 2 раз"), "{}", exc.summary());
    }
}

#[test]
fn test_null_optional_fields_accepted() {
    let mut records = sample();
    for key in [
        "deadline",
        "completionDate",
        "serviceId",
        "solution",
        "isNeedUp",
        "stateReason",
        "header",
    ] {
        records[0].insert(key.into(), Value::Null);
    }
    records[0].insert("isNeedUp".into(), json!("Да"));
    let snap = validate_snapshot(values(records), UTC).unwrap();
    let inc = &snap.incidents[0];
    assert_eq!(inc.columns.deadline, None);
    assert_eq!(inc.columns.is_need_up, Some(true));
}

#[test]
fn test_unknown_status_is_warning_not_rejection() {
    let mut records = sample();
    set(&mut records[0], json!({"stateEnum": "Suspended", "stateId": 99}));
    let snap = validate_snapshot(values(records), UTC).unwrap();
    assert_eq!(snap.incidents[0].state.state, S::Unknown);
    assert_eq!(snap.warnings.len(), 1);
    assert!(snap.warnings[0].contains("нераспознанный статус"));
}

#[test]
fn test_many_errors_summarized() {
    let mut records = sample();
    for r in &mut records {
        r.insert("priorityId".into(), json!(9));
    }
    let exc = rejected(validate_snapshot(values(records), UTC));
    assert_eq!(exc.issues.len(), 44);
    assert!(exc.summary().contains("и ещё ошибок: 34"));
}

// ================================================================ нормализация и хэш

#[test]
fn test_whitespace_reordering_and_date_format_do_not_change_hash() {
    let mut records = sample();
    let raw = by_id(&mut records, DEMO_ID).clone();
    let base = normalize_record(raw.clone(), UTC).unwrap();
    let mut noisy: Map<String, Value> = raw
        .iter()
        .rev()
        .map(|(k, v)| {
            (
                k.clone(),
                v.as_str()
                    .map_or_else(|| v.clone(), |s| json!(format!("  {s}  "))),
            )
        })
        .collect();
    noisy.insert("deadline".into(), json!("2026-11-10T15:32:14.000+03:00")); // тот же момент времени
    noisy.insert("isNeedUp".into(), json!("")); // пустая строка == null
    assert_eq!(
        normalize_record(noisy, UTC).unwrap().content_hash,
        base.content_hash
    );
}

#[test]
fn test_unknown_extra_fields_are_tracked() {
    let mut raw = sample().remove(0);
    let base = normalize_record(raw.clone(), UTC).unwrap().content_hash;
    raw.insert("newItsmField".into(), json!("value"));
    assert_ne!(normalize_record(raw, UTC).unwrap().content_hash, base);
}

#[test]
fn test_every_source_field_reaches_a_column() {
    let mut records = sample();
    let inc = normalize_record(by_id(&mut records, DEMO_ID).clone(), UTC).unwrap();
    assert_eq!(inc.columns.state_label.as_deref(), Some("Выполняется"));
    assert_eq!(
        inc.columns.last_assignment_to_team_at,
        Some(utc(2026, 11, 11, 3, 3, 15))
    );
    assert_eq!(inc.columns.priority_id, Some(3));
    assert_eq!(inc.columns.normalized_state, S::WorkInProgress);
    assert_eq!(inc.columns.is_root_incident, Some(true));
    assert_eq!(inc.columns.product_url, None);
}

// ================================================================ план импорта

#[test]
fn test_initial_import_creates_all() {
    let plan = build_import_plan(&[], accepted(), &PlanOptions::new(true));
    assert!(plan.is_initial);
    assert_eq!((plan.count(ChangeType::Created), plan.items.len()), (44, 44));
    assert!(plan.items.iter().all(|i| i.new_revision == 1));
    // Два инцидента на координаторах получают тип assigned; уведомлений при первичной загрузке нет.
    let mut assigned: Vec<_> = plan
        .items
        .iter()
        .filter(|i| i.event_types == [T::Created, T::Assigned])
        .map(|i| i.incident_id.as_str())
        .collect();
    assigned.sort();
    assert_eq!(assigned, ["INC000021931151", "INC000021938960"]);
    assert!(
        plan.items
            .iter()
            .all(|i| i.event_types == [T::Created] || i.event_types == [T::Created, T::Assigned])
    );
    assert!(plan.items.iter().all(|i| i.notifications.is_empty()));
}

#[test]
fn test_same_snapshot_shuffled_produces_no_changes() {
    let mut records = sample();
    // Детерминированная перестановка (в Python — random.Random(1).shuffle).
    records.reverse();
    records.rotate_left(17);
    for r in &mut records {
        let header = format!(" {} ", r["header"].as_str().unwrap());
        r.insert("header".into(), json!(header));
    }
    let plan = plan(&stored(&accepted(), 1, true), records);
    assert!(!plan.has_changes() && plan.hash_updates.is_empty());
}

#[test]
fn test_hash_only_difference_is_silent() {
    let acc = accepted();
    let mut st = stored(&acc, 1, true);
    st[0].content_hash = "old-normalizer-hash".into();
    let plan = plan(&st, sample());
    assert!(!plan.has_changes());
    assert_eq!(
        plan.hash_updates.into_iter().collect::<Vec<_>>(),
        [(1, acc[0].content_hash.clone())]
    );
}

#[test]
fn test_new_changed_and_removed_incidents() {
    let mut records = sample();
    let removed = records.remove(0);
    let mut new = records[0].clone();
    set(
        &mut new,
        json!({"id": "INC000099999999", "sysId": "999999999999999999"}),
    );
    records.push(new);
    by_id(&mut records, DEMO_ID).insert("description".into(), json!("Новое описание"));

    let plan = plan(&stored(&accepted(), 3, true), records);
    assert_eq!(
        (
            plan.count(ChangeType::Created),
            plan.count(ChangeType::Updated),
            plan.count(ChangeType::Removed)
        ),
        (1, 1, 1)
    );
    let find = |k| plan.items.iter().find(|i| i.kind == k).unwrap();

    let created = find(ChangeType::Created);
    assert_eq!(created.incident_id, "INC000099999999");
    assert_eq!(created.event_types, [T::Created]);
    let upd = find(ChangeType::Updated);
    assert_eq!((upd.incident_id.as_str(), upd.new_revision), (DEMO_ID, 4));
    assert_eq!(upd.changed_fields(), ["description"]);
    assert_eq!(upd.event_types, [T::Changed]);
    let rem = find(ChangeType::Removed);
    assert_eq!(rem.incident_id, removed["id"].as_str().unwrap());
    assert!(rem.current.is_none());
    assert_eq!(rem.event_types, [T::Removed]);
    assert_eq!(rem.previous_raw.as_ref(), Some(&removed));
}

#[test]
fn test_restored_incident() {
    let mut st = stored(&accepted(), 1, true);
    st[0].incident_revision = 2;
    st[0].is_present = false;
    let plan = plan(&st, sample());
    assert_eq!(plan.items.len(), 1);
    let item = &plan.items[0];
    // Возврат в снимок — повторный вход в группу координаторов
    assert_eq!((item.kind, item.new_revision), (ChangeType::Restored, 3));
    assert_eq!(item.event_types, [T::Created, T::Assigned]);
}

#[test]
fn test_demo_step_1_priority_and_team() {
    let mut records = sample();
    set(
        by_id(&mut records, DEMO_ID),
        json!({"priorityId": 4, "priority": "Наивысший", "assignedGroupName": "КЦ ИТ Координаторы Массовых Инцидентов"}),
    );
    let plan = plan(&stored(&accepted(), 1, true), records);
    let [item] = plan.items.as_slice() else {
        panic!("один элемент")
    };
    let mut fields = item.changed_fields();
    fields.sort();
    assert_eq!(fields, ["assignedGroupName", "priority", "priorityId"]);
    assert_eq!(item.event_types, [T::Critical, T::Assigned, T::Team, T::Priority]);
}

#[test]
fn test_demo_step_2_deadline_only() {
    let mut records = sample();
    by_id(&mut records, DEMO_ID).insert("deadline".into(), json!("2026-11-25T12:00:00Z"));
    let plan = plan(&stored(&accepted(), 1, true), records);
    let [item] = plan.items.as_slice() else {
        panic!("один элемент")
    };
    assert_eq!(item.changed_fields(), ["deadline"]);
    assert_eq!(item.event_types, [T::Deadline]);
    assert_eq!(
        (&item.changes[0].old, &item.changes[0].new),
        (&json!("2026-11-10T12:32:14Z"), &json!("2026-11-25T12:00:00Z"))
    );
}

#[test]
fn test_demo_step_3_research() {
    let mut records = sample();
    set(
        by_id(&mut records, DEMO_ID),
        json!({"stateEnum": "Research", "stateId": 20, "state": "Исследование ситуации", "completionDate": "2026-11-20T15:00:00Z"}),
    );
    let plan = plan(&stored(&accepted(), 1, true), records);
    let [item] = plan.items.as_slice() else {
        panic!("один элемент")
    };
    assert_eq!(
        (item.previous_state, item.current_state),
        (Some(S::WorkInProgress), Some(S::Research))
    );
    assert_eq!(item.event_types, [T::Research]);
}

#[test]
fn test_status_transitions_are_distinguished() {
    // API.md §10, проверка 5: Resolved, Closed и возврат в работу различаются.
    let cases = [
        (S::WorkInProgress, S::Resolved, vec![T::Resolved]),
        (S::Resolved, S::Closed, vec![T::Closed]),
        (S::Resolved, S::WorkInProgress, vec![T::Reopened]),
        (S::Research, S::Assigned, vec![T::Reopened]),
        (S::Assigned, S::WorkInProgress, vec![T::Changed]),
    ];
    for (prev, cur, expected) in cases {
        let types = classify_event_types(
            ChangeType::Updated,
            Some(prev),
            Some(cur),
            &["stateEnum", "stateId", "state"],
        );
        assert_eq!(types, expected, "{prev} → {cur}");
    }
}

#[test]
fn test_solution_change_does_not_change_status() {
    // API.md §10, проверка 6: заполненный solution не меняет статус.
    let mut records = sample();
    by_id(&mut records, DEMO_ID).insert("solution".into(), json!("Заменена прокладка, проблема устранена"));
    let plan = plan(&stored(&accepted(), 1, true), records);
    let [item] = plan.items.as_slice() else {
        panic!("один элемент")
    };
    assert_eq!(item.current_state, Some(S::WorkInProgress));
    assert_eq!(item.event_types, [T::Changed]);
}

#[test]
fn test_multiple_types_in_one_event() {
    let mut records = sample();
    set(
        by_id(&mut records, DEMO_ID),
        json!({"stateEnum": "Resolved", "stateId": 7, "deadline": null, "priorityId": 2}),
    );
    let plan = plan(&stored(&accepted(), 1, true), records);
    let [item] = plan.items.as_slice() else {
        panic!("один элемент")
    };
    assert_eq!(item.event_types, [T::Resolved, T::Deadline, T::Priority]);
}

#[test]
fn test_offset_change_equivalent_time_is_not_a_change() {
    let mut records = sample();
    // 2026-11-10T12:32:14Z в зоне +05:00 (`astimezone(timezone(timedelta(hours=5))).isoformat()`).
    by_id(&mut records, DEMO_ID).insert("deadline".into(), json!("2026-11-10T17:32:14+05:00"));
    assert!(!plan(&stored(&accepted(), 1, true), records).has_changes());
}
