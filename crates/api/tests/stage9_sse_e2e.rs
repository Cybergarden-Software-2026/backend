//! Порт `backend/tests/integration/test_stage9_sse.py`: SSE end-to-end — настоящий сервер axum на
//! случайном порту, HTTP-клиент, импортёр и PostgreSQL. Нужен `TEST_DATABASE_URL`.

#[path = "../../importer/tests/common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use dashboard_api::AppState;
use serde_json::{Value, json};

struct Server {
    e: Env,
    base: String,
    client: reqwest::Client,
    state: AppState,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start() -> Option<Self> {
        let e = Env::with_vars(&[
            ("SSE_HEARTBEAT_SECONDS", "0.3"),
            ("SSE_POLL_INTERVAL_SECONDS", "0.1"),
        ])
        .await?;
        let state = AppState::with_clock(Arc::clone(&e.settings), e.pool().clone(), e.clock.as_clock());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let served = state.clone();
        let task = tokio::spawn(async move {
            dashboard_api::serve(listener, served, async {
                let _ = rx.await;
            })
            .await
            .unwrap();
        });
        Some(Self {
            e,
            base: format!("http://127.0.0.1:{port}/api/v1"),
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
            state,
            stop: Some(tx),
            task,
        })
    }

    /// Плавная остановка должна завершиться, даже если клиенты не отключились.
    async fn finish(mut self) {
        let _ = self.stop.take().unwrap().send(());
        tokio::time::timeout(Duration::from_secs(10), &mut self.task)
            .await
            .expect("сервер не остановился: открытые потоки SSE держат плавную остановку")
            .unwrap();
        self.e.finish().await;
    }

    async fn publish(&mut self, recs: &[Record]) -> Instant {
        self.e.write_json(recs);
        let saved_at = Instant::now();
        self.e.tick().await;
        saved_at
    }

    async fn stream(&self, path: &str, headers: &[(&str, &str)]) -> Sse {
        let mut req = self.client.get(format!("{}{path}", self.base));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = req.send().await.unwrap();
        Sse {
            status: res.status().as_u16(),
            headers: res.headers().clone(),
            res,
            buf: Vec::new(),
        }
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap()
    }
}

/// Событие или комментарий: ключи `event`, `id`, `data`, `comment`, `retry`.
type Item = std::collections::HashMap<String, String>;

struct Sse {
    status: u16,
    headers: reqwest::header::HeaderMap,
    res: reqwest::Response,
    buf: Vec<u8>,
}

impl Sse {
    fn content_type(&self) -> &str {
        self.headers["content-type"].to_str().unwrap()
    }

    async fn line(&mut self) -> Option<String> {
        loop {
            if let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                return Some(String::from_utf8(line[..line.len() - 1].to_vec()).unwrap());
            }
            let chunk = self.res.chunk().await.unwrap()?;
            self.buf.extend_from_slice(&chunk);
        }
    }

    async fn next(&mut self, limit: Duration) -> Item {
        tokio::time::timeout(limit, async {
            let mut item = Item::new();
            loop {
                let line = self.line().await.expect("поток закрыт");
                if line.is_empty() {
                    if !item.is_empty() {
                        return item;
                    }
                    continue;
                }
                if let Some(c) = line.strip_prefix(':') {
                    item.insert("comment".into(), c.trim().into());
                    continue;
                }
                let (k, v) = line.split_once(": ").unwrap_or((&line, ""));
                match item.get_mut(k) {
                    Some(prev) if k == "data" => {
                        prev.push('\n');
                        prev.push_str(v);
                    }
                    _ => {
                        item.insert(k.into(), v.into());
                    }
                }
            }
        })
        .await
        .expect("таймаут ожидания события")
    }

    async fn next_event(&mut self, name: &str, limit: Duration) -> Item {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(100));
            let item = self.next(left).await;
            if item.get("event").map(String::as_str) == Some(name) {
                return item;
            }
        }
    }
}

const S5: Duration = Duration::from_secs(5);
const S2: Duration = Duration::from_secs(2);

fn data(item: &Item) -> Value {
    serde_json::from_str(&item["data"]).unwrap()
}

macro_rules! server_or_skip {
    () => {
        match Server::start().await {
            Some(s) => s,
            None => {
                eprintln!("skip: TEST_DATABASE_URL is not set");
                return;
            }
        }
    };
}

// ================================================================ режим snapshot (API.md §4)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_snapshot_stream_end_to_end_within_5_seconds() {
    let mut srv = server_or_skip!();
    let mut recs = records();
    srv.publish(&recs).await;
    let mut sse = srv.stream("/events/stream", &[]).await;
    assert_eq!(sse.status, 200);
    assert!(sse.content_type().starts_with("text/event-stream"));
    assert!(
        sse.headers["cache-control"]
            .to_str()
            .unwrap()
            .starts_with("no-cache")
    );
    assert_eq!(sse.next(S5).await["retry"], "2000");
    let first = sse.next_event("snapshot", S5).await;
    let snap = data(&first);
    assert_eq!(first["id"], "1");
    assert_eq!(snap["revision"], 1);
    assert_eq!(snap["items"].as_array().unwrap().len(), 44);
    assert!(snap["items"][0].get("sysId").is_some() && snap["items"][0].get("dashboard").is_some());
    // То же тело, что /snapshot (кроме времени ответа; часы в тесте стоят).
    let rest: Value = srv.get("/snapshot").await.json().await.unwrap();
    assert_eq!(snap, rest);

    update(
        demo(&mut recs),
        &[("priorityId", json!(4)), ("assignedGroupName", json!(COORD))],
    );
    let saved_at = srv.publish(&recs).await;
    let upd = sse.next_event("snapshot", S5).await;
    let latency = saved_at.elapsed();
    let d = data(&upd);
    assert_eq!(upd["id"], "2");
    assert_eq!(d["revision"], 2);
    let item = d["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == DEMO_ID)
        .unwrap();
    assert_eq!(item["priorityId"], 4);
    assert_eq!(item["dashboard"]["groups"], json!(["critical", "coordinators"]));
    assert_eq!(
        d["events"][0]["types"].as_array().unwrap()[..2],
        [json!("critical"), json!("assigned")]
    );
    assert!(
        latency < Duration::from_secs(5),
        "обновление дошло за {latency:?}"
    );
    drop(sse);
    srv.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_snapshot_stream_heartbeat_and_reconnect() {
    let mut srv = server_or_skip!();
    let mut recs = records();
    srv.publish(&recs).await;
    {
        let mut sse = srv.stream("/events/stream", &[]).await;
        sse.next_event("snapshot", S5).await;
        assert_eq!(sse.next(S2).await["comment"], "heartbeat");
    }
    demo(&mut recs).insert("deadline".into(), json!("2026-11-25T12:00:00Z")); // клиент отключён
    srv.publish(&recs).await;
    let mut sse = srv.stream("/events/stream", &[("Last-Event-ID", "1")]).await;
    let fresh = sse.next_event("snapshot", S5).await;
    assert_eq!(fresh["id"], "2"); // после переподключения — сразу актуальный снимок
    drop(sse);
    srv.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_snapshot_stream_waits_for_first_import() {
    let mut srv = server_or_skip!();
    let mut sse = srv.stream("/events/stream", &[]).await;
    sse.next(S5).await; // retry
    assert_eq!(sse.next(S2).await["comment"], "heartbeat"); // данных ещё нет
    srv.publish(&records()).await;
    assert_eq!(sse.next_event("snapshot", S5).await["id"], "1");
    drop(sse);
    srv.finish().await;
}

// ================================================================ режим notify (ARCHITECTURE.md)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_notify_mode_flow() {
    let mut srv = server_or_skip!();
    let mut recs = records();
    srv.publish(&recs).await;
    let dash: Value = srv.get("/dashboard").await.json().await.unwrap();
    let cursor = dash["eventCursor"].as_str().unwrap().to_owned();
    {
        let mut sse = srv
            .stream(&format!("/events/stream?mode=notify&since={cursor}"), &[])
            .await;
        assert_eq!(sse.next(S5).await["retry"], "2000");
        assert_eq!(sse.next(S2).await["comment"], "heartbeat");
        demo(&mut recs).insert("priorityId".into(), json!(4));
        srv.publish(&recs).await;
        let changed = sse.next_event("dashboard.changed", S5).await;
        assert_eq!(changed["id"], "2");
        assert_eq!(data(&changed), json!({"schemaVersion": 1, "reason": "import"}));
    }
    // после события клиент перечитывает снимок, и курсор совпадает
    let dash: Value = srv.get("/dashboard").await.json().await.unwrap();
    assert_eq!(dash["eventCursor"], "2");

    // переподключение: Last-Event-ID важнее since
    demo(&mut recs).insert("priorityId".into(), json!(3));
    srv.publish(&recs).await;
    let mut sse = srv
        .stream("/events/stream?mode=notify&since=999", &[("Last-Event-ID", "2")])
        .await;
    assert_eq!(sse.next_event("dashboard.changed", S5).await["id"], "3");
    drop(sse);
    srv.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_notify_mode_stale_cursor_resets() {
    let mut srv = server_or_skip!();
    srv.publish(&records()).await;
    let mut sse = srv.stream("/events/stream?mode=notify&since=500", &[]).await;
    let reset = sse.next_event("stream.reset", S5).await;
    assert_eq!(data(&reset)["reason"], "cursor_ahead");
    assert_eq!(reset["id"], "1");
    assert!(sse.line().await.is_none(), "поток закрыт");
    srv.finish().await;
}

/// `GET /events?since=` (ARCHITECTURE.md) отдаёт поток notify; без since — JSON-журнал.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_architecture_compatible_events_since() {
    let mut srv = server_or_skip!();
    let mut recs = records();
    srv.publish(&recs).await;
    {
        let mut sse = srv.stream("/events?since=1", &[]).await;
        assert!(sse.content_type().starts_with("text/event-stream"));
        demo(&mut recs).insert("priorityId".into(), json!(4));
        srv.publish(&recs).await;
        assert_eq!(sse.next_event("dashboard.changed", S5).await["id"], "2");
    }
    {
        let sse = srv.stream("/events", &[("Accept", "text/event-stream")]).await;
        assert!(sse.content_type().starts_with("text/event-stream"));
    }
    let journal = srv.get("/events").await;
    assert!(
        journal.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let body: Value = journal.json().await.unwrap();
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    srv.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_source_error_is_pushed() {
    let mut srv = server_or_skip!();
    srv.publish(&records()).await;
    let mut sse = srv.stream("/events/stream", &[]).await;
    sse.next_event("snapshot", S5).await;
    srv.e.write_text("[{");
    srv.e.tick().await;
    srv.e.tick().await;
    let d = data(&sse.next_event("snapshot", S5).await);
    assert_eq!(d["source"]["status"], "error");
    assert_eq!(d["items"].as_array().unwrap().len(), 44);
    drop(sse);
    srv.finish().await;
}

// ================================================================ только Rust-версия

/// Остановка сервера при открытых потоках обоих режимов: плавная остановка завершается,
/// клиенты видят конец потока.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn graceful_shutdown_closes_open_streams() {
    let mut srv = server_or_skip!();
    srv.publish(&records()).await;
    let mut a = srv.stream("/events/stream", &[]).await;
    let mut b = srv.stream("/events/stream?mode=notify", &[]).await;
    a.next_event("snapshot", S5).await;
    b.next(S5).await;
    let broadcaster = srv.state.broadcaster();
    srv.finish().await; // таймаут внутри — если потоки держат остановку
    assert!(broadcaster.is_closing());
    for s in [&mut a, &mut b] {
        tokio::time::timeout(S5, async { while s.line().await.is_some() {} })
            .await
            .expect("поток не закрылся");
    }
}

/// Ошибки параметров потока — 400 в формате API (как FastAPI), без открытия потока.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_parameter_errors() {
    let srv = server_or_skip!();
    let long = "1".repeat(33);
    let res = srv
        .client
        .get(format!("{}/events/stream?mode=bad&since={long}", srv.base))
        .header("Last-Event-ID", "9".repeat(33))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["code"], "VALIDATION_ERROR");
    assert_eq!(
        body["errors"],
        json!([
            {"loc": ["query", "mode"], "msg": "Input should be 'snapshot' or 'notify'", "type": "literal_error"},
            {"loc": ["query", "since"], "msg": "String should have at most 32 characters", "type": "string_too_long"},
            {"loc": ["header", "Last-Event-ID"], "msg": "String should have at most 32 characters", "type": "string_too_long"},
        ])
    );
    srv.finish().await;
}
