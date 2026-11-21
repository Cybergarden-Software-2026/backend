//! Порт `backend/tests/unit/test_stage9_sse.py` (без БД): формат SSE, broadcaster, режимы
//! snapshot и notify (ТЗ §15 «SSE»), плюс проверки, специфичные для Rust-версии (опоздавший
//! опрос, остановка сервера, отключение клиента, обрыв потока при ошибке БД).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dashboard_api::sse::{
    BoxFuture, Broadcaster, BuiltSnapshot, HEARTBEAT, Journal, JournalHead, NotifyStart, SseStream,
    format_event, notify_stream, plan_notify_start, snapshot_stream,
};
use serde_json::{Value, json};

const HB: Duration = Duration::from_millis(50);

#[derive(Default)]
struct FakeJournal {
    max_id: AtomicI64,
    min_id: Mutex<Option<i64>>,
    ready: Mutex<bool>,
    builds: AtomicUsize,
    fail_next: AtomicU32,
    /// Задержка ответа `fetch_head` по номеру вызова (для гонки опросов).
    delays: Mutex<HashMap<usize, Duration>>,
    fetches: AtomicUsize,
}

impl FakeJournal {
    fn new(max_id: i64, min_id: Option<i64>, ready: bool) -> Arc<Self> {
        let j = Self::default();
        j.max_id.store(max_id, Ordering::SeqCst);
        *j.min_id.lock().unwrap() = min_id;
        *j.ready.lock().unwrap() = ready;
        Arc::new(j)
    }
    fn simple(max_id: i64) -> Arc<Self> {
        Self::new(max_id, Some(1), true)
    }
    fn set(&self, max_id: i64) {
        self.max_id.store(max_id, Ordering::SeqCst);
    }
}

impl Journal for FakeJournal {
    fn fetch_head(&self) -> BoxFuture<'_, anyhow::Result<JournalHead>> {
        Box::pin(async move {
            let n = self.fetches.fetch_add(1, Ordering::SeqCst);
            let head = JournalHead {
                max_id: self.max_id.load(Ordering::SeqCst),
                min_id: *self.min_id.lock().unwrap(),
                reason: Some("import".into()),
            };
            let delay = self.delays.lock().unwrap().remove(&n);
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
            if self
                .fail_next
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
                .is_ok()
            {
                anyhow::bail!("db down");
            }
            Ok(head)
        })
    }

    fn build_snapshot(&self) -> BoxFuture<'_, anyhow::Result<Option<BuiltSnapshot>>> {
        Box::pin(async move {
            self.builds.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
            if !*self.ready.lock().unwrap() {
                return Ok(None);
            }
            let rev = self.max_id.load(Ordering::SeqCst);
            Ok(Some(dashboard_api::sse::snapshot_from_value(
                rev,
                &json!({"revision": rev, "items": []}),
            )))
        })
    }
}

fn broadcaster(j: &Arc<FakeJournal>) -> Arc<Broadcaster> {
    Broadcaster::new(Arc::clone(j) as Arc<dyn Journal>, Duration::from_millis(10))
}

async fn take(stream: &mut SseStream, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while out.len() < n {
            out.push(
                stream
                    .next_chunk()
                    .await
                    .expect("поток закрыт")
                    .expect("ошибка потока"),
            );
        }
    })
    .await
    .expect("таймаут");
    out
}

async fn rest(stream: &mut SseStream) -> Vec<String> {
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(chunk) = stream.next_chunk().await {
            out.push(chunk.unwrap());
        }
    })
    .await
    .expect("поток не закрылся");
    out
}

fn parse(chunk: &str) -> HashMap<String, String> {
    let mut fields: HashMap<String, String> = HashMap::new();
    for line in chunk.trim().split('\n') {
        if let Some(c) = line.strip_prefix(':') {
            fields.insert("comment".into(), c.trim().into());
            continue;
        }
        let (key, value) = line.split_once(": ").unwrap_or((line, ""));
        match fields.get_mut(key) {
            Some(prev) if key == "data" => {
                prev.push('\n');
                prev.push_str(value);
            }
            _ => {
                fields.insert(key.into(), value.into());
            }
        }
    }
    fields
}

fn data(chunk: &str) -> Value {
    serde_json::from_str(&parse(chunk)["data"]).unwrap()
}

// ================================================================ формат

#[test]
fn test_format_event() {
    let chunk = format_event("snapshot", r#"{"a":1}"#, Some(42), Some(2000));
    assert_eq!(chunk, "retry: 2000\nid: 42\nevent: snapshot\ndata: {\"a\":1}\n\n");
    assert_eq!(
        format_event("x", "l1\nl2", None, None),
        "event: x\ndata: l1\ndata: l2\n\n"
    );
    assert_eq!(format_event("x", "", None, None), "event: x\ndata: \n\n");
    assert_eq!(HEARTBEAT, ": heartbeat\n\n");
}

#[test]
fn test_plan_notify_start() {
    use NotifyStart::*;
    let h = |max, min| JournalHead::new(max, min);
    let cases: &[(Option<&str>, JournalHead, NotifyStart)] = &[
        (None, h(5, Some(1)), Live),
        (Some(""), h(5, Some(1)), Live),
        (Some("5"), h(5, Some(1)), Live),
        (Some("3"), h(5, Some(1)), Replay),
        (Some("0"), h(5, Some(1)), Replay), // min_id - 1 — ещё в журнале
        (Some("7"), h(5, Some(1)), Reset("cursor_ahead")),
        (Some("2"), h(9, Some(5)), Reset("cursor_expired")),
        (Some("abc"), h(5, Some(1)), Reset("invalid_cursor")),
        (Some("-1"), h(5, Some(1)), Reset("invalid_cursor")),
        // Сверх Python-тестов: поведение str.strip/isdigit/int.
        (Some("   "), h(5, Some(1)), Live),
        (Some(" 3\t"), h(5, Some(1)), Replay),
        (Some("\u{3000}5"), h(5, Some(1)), Live),
        (Some("٣"), h(5, Some(1)), Replay),
        (Some("+3"), h(5, Some(1)), Reset("invalid_cursor")),
        (Some("1_0"), h(50, Some(1)), Reset("invalid_cursor")),
        (Some("3.0"), h(5, Some(1)), Reset("invalid_cursor")),
        (
            Some("99999999999999999999999999999999"),
            h(5, Some(1)),
            Reset("cursor_ahead"),
        ),
        (Some("0"), h(0, None), Live),
        (Some("1"), h(0, None), Reset("cursor_ahead")),
        (Some("3"), h(5, None), Replay),
        // `"²"` isdigit() пропускает, int() падает: в Python поток обрывается, здесь — reset.
        (Some("²"), h(5, Some(1)), Reset("invalid_cursor")),
    ];
    for (cursor, head, expected) in cases {
        assert_eq!(plan_notify_start(*cursor, head), *expected, "{cursor:?} {head:?}");
    }
}

// ================================================================ режим snapshot (API.md)

#[tokio::test]
async fn test_snapshot_mode_first_connection_gets_snapshot_immediately() {
    let j = FakeJournal::simple(42);
    let b = broadcaster(&j);
    let mut s = snapshot_stream(&b, HB);
    let chunks = take(&mut s, 2).await;
    b.stop();
    assert_eq!(chunks[0], "retry: 2000\n\n");
    let snap = parse(&chunks[1]);
    assert_eq!((snap["id"].as_str(), snap["event"].as_str()), ("42", "snapshot"));
    assert_eq!(data(&chunks[1])["revision"], 42);
}

#[tokio::test]
async fn test_snapshot_mode_sends_new_snapshot_on_change_and_heartbeat_when_idle() {
    let j = FakeJournal::simple(1);
    let b = broadcaster(&j);
    let mut s = snapshot_stream(&b, HB);
    take(&mut s, 2).await;
    let idle = take(&mut s, 1).await;
    j.set(2);
    let changed = take(&mut s, 1).await;
    b.stop();
    assert_eq!(idle, [HEARTBEAT]);
    assert_eq!(parse(&changed[0])["id"], "2");
}

#[tokio::test]
async fn test_snapshot_mode_waits_for_first_import() {
    let j = FakeJournal::new(0, None, false);
    let b = broadcaster(&j);
    let mut s = snapshot_stream(&b, HB);
    let first = take(&mut s, 2).await; // retry + heartbeat: снимка ещё нет
    j.set(1);
    *j.min_id.lock().unwrap() = Some(1);
    *j.ready.lock().unwrap() = true;
    let snap = take(&mut s, 1).await;
    b.stop();
    assert_eq!(first[1], HEARTBEAT);
    let p = parse(&snap[0]);
    assert_eq!((p["event"].as_str(), p["id"].as_str()), ("snapshot", "1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_snapshot_built_once_per_revision_for_many_clients() {
    let j = FakeJournal::simple(7);
    let b = broadcaster(&j);
    let mut streams: Vec<SseStream> = (0..20).map(|_| snapshot_stream(&b, HB)).collect();
    let mut results = Vec::new();
    for s in &mut streams {
        results.push(take(s, 2).await);
    }
    b.stop();
    assert_eq!(j.builds.load(Ordering::SeqCst), 1);
    assert!(results.iter().all(|r| parse(&r[1])["id"] == "7"));
}

#[tokio::test]
async fn snapshot_stream_uses_reinitialized_database() {
    let j = FakeJournal::simple(9);
    let b = broadcaster(&j);
    let mut existing = snapshot_stream(&b, HB);
    take(&mut existing, 2).await;
    j.set(1);
    b.refresh().await.unwrap();
    let update = take(&mut existing, 1).await;
    let reconnect = take(&mut snapshot_stream(&b, HB), 2).await;
    b.stop();
    assert_eq!(data(&update[0])["revision"], 1);
    assert_eq!(data(&reconnect[1])["revision"], 1);
}

#[tokio::test]
async fn test_reconnect_in_snapshot_mode_gets_fresh_snapshot() {
    let j = FakeJournal::simple(3);
    let b = broadcaster(&j);
    take(&mut snapshot_stream(&b, HB), 2).await; // первое подключение
    j.set(5); // клиент пропустил ревизии 4 и 5; фоновый опрос их ещё не увидел
    b.set_poll_interval(Duration::from_secs(3600)); // опрос «спит» — свежесть даёт переподключка
    tokio::time::sleep(Duration::from_millis(30)).await;
    let again = take(&mut snapshot_stream(&b, HB), 2).await;
    b.stop();
    assert_eq!(parse(&again[1])["id"], "5");
}

// ================================================================ режим notify (ARCHITECTURE.md)

#[tokio::test]
async fn test_notify_first_connection_then_event() {
    let j = FakeJournal::simple(184);
    let b = broadcaster(&j);
    let mut s = notify_stream(&b, Some("184".into()), HB);
    let first = take(&mut s, 2).await;
    j.set(185);
    let event = take(&mut s, 1).await.remove(0);
    b.stop();
    assert_eq!(first, ["retry: 2000\n\n", HEARTBEAT]);
    let p = parse(&event);
    assert_eq!(
        (p["event"].as_str(), p["id"].as_str()),
        ("dashboard.changed", "185")
    );
    assert_eq!(data(&event), json!({"schemaVersion": 1, "reason": "import"}));
    // Тело как у json.dumps(separators=(",", ":")).
    assert_eq!(p["data"], r#"{"schemaVersion":1,"reason":"import"}"#);
}

#[tokio::test]
async fn test_notify_reconnect_sees_change_before_background_poll() {
    let j = FakeJournal::simple(3);
    let b = Broadcaster::new(Arc::clone(&j) as Arc<dyn Journal>, Duration::from_secs(3600));
    b.head().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await; // фоновый опрос отработал и «уснул» на час
    j.set(4); // изменение после последнего опроса
    let mut s = notify_stream(&b, Some("3".into()), HB);
    let replay = take(&mut s, 2).await.remove(1);
    b.stop();
    assert_eq!(parse(&replay)["id"], "4");
}

#[tokio::test]
async fn test_notify_reconnect_with_last_event_id_replays_once() {
    let j = FakeJournal::simple(10);
    let b = broadcaster(&j);
    let mut s = notify_stream(&b, Some("6".into()), HB); // пропущены 7–10
    let chunks = take(&mut s, 3).await;
    b.stop();
    assert_eq!(parse(&chunks[1])["id"], "10");
    assert_eq!(chunks[2], HEARTBEAT); // одно событие, не четыре
}

#[tokio::test]
async fn test_notify_stale_cursor_resets_and_closes() {
    for (cursor, (max, min), reason) in [
        ("2", (9, 5), "cursor_expired"),
        ("50", (9, 1), "cursor_ahead"),
        ("x", (9, 1), "invalid_cursor"),
    ] {
        let j = FakeJournal::new(max, Some(min), true);
        let b = broadcaster(&j);
        let chunks = rest(&mut notify_stream(&b, Some(cursor.into()), HB)).await; // поток завершается
        b.stop();
        assert_eq!(chunks.len(), 2);
        let p = parse(&chunks[1]);
        assert_eq!(p["event"], "stream.reset");
        assert_eq!(
            data(&chunks[1]),
            json!({"schemaVersion": 1, "reason": reason, "eventCursor": p["id"]})
        );
    }
}

#[tokio::test]
async fn test_notify_journal_rollback_resets() {
    let j = FakeJournal::simple(9);
    let b = broadcaster(&j);
    let mut s = notify_stream(&b, Some("9".into()), HB);
    take(&mut s, 1).await;
    j.set(1); // БД пересоздали
    let tail = rest(&mut s).await;
    b.stop();
    assert_eq!(parse(tail.last().unwrap())["event"], "stream.reset");
}

#[tokio::test]
async fn test_broadcaster_survives_db_errors() {
    let j = FakeJournal::simple(1);
    let b = broadcaster(&j);
    b.head().await.unwrap();
    j.fail_next.store(3, Ordering::SeqCst);
    j.set(2);
    let head = b.wait_for_change(1, Duration::from_secs(1)).await.unwrap();
    b.stop();
    assert_eq!(head.unwrap().max_id, 2);
}

#[tokio::test]
async fn test_wait_for_change_times_out() {
    let b = broadcaster(&FakeJournal::simple(1));
    let result = b.wait_for_change(1, HB).await.unwrap();
    b.stop();
    assert!(result.is_none());
}

// ================================================================ только Rust-версия

/// Ответ опроса, начатого раньше, пришёл позже — курсор не откатывается назад (иначе поток
/// notify принял бы это за пересоздание БД и отправил `stream.reset`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_poll_result_does_not_move_cursor_back() {
    let j = FakeJournal::simple(5);
    let b = Broadcaster::new(Arc::clone(&j) as Arc<dyn Journal>, Duration::from_secs(3600));
    b.head().await.unwrap(); // вызов 0; фоновый опрос — вызов 1
    tokio::time::sleep(Duration::from_millis(30)).await;
    let n = j.fetches.load(Ordering::SeqCst);
    j.delays.lock().unwrap().insert(n, Duration::from_millis(200)); // этот опрос «застрянет»
    let slow = {
        let b = Arc::clone(&b);
        tokio::spawn(async move { b.poll_once().await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    j.set(6);
    assert_eq!(b.poll_once().await.unwrap().max_id, 6);
    assert_eq!(slow.await.unwrap().max_id, 5); // вернул прочитанное, но не применил
    assert_eq!(b.head().await.unwrap().max_id, 6);
    b.stop();
}

#[tokio::test]
async fn shutdown_closes_open_streams() {
    let j = FakeJournal::simple(1);
    let b = broadcaster(&j);
    let mut snap = snapshot_stream(&b, Duration::from_secs(3600));
    let mut notify = notify_stream(&b, None, Duration::from_secs(3600));
    take(&mut snap, 2).await;
    take(&mut notify, 1).await;
    b.shutdown();
    assert!(rest(&mut snap).await.is_empty());
    assert!(rest(&mut notify).await.is_empty());
}

#[tokio::test]
async fn db_error_on_connect_aborts_stream() {
    let j = FakeJournal::simple(1);
    j.fail_next.store(1, Ordering::SeqCst);
    let b = Broadcaster::new(Arc::clone(&j) as Arc<dyn Journal>, Duration::from_secs(3600));
    let mut s = snapshot_stream(&b, HB);
    assert_eq!(s.next_chunk().await.unwrap().unwrap(), "retry: 2000\n\n");
    assert!(s.next_chunk().await.unwrap().is_err()); // соединение обрывается
    assert!(s.next_chunk().await.is_none());
    b.stop();
}

#[tokio::test]
async fn client_disconnect_ends_stream_task_without_waiting_for_heartbeat() {
    let j = FakeJournal::simple(1);
    let b = broadcaster(&j);
    let mut s = snapshot_stream(&b, Duration::from_secs(3600));
    take(&mut s, 2).await;
    drop(s);
    // Задача потока держит Arc broadcaster; после отключения она завершается сразу.
    tokio::time::timeout(Duration::from_secs(1), async {
        while Arc::strong_count(&b) > 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("задача потока не завершилась");
}

#[tokio::test]
async fn background_task_ends_when_broadcaster_dropped() {
    let j = FakeJournal::simple(1);
    let b = broadcaster(&j);
    b.head().await.unwrap();
    drop(b);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let n = j.fetches.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        j.fetches.load(Ordering::SeqCst),
        n,
        "опрос продолжается после drop"
    );
}
