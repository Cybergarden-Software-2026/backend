//! Server-Sent Events — порт `app/realtime/{sse, db_broadcaster}.py`.
//!
//! Два режима `GET /api/v1/events/stream`:
//!
//! * `mode=snapshot` (по умолчанию, API.md §4): при подключении — `retry: 2000` и полный снимок
//!   (`event: snapshot`), далее новый снимок при каждой новой ревизии; `id:` = revision;
//! * `mode=notify` (ARCHITECTURE.md): только сигнал `dashboard.changed` (`id:` = курсор журнала);
//!   курсор — `Last-Event-ID`, при первом подключении — `since`. Курсор вне журнала —
//!   `stream.reset`, и поток закрывается.
//!
//! Источник изменений — [`Broadcaster`]: один цикл опроса `dashboard_events` на процесс. Снимок
//! строится один раз на ревизию. Доступ к БД — через трейт [`Journal`] (в тестах — подделка).
//!
//! Отличия от asyncio-версии, связанные с многопоточностью: результаты опроса применяются в
//! порядке начала запросов (опоздавший ответ более раннего опроса не откатывает курсор назад —
//! в однопоточном Python такое было возможно, но маловероятно); поток — задача tokio, которая
//! пишет в канал ёмкостью 1 (как генератор Python, не уходит вперёд читателя больше чем на
//! одно событие) и завершается сразу после отключения клиента, не дожидаясь heartbeat.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use dashboard_domain::pycompat;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::AppState;

pub const RETRY_MS: u32 = 2000;
pub const HEARTBEAT: &str = ": heartbeat\n\n";

// ====================================================================== формат SSE

/// Одно SSE-событие. Многострочные данные разбиваются на несколько строк `data:`.
pub fn format_event(event: &str, data: &str, event_id: Option<i64>, retry: Option<u32>) -> String {
    let mut out = String::with_capacity(data.len() + event.len() + 32);
    if let Some(retry) = retry {
        out.push_str(&format!("retry: {retry}\n"));
    }
    if let Some(id) = event_id {
        out.push_str(&format!("id: {id}\n"));
    }
    out.push_str("event: ");
    out.push_str(event);
    out.push('\n');
    // `data.split("\n")` Python: пустая строка тоже даёт одну строку `data: `.
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

pub fn retry_line() -> String {
    format!("retry: {RETRY_MS}\n\n")
}

pub fn dashboard_changed(event_id: i64, reason: Option<&str>) -> String {
    let data = json!({"schemaVersion": 1, "reason": reason});
    format_event("dashboard.changed", &pycompat::dumps(&data), Some(event_id), None)
}

pub fn stream_reset(event_id: i64, reason: &str) -> String {
    let data = json!({"schemaVersion": 1, "reason": reason, "eventCursor": event_id.to_string()});
    format_event("stream.reset", &pycompat::dumps(&data), Some(event_id), None)
}

// ====================================================================== журнал

/// Границы журнала `dashboard_events`: курсор (max id, 0 — пусто), самый старый id и причина
/// последнего события.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalHead {
    pub max_id: i64,
    pub min_id: Option<i64>,
    pub reason: Option<String>,
}

impl JournalHead {
    pub fn new(max_id: i64, min_id: Option<i64>) -> Self {
        Self {
            max_id,
            min_id,
            reason: None,
        }
    }
}

/// Готовое тело события `snapshot` для одной ревизии.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltSnapshot {
    pub revision: i64,
    pub json: Arc<str>,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Доступ broadcaster к данным (`fetch_head` / `build_snapshot` Python-версии).
pub trait Journal: Send + Sync + 'static {
    fn fetch_head(&self) -> BoxFuture<'_, anyhow::Result<JournalHead>>;
    /// `None` — первого снимка ещё нет.
    fn build_snapshot(&self) -> BoxFuture<'_, anyhow::Result<Option<BuiltSnapshot>>>;
}

// ====================================================================== broadcaster

/// Опрашивает журнал и будит всех подписчиков при новой ревизии (`RevisionBroadcaster`).
pub struct Broadcaster {
    journal: Arc<dyn Journal>,
    poll_interval: Mutex<Duration>,
    /// Текущая голова журнала; `None` — первого успешного опроса ещё не было. Значение меняется
    /// только вместе с `max_id` (причина и `min_id` — от того же опроса, как в Python).
    head: watch::Sender<Option<JournalHead>>,
    /// Номер последнего начатого опроса и последнего применённого.
    started: AtomicU64,
    applied: Mutex<u64>,
    snapshot: Mutex<Option<BuiltSnapshot>>,
    build_lock: tokio::sync::Mutex<()>,
    task: Mutex<Option<JoinHandle<()>>>,
    /// `true` — сервер останавливается: потоки закрываются.
    closing: watch::Sender<bool>,
    me: Weak<Broadcaster>,
}

impl Broadcaster {
    pub fn new(journal: Arc<dyn Journal>, poll_interval: Duration) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            journal,
            poll_interval: Mutex::new(poll_interval),
            head: watch::Sender::new(None),
            started: AtomicU64::new(0),
            applied: Mutex::new(0),
            snapshot: Mutex::new(None),
            build_lock: tokio::sync::Mutex::new(()),
            task: Mutex::new(None),
            closing: watch::Sender::new(false),
            me: me.clone(),
        })
    }

    pub fn set_poll_interval(&self, interval: Duration) {
        *self.poll_interval.lock().unwrap() = interval;
    }

    // ------------------------------------------------------------ жизненный цикл

    /// Запускает фоновый опрос, если он ещё не идёт. Задача держит слабую ссылку и сама
    /// завершается, когда broadcaster больше никому не нужен.
    pub fn start(&self) {
        let mut task = self.task.lock().unwrap();
        if task.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let me = self.me.clone();
        *task = Some(tokio::spawn(async move {
            loop {
                let interval = {
                    let Some(b) = me.upgrade() else { return };
                    if let Err(e) = b.poll_once().await {
                        tracing::error!("SSE: не удалось прочитать журнал; повтор: {e:#}");
                    }
                    *b.poll_interval.lock().unwrap()
                };
                tokio::time::sleep(interval).await;
            }
        }));
    }

    /// Останавливает фоновый опрос (открытые потоки продолжают работать; `start` запустит снова).
    pub fn stop(&self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }

    /// Остановка сервера: опрос прекращается, все открытые потоки закрываются (иначе плавная
    /// остановка axum ждала бы бесконечные ответы).
    pub fn shutdown(&self) {
        self.stop();
        self.closing.send_replace(true);
    }

    pub fn is_closing(&self) -> bool {
        *self.closing.borrow()
    }

    // ------------------------------------------------------------ опрос

    pub async fn poll_once(&self) -> anyhow::Result<JournalHead> {
        let ticket = self.started.fetch_add(1, Ordering::SeqCst) + 1;
        let head = self.journal.fetch_head().await?;
        self.head.send_if_modified(|current| {
            let mut applied = self.applied.lock().unwrap();
            if ticket < *applied {
                return false; // ответ более позднего опроса уже применён
            }
            *applied = ticket;
            match current {
                Some(c) if c.max_id == head.max_id => false,
                _ => {
                    *current = Some(head.clone());
                    true
                }
            }
        });
        Ok(head)
    }

    /// Немедленно перечитать журнал. Вызывается при каждом новом подключении: переподключившийся
    /// клиент должен сразу получить актуальную ревизию, не дожидаясь следующего цикла опроса.
    pub async fn refresh(&self) -> anyhow::Result<JournalHead> {
        self.start();
        self.poll_once().await
    }

    /// Текущие границы журнала (ждёт первого опроса).
    pub async fn head(&self) -> anyhow::Result<JournalHead> {
        self.start();
        if let Some(h) = self.head.borrow().clone() {
            return Ok(h);
        }
        self.poll_once().await?;
        Ok(self
            .head
            .borrow()
            .clone()
            .expect("после успешного опроса голова журнала известна"))
    }

    /// Ждёт, пока курсор журнала станет отличным от `known_max_id`. `None` — таймаут (пора
    /// heartbeat).
    pub async fn wait_for_change(
        &self,
        known_max_id: i64,
        wait: Duration,
    ) -> anyhow::Result<Option<JournalHead>> {
        let deadline = Instant::now() + wait;
        let mut rx = self.head.subscribe();
        loop {
            let seen = rx.borrow_and_update().clone();
            let head = match seen {
                Some(h) => {
                    self.start();
                    h
                }
                None => self.head().await?,
            };
            if head.max_id != known_max_id {
                return Ok(Some(head));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            if tokio::time::timeout(deadline - now, rx.changed()).await.is_err() {
                return Ok(None);
            }
        }
    }

    /// Снимок для текущей ревизии; конкурирующие подписчики ждут одной сборки.
    pub async fn snapshot(&self) -> anyhow::Result<Option<BuiltSnapshot>> {
        let head = self.head().await?;
        let fresh =
            |cached: &Option<BuiltSnapshot>| cached.as_ref().filter(|c| c.revision >= head.max_id).cloned();
        if let Some(c) = fresh(&self.snapshot.lock().unwrap()) {
            return Ok(Some(c));
        }
        let _guard = self.build_lock.lock().await;
        if let Some(c) = fresh(&self.snapshot.lock().unwrap()) {
            return Ok(Some(c));
        }
        let built = self.journal.build_snapshot().await?;
        if let Some(b) = &built {
            *self.snapshot.lock().unwrap() = Some(b.clone());
        }
        Ok(built)
    }
}

// ====================================================================== потоки

/// Поток SSE: куски текста по одному на событие. Ошибка БД обрывает соединение (как исключение
/// в генераторе `StreamingResponse`), остановка сервера и конец потока `notify` — завершают его.
pub struct SseStream {
    rx: mpsc::Receiver<Result<String, std::io::Error>>,
}

impl SseStream {
    /// Следующий кусок; `None` — поток завершён.
    pub async fn next_chunk(&mut self) -> Option<Result<String, std::io::Error>> {
        self.rx.recv().await
    }

    pub fn into_response(self) -> Response {
        let body = Body::from_stream(ChunkStream(self.rx));
        let mut res = body.into_response();
        let h = res.headers_mut();
        // StreamingResponse(media_type="text/event-stream") Starlette добавляет charset к text/*.
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, no-transform"),
        );
        h.insert("x-accel-buffering", HeaderValue::from_static("no")); // nginx: не буферизовать
        h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        res
    }
}

struct ChunkStream(mpsc::Receiver<Result<String, std::io::Error>>);

impl futures_core::Stream for ChunkStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx).map(|item| item.map(|r| r.map(Bytes::from)))
    }
}

/// Отправка кусков из задачи потока. `Err(Closed)` — клиент отключился.
pub struct Sink {
    tx: mpsc::Sender<Result<String, std::io::Error>>,
}

pub struct Closed;

impl Sink {
    async fn send(&self, chunk: String) -> Result<(), Closed> {
        self.tx.send(Ok(chunk)).await.map_err(|_| Closed)
    }
}

enum Stop {
    Closed,
    Failed(anyhow::Error),
}

impl From<Closed> for Stop {
    fn from(_: Closed) -> Self {
        Stop::Closed
    }
}

impl From<anyhow::Error> for Stop {
    fn from(e: anyhow::Error) -> Self {
        Stop::Failed(e)
    }
}

fn spawn_stream<F, Fut>(broadcaster: &Arc<Broadcaster>, body: F) -> SseStream
where
    F: FnOnce(Sink) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), Stop>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(1);
    let mut closing = broadcaster.closing.subscribe();
    let watcher = tx.clone();
    tokio::spawn(async move {
        let run = body(Sink { tx: tx.clone() });
        let result = tokio::select! {
            r = run => r,
            () = watcher.closed() => Err(Stop::Closed),
            _ = closing.wait_for(|c| *c) => Err(Stop::Closed),
        };
        if let Err(Stop::Failed(e)) = result {
            tracing::error!("SSE: поток прерван: {e:#}");
            let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
        }
    });
    SseStream { rx }
}

/// Режим API.md: снимок сразу и при каждой новой ревизии.
pub fn snapshot_stream(broadcaster: &Arc<Broadcaster>, heartbeat: Duration) -> SseStream {
    let b = Arc::clone(broadcaster);
    spawn_stream(broadcaster, move |sink| async move {
        sink.send(retry_line()).await?;
        let mut sent: Option<i64> = None;
        let mut known = b.refresh().await?.max_id;
        loop {
            if let Some(snap) = b.snapshot().await? {
                if sent != Some(snap.revision) {
                    sink.send(format_event("snapshot", &snap.json, Some(snap.revision), None))
                        .await?;
                    sent = Some(snap.revision);
                    known = known.max(snap.revision);
                }
            }
            match b.wait_for_change(known, heartbeat).await? {
                None => sink.send(HEARTBEAT.to_owned()).await?,
                Some(head) => known = head.max_id,
            }
        }
    })
}

/// Что отправить при (пере)подключении в режиме notify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyStart {
    /// Подписка «с текущего момента».
    Live,
    /// Журнал ушёл вперёд — одно `dashboard.changed` с последним id.
    Replay,
    /// `stream.reset` с причиной, поток закрывается.
    Reset(&'static str),
}

/// `plan_notify_start`.
///
/// * нет курсора — `Live`;
/// * курсор не число — `invalid_cursor`;
/// * курсор больше текущего (БД пересоздана) — `cursor_ahead`;
/// * курсор старше хранимого журнала — `cursor_expired`;
/// * журнал ушёл вперёд — `Replay`; иначе — `Live`.
///
/// Цифры — десятичные Unicode (`"١٢"` = 12), пробелы по краям допускаются, как `isdigit()` +
/// `int()`. Отличие, оставленное намеренно: `"²"` и подобные `isdigit()` пропускает, а `int()`
/// на них падает — Python обрывает поток после `retry:`; здесь — `stream.reset` (`invalid_cursor`).
pub fn plan_notify_start(cursor: Option<&str>, head: &JournalHead) -> NotifyStart {
    let Some(cursor) = cursor else {
        return NotifyStart::Live;
    };
    let stripped = pycompat::strip(cursor);
    if stripped.is_empty() {
        return NotifyStart::Live;
    }
    if !stripped.chars().all(|c| pycompat::decimal_digit(c).is_some()) {
        return NotifyStart::Reset("invalid_cursor");
    }
    let Some(value) = pycompat::py_int(cursor) else {
        return NotifyStart::Reset("invalid_cursor");
    };
    let max_id = i128::from(head.max_id);
    if value > max_id {
        return NotifyStart::Reset("cursor_ahead");
    }
    if head.min_id.is_some_and(|min| value < i128::from(min) - 1) {
        return NotifyStart::Reset("cursor_expired");
    }
    if value < max_id {
        NotifyStart::Replay
    } else {
        NotifyStart::Live
    }
}

/// Режим ARCHITECTURE.md: `dashboard.changed` / `stream.reset` по курсору журнала.
pub fn notify_stream(
    broadcaster: &Arc<Broadcaster>,
    cursor: Option<String>,
    heartbeat: Duration,
) -> SseStream {
    let b = Arc::clone(broadcaster);
    spawn_stream(broadcaster, move |sink| async move {
        sink.send(retry_line()).await?;
        let head = b.refresh().await?;
        match plan_notify_start(cursor.as_deref(), &head) {
            NotifyStart::Reset(reason) => {
                sink.send(stream_reset(head.max_id, reason)).await?;
                return Ok(());
            }
            NotifyStart::Replay => {
                sink.send(dashboard_changed(head.max_id, head.reason.as_deref()))
                    .await?;
            }
            NotifyStart::Live => {}
        }
        let mut known = head.max_id;
        loop {
            let Some(new) = b.wait_for_change(known, heartbeat).await? else {
                sink.send(HEARTBEAT.to_owned()).await?;
                continue;
            };
            if new.max_id < known {
                // журнал «откатился» (БД пересоздана) — клиенту нужен полный снимок
                sink.send(stream_reset(new.max_id, "cursor_ahead")).await?;
                return Ok(());
            }
            sink.send(dashboard_changed(new.max_id, new.reason.as_deref()))
                .await?;
            known = new.max_id;
        }
    })
}

// ====================================================================== связка с PostgreSQL

/// `make_db_broadcaster`: журнал из `dashboard_events`, снимок — тот же, что `/snapshot`
/// ([`crate::dashboard::build_snapshot`]), записанный как `model_dump_json()` pydantic.
pub struct DbJournal {
    state: AppState,
}

impl DbJournal {
    /// Своя копия состояния без ссылки на broadcaster (иначе получился бы цикл Arc).
    pub fn new(state: &AppState) -> Self {
        Self {
            state: state.detached(),
        }
    }
}

impl Journal for DbJournal {
    fn fetch_head(&self) -> BoxFuture<'_, anyhow::Result<JournalHead>> {
        Box::pin(async move {
            let (max_id, min_id, reason): (Option<i64>, Option<i64>, Option<String>) = sqlx::query_as(
                "SELECT max(id), min(id), \
                 (SELECT reason FROM dashboard_events ORDER BY id DESC LIMIT 1) FROM dashboard_events",
            )
            .fetch_one(&self.state.pool)
            .await?;
            Ok(JournalHead {
                max_id: max_id.unwrap_or(0),
                min_id,
                reason: if max_id.is_some() { reason } else { None },
            })
        })
    }

    fn build_snapshot(&self) -> BoxFuture<'_, anyhow::Result<Option<BuiltSnapshot>>> {
        Box::pin(async move {
            let value = match crate::dashboard::build_snapshot(&self.state).await {
                Ok(v) => v,
                Err(e) if e.code == "SOURCE_NOT_READY" => return Ok(None),
                Err(e) => return Err(anyhow::anyhow!("{}: {}", e.code, e.message)),
            };
            let revision = value["revision"].as_i64().unwrap_or_default();
            Ok(Some(snapshot_from_value(revision, &value)))
        })
    }
}

pub fn snapshot_from_value(revision: i64, value: &Value) -> BuiltSnapshot {
    BuiltSnapshot {
        revision,
        json: pycompat::dumps_pydantic(value).into(),
    }
}
