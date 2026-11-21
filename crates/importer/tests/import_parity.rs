//! Паритет импортёра с настоящей Python-версией на PostgreSQL (этап R4).
//!
//! `fixtures/import_python.json` собран `gen_import_parity.py`: три сценария по 121 шагу
//! (правки полей, выбывание/возврат записей, новые записи, перестановки, отметка «Решён» и
//! её истечение, битый и пустой JSON, удаление файла, некорректные снимки, ошибка БД, повторная
//! запись того же файла, «файл не менялся») прогнаны через `ImporterWorker` Python.
//! Тест повторяет шаги через Rust-импортёр и сравнивает:
//! * итог каждого шага (`status`, `revision`, счётчики, `message`, `sourceStatus`);
//! * после каждого шага — md5 реестра, строку `import_state` и максимальные id журналов;
//! * в конце — полный дамп `incidents`, `incident_changes`, `change_events`, `dashboard_events`,
//!   `notifications`, `import_state` (запросы дампа берутся из файла — одни и те же для обеих версий).
//!
//! Сравнение точное, без нормализации: тексты ошибок (включая синтаксические ошибки JSON,
//! `domain::pyjson`) и имена исключений БД совпадают. Эталон собран Python 3.12, как в образе
//! backend. Нужен `TEST_DATABASE_URL`.

mod common;

use common::*;
use serde_json::{Map, Value, json};
use sqlx::AssertSqlSafe;

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/import_python.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("import_python.json")).unwrap()
}

/// Запись из правки исходной (`encode` в генераторе).
fn decode(spec: &Value, sample: &[Record]) -> Value {
    if let Some(value) = spec.get("value") {
        return value.clone();
    }
    let mut record = sample[spec["base"].as_u64().unwrap() as usize].clone();
    for key in spec.get("unset").and_then(Value::as_array).into_iter().flatten() {
        record.shift_remove(key.as_str().unwrap());
    }
    for (k, v) in spec.get("set").and_then(Value::as_object).into_iter().flatten() {
        record.insert(k.clone(), v.clone());
    }
    if let Some(order) = spec.get("order").and_then(Value::as_array) {
        let mut reordered = Map::new();
        for key in order {
            let key = key.as_str().unwrap();
            reordered.insert(key.to_owned(), record[key].clone());
        }
        record = reordered;
    }
    Value::Object(record)
}

/// Первое расхождение двух JSON-значений: путь и обе стороны.
fn first_diff(path: &str, a: &Value, b: &Value) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                let (va, vb) = (x.get(k).unwrap_or(&Value::Null), y.get(k).unwrap_or(&Value::Null));
                if x.contains_key(k) != y.contains_key(k) {
                    return Some(format!("{path}.{k}: python={va} rust={vb} (key presence)"));
                }
                if let Some(d) = first_diff(&format!("{path}.{k}"), va, vb) {
                    return Some(d);
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) => {
            for (i, (va, vb)) in x.iter().zip(y).enumerate() {
                if let Some(d) = first_diff(&format!("{path}[{i}]"), va, vb) {
                    return Some(d);
                }
            }
            (x.len() != y.len()).then(|| format!("{path}: python len {} != rust len {}", x.len(), y.len()))
        }
        _ => (a != b).then(|| format!("{path}: python={a} rust={b}")),
    }
}

fn outcome_json(o: &dashboard_importer::ImportOutcome) -> Value {
    json!({
        "status": o.status.as_str(),
        "revision": o.revision,
        "created": o.created,
        "updated": o.updated,
        "removed": o.removed,
        "restored": o.restored,
        "message": o.message,
        "sourceStatus": o.source_status.map(|s| s.as_str()),
    })
}

async fn query_json(e: &Env, sql: &str) -> Value {
    sqlx::query_scalar::<_, Value>(AssertSqlSafe(sql.to_owned()))
        .fetch_one(e.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn importer_matches_python_on_scenarios() {
    if std::env::var("TEST_DATABASE_URL").is_err() {
        eprintln!("skip: TEST_DATABASE_URL is not set");
        return;
    }
    let fixture = fixture();
    assert_eq!(&fixture["python"].as_str().unwrap()[..5], "3.12.");
    let sample = records();
    let settings: Vec<(String, String)> = fixture["settings"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
        .collect();
    let vars: Vec<(&str, &str)> = settings.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let step_sql = fixture["step_sql"].as_str().unwrap();
    let mut checked_steps = 0;

    for scenario in fixture["scenarios"].as_array().unwrap() {
        let seed = &scenario["seed"];
        let mut e = Env::with_vars(&vars).await.unwrap();
        let steps = scenario["steps"].as_array().unwrap();
        let results = scenario["results"].as_array().unwrap();
        for (i, (step, expected)) in steps.iter().zip(results).enumerate() {
            match step["action"].as_str().unwrap() {
                "write" => {
                    let recs: Vec<Value> = step["records"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|s| decode(s, &sample))
                        .collect();
                    let tmp = e.source.with_extension("tmp");
                    std::fs::write(&tmp, serde_json::to_vec_pretty(&recs).unwrap()).unwrap();
                    std::fs::rename(&tmp, &e.source).unwrap();
                }
                "text" => e.write_text(step["text"].as_str().unwrap()),
                "delete" => {
                    let _ = std::fs::remove_file(&e.source);
                }
                "none" => {}
                other => panic!("unknown action {other}"),
            }
            let outcome = e.tick_after(step["advance"].as_f64().unwrap()).await;
            let got = json!({
                "outcome": outcome_json(&outcome),
                "db": query_json(&e, step_sql).await,
            });
            let want = expected.clone();
            if let Some(diff) = first_diff("", &want, &got) {
                panic!(
                    "seed {seed}, step {i} ({} +{}s): {diff}\npython: {}\nrust:   {}",
                    step["action"], step["advance"], want["outcome"], got["outcome"]
                );
            }
            checked_steps += 1;
        }
        for (table, sql) in fixture["dump_sql"].as_object().unwrap() {
            let got = query_json(&e, sql.as_str().unwrap()).await;
            let want = &scenario["dump"][table];
            if let Some(diff) = first_diff(table, want, &got) {
                panic!("seed {seed}, final dump: {diff}");
            }
        }
        e.finish().await;
    }
    assert_eq!(checked_steps, 363);
}
