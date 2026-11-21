//! Паритет REST API с настоящей Python-версией (этап R5).
//!
//! `fixtures/api_python.json` собран `gen_api_parity.py`: два сценария по 43 шага (битый файл до
//! первого снимка, затем шаги генератора паритета импортёра — правки, выбывание и возврат записей,
//! отметка «Решён», ошибки источника) прогнаны через Python-импортёр, и после каждого шага —
//! набор запросов к настоящему приложению FastAPI (2,2 тыс. ответов: снимки, фильтры, сортировки,
//! цепочки курсоров, 404/409/503, ошибки параметров с текстами pydantic, отметки прочтения
//! уведомлений разными пользователями, 307/405, OpenAPI и страницы документации).
//!
//! Тест повторяет шаги Rust-импортёром (его паритет с Python проверяет `import_parity.rs`) на
//! тех же часах и отправляет те же запросы в `router()`. Сравнение точное: статус, `content-type`,
//! `location`, `x-request-id` и тело **байт в байт** (большие тела — по sha256), включая
//! `serverTime`, `computedAt` и `remainingSeconds`. Нужен `TEST_DATABASE_URL`.

#[path = "../../importer/tests/common/mod.rs"]
mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use common::*;
use dashboard_api::{AppState, router};
use http_body_util::BodyExt;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/api_python.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("api_python.json")).unwrap()
}

/// Запись из правки исходной (формат `encode` генератора паритета импортёра).
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

const HEADERS: [&str; 3] = ["content-type", "location", "x-request-id"];

struct Got {
    status: u16,
    headers: Map<String, Value>,
    body: String,
}

async fn send(app: &axum::Router, req: &Value) -> Got {
    let mut builder = Request::builder()
        .method(req["m"].as_str().unwrap())
        .uri(req["p"].as_str().unwrap())
        .header("host", "test");
    for (k, v) in req["h"].as_object().into_iter().flatten() {
        builder = builder.header(k.as_str(), v.as_str().unwrap());
    }
    let res = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let mut headers = Map::new();
    for name in HEADERS {
        if let Some(v) = res.headers().get(name) {
            headers.insert(name.into(), Value::String(v.to_str().unwrap().to_owned()));
        }
    }
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    Got {
        status,
        headers,
        body: String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    }
}

/// Первое расхождение ответа с эталоном.
fn compare(want: &Value, got: &Got) -> Option<String> {
    let mut problems = Vec::new();
    if want["status"].as_u64() != Some(u64::from(got.status)) {
        problems.push(format!("status python={} rust={}", want["status"], got.status));
    }
    let empty = Map::new();
    let want_headers = want["headers"].as_object().unwrap_or(&empty);
    if *want_headers != got.headers {
        problems.push(format!(
            "headers python={} rust={}",
            Value::Object(want_headers.clone()),
            Value::Object(got.headers.clone())
        ));
    }
    if let Some(body) = want.get("body").and_then(Value::as_str) {
        if body != got.body {
            let at = body
                .chars()
                .zip(got.body.chars())
                .take_while(|(a, b)| a == b)
                .count();
            let tail = |s: &str| {
                s.chars()
                    .skip(at.saturating_sub(80))
                    .take(240)
                    .collect::<String>()
            };
            problems.push(format!(
                "body differs at char {at}\n  python: …{}\n  rust:   …{}",
                tail(body),
                tail(&got.body)
            ));
        }
    } else {
        let digest = hex::encode(Sha256::digest(got.body.as_bytes()));
        if want["sha256"].as_str() != Some(digest.as_str()) {
            let prefix: String = got.body.chars().take(600).collect();
            problems.push(format!(
                "body sha256 differs (python len {}, rust len {}); rust: {prefix}…\n  \
                 (полные тела Python: API_PARITY_BIG=100000000 python3.12 gen_api_parity.py)",
                want["len"],
                got.body.chars().count()
            ));
        }
    }
    (!problems.is_empty()).then(|| problems.join("\n  "))
}

#[tokio::test]
async fn rest_api_matches_python() {
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
    let (mut checked, mut failures) = (0usize, Vec::new());

    for scenario in fixture["scenarios"].as_array().unwrap() {
        let seed = &scenario["seed"];
        let mut e = Env::with_vars(&vars).await.unwrap();
        let state = AppState::with_clock(Arc::clone(&e.settings), e.pool().clone(), e.clock.as_clock());
        let app = router(state);
        let steps = scenario["steps"].as_array().unwrap();
        let results = scenario["results"].as_array().unwrap();
        assert_eq!(results.len(), steps.len() + 1);
        for (i, result) in results.iter().enumerate() {
            let label = if i == 0 {
                "до первого снимка".to_owned()
            } else {
                let step = &steps[i - 1];
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
                assert_eq!(
                    outcome.status.as_str(),
                    result["outcome"]["status"].as_str().unwrap(),
                    "seed {seed}, step {}: import outcome",
                    i - 1
                );
                format!("шаг {} ({})", i - 1, step["action"])
            };
            for want in result["responses"].as_array().unwrap() {
                let got = send(&app, &want["req"]).await;
                checked += 1;
                if let Some(diff) = compare(want, &got) {
                    failures.push(format!(
                        "seed {seed}, {label}: {} {}\n  {diff}",
                        want["req"]["m"].as_str().unwrap(),
                        want["req"]["p"].as_str().unwrap()
                    ));
                }
            }
        }
        e.finish().await;
    }
    if !failures.is_empty() {
        let shown: Vec<&String> = failures.iter().take(15).collect();
        panic!(
            "{} из {checked} ответов расходятся с Python:\n{}",
            failures.len(),
            shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
        );
    }
    assert_eq!(checked, 2237);
}
