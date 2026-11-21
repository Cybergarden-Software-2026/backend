//! Паритет SSE с настоящей Python-версией (этап R6).
//!
//! `fixtures/sse_python.json` собран `gen_sse_parity.py`: сценарий из 29 шагов (битый файл до
//! первого снимка, шаг с дробными числами в исходных записях, затем шаги генератора паритета
//! импортёра) прогнан через Python-импортёр, и после каждого шага — 28–30 потоков к настоящему
//! uvicorn: `/events/stream` в режиме snapshot, notify со всеми видами курсоров (в журнале, вне
//! его, нецифровые, Unicode-цифры, `Last-Event-ID` против `since`), `/events?since=…`,
//! `Accept: text/event-stream`, ошибки параметров.
//!
//! Тест повторяет шаги Rust-импортёром на тех же часах, запускает настоящий сервер axum и читает
//! те же потоки. Сравнение точное: статус, заголовки (`content-type`, `cache-control`,
//! `x-accel-buffering`, `connection`, `transfer-encoding`, `content-length`, `x-request-id`),
//! первые события **байт в байт** (`data:` снимков — по sha256, включая `serverTime`, SLA и запись
//! дробных чисел как у `model_dump_json`), закрытие потока после `stream.reset`. Отдельно — запись
//! 4 тыс. чисел `pycompat::float_pydantic` против pydantic-core. Нужен `TEST_DATABASE_URL`
//! (кроме проверки чисел).

#[path = "../../importer/tests/common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use dashboard_api::AppState;
use dashboard_domain::pycompat;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sse_python.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("sse_python.json")).unwrap()
}

#[test]
fn floats_are_written_like_model_dump_json() {
    let fixture = fixture();
    let floats = fixture["floats"].as_array().unwrap();
    let mut failures = Vec::new();
    for pair in floats {
        let bits = hex::decode(pair[0].as_str().unwrap()).unwrap();
        let x = f64::from_le_bytes(bits.try_into().unwrap());
        let want = pair[1].as_str().unwrap();
        let got = pycompat::float_pydantic(x);
        if got != want {
            failures.push(format!("{x:e}: pydantic {want}, rust {got}"));
        }
        // Через сериализатор целиком: число внутри массива.
        let doc = pycompat::dumps_pydantic(&json!([x]));
        assert_eq!(doc, format!("[{want}]"));
    }
    assert!(
        failures.is_empty(),
        "{} расхождений:\n{}",
        failures.len(),
        failures[..failures.len().min(20)].join("\n")
    );
    assert!(floats.len() > 4000);
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

const HEADERS: [&str; 7] = [
    "content-type",
    "cache-control",
    "x-accel-buffering",
    "connection",
    "transfer-encoding",
    "content-length",
    "x-request-id",
];
const BIG: usize = 2048;

fn block_repr(block: &str) -> Value {
    if block.chars().count() <= BIG {
        return Value::String(block.to_owned());
    }
    json!({
        "sha256": hex::encode(Sha256::digest(block.as_bytes())),
        "len": block.chars().count(),
        "head": block.chars().take(60).collect::<String>(),
    })
}

/// Ответ потока в том же виде, что пишет генератор.
async fn read_stream(client: &reqwest::Client, base: &str, req: &Value) -> Value {
    let mut builder = client.get(format!("{base}{}", req["p"].as_str().unwrap()));
    for (k, v) in req["h"].as_object().into_iter().flatten() {
        builder = builder.header(k.as_str(), v.as_str().unwrap());
    }
    let mut res = builder.send().await.unwrap();
    let mut headers = Map::new();
    for name in HEADERS {
        if let Some(v) = res.headers().get(name) {
            headers.insert(name.into(), Value::String(v.to_str().unwrap().to_owned()));
        }
    }
    let mut rec = json!({"req": req, "status": res.status().as_u16(), "headers": headers});
    let is_stream = res
        .headers()
        .get("content-type")
        .is_some_and(|v| v.to_str().unwrap().starts_with("text/event-stream"));
    if !is_stream {
        rec["body"] = Value::String(res.text().await.unwrap());
        return rec;
    }
    let n = req["n"].as_u64().map(|n| n as usize);
    let (mut buf, mut blocks, mut closed) = (Vec::<u8>::new(), Vec::new(), false);
    let read = async {
        loop {
            while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
                if n.is_some_and(|n| blocks.len() >= n) {
                    break;
                }
                let block: Vec<u8> = buf.drain(..pos + 2).collect();
                blocks.push(block_repr(std::str::from_utf8(&block).unwrap()));
            }
            if n.is_some_and(|n| blocks.len() >= n) {
                break;
            }
            match res.chunk().await.unwrap() {
                Some(chunk) => buf.extend_from_slice(&chunk),
                None => {
                    closed = true;
                    break;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .unwrap_or_else(|_| panic!("таймаут потока {req}"));
    rec["blocks"] = Value::Array(blocks);
    if n.is_none() {
        assert!(closed && buf.is_empty(), "{req}: поток не закрыт");
        rec["closed"] = Value::Bool(true);
    }
    rec
}

fn describe_diff(want: &Value, got: &Value) -> String {
    let mut out = Vec::new();
    for key in ["status", "headers", "body", "closed"] {
        if want.get(key) != got.get(key) {
            out.push(format!(
                "{key}: python={} rust={}",
                want.get(key).unwrap_or(&Value::Null),
                got.get(key).unwrap_or(&Value::Null)
            ));
        }
    }
    let (w, g) = (&want["blocks"], &got["blocks"]);
    if w != g {
        let (wa, ga) = (
            w.as_array().cloned().unwrap_or_default(),
            g.as_array().cloned().unwrap_or_default(),
        );
        for i in 0..wa.len().max(ga.len()) {
            if wa.get(i) != ga.get(i) {
                let short = |v: Option<&Value>| {
                    v.map(|v| v.to_string().chars().take(300).collect::<String>())
                        .unwrap_or_default()
                };
                out.push(format!(
                    "block {i}:\n    python: {}\n    rust:   {}",
                    short(wa.get(i)),
                    short(ga.get(i))
                ));
                break;
            }
        }
    }
    out.join("\n  ")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_streams_match_python() {
    if std::env::var("TEST_DATABASE_URL").is_err() {
        eprintln!("skip: TEST_DATABASE_URL is not set");
        return;
    }
    let fixture = fixture();
    let sample = records();
    let settings: Vec<(String, String)> = fixture["settings"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
        .collect();
    let vars: Vec<(&str, &str)> = settings.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let (mut checked, mut failures) = (0usize, Vec::new());

    for scenario in fixture["scenarios"].as_array().unwrap() {
        let seed = &scenario["seed"];
        let mut e = Env::with_vars(&vars).await.unwrap();
        let state = AppState::with_clock(Arc::clone(&e.settings), e.pool().clone(), e.clock.as_clock());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(dashboard_api::serve(listener, state, async {
            let _ = stopped.await;
        }));

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
                        std::fs::write(&tmp, serde_json::to_string_pretty(&recs).unwrap()).unwrap();
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
                let got = read_stream(&client, &base, &want["req"]).await;
                checked += 1;
                if got != *want {
                    failures.push(format!(
                        "seed {seed}, {label}: GET {} {}\n  {}",
                        want["req"]["p"].as_str().unwrap(),
                        want["req"]["h"],
                        describe_diff(want, &got)
                    ));
                }
            }
        }
        let _ = stop.send(());
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("сервер не остановился")
            .unwrap()
            .unwrap();
        e.finish().await;
    }
    if !failures.is_empty() {
        panic!(
            "{} из {checked} потоков расходятся с Python:\n{}",
            failures.len(),
            failures.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
        );
    }
    assert_eq!(checked, 836);
}
