//! Окружение интеграционных тестов импортёра — порт `env()` из
//! `backend/tests/integration/test_stage5_import.py`.
//!
//! Нужен `TEST_DATABASE_URL` (как у Python-тестов): каждый тест создаёт свою временную базу
//! `rust_import_*`, применяет общую миграцию и удаляет базу в конце; сама база из
//! `TEST_DATABASE_URL` не меняется. Без переменной тесты пропускаются.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use dashboard_config::Settings;
use dashboard_importer::{ImportOutcome, ImportService, ImporterWorker};
use serde_json::{Map, Value};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool, Row};

pub const DEMO_ID: &str = "INC000021938167";
pub const COORD: &str = "КЦ ИТ Координаторы Массовых Инцидентов";

/// Исходная выгрузка `testdata/character_problems.json` (не `data/…`, её меняет демо).
pub const SAMPLE_JSON: &str = include_str!("../../../../testdata/character_problems.json");

pub type Record = Map<String, Value>;

pub fn records() -> Vec<Record> {
    serde_json::from_str(SAMPLE_JSON).expect("character_problems.json")
}

pub fn demo(recs: &mut [Record]) -> &mut Record {
    recs.iter_mut().find(|r| r["id"] == DEMO_ID).expect("demo record")
}

/// `record.update(k=v, …)`.
pub fn update(record: &mut Record, pairs: &[(&str, Value)]) {
    for (k, v) in pairs {
        record.insert((*k).to_owned(), v.clone());
    }
}

pub fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    u64::from(std::process::id()) * 10_000 + N.fetch_add(1, Ordering::Relaxed)
}

fn replace_db_name(url: &str, name: &str) -> String {
    let (base, query) = url.split_once('?').map_or((url, None), |(b, q)| (b, Some(q)));
    let base = &base[..base.rfind('/').unwrap()];
    match query {
        Some(q) => format!("{base}/{name}?{q}"),
        None => format!("{base}/{name}"),
    }
}

/// Временная база с общей схемой. `url` — для внешних процессов (например, Python-эталона).
pub struct TempDb {
    admin_url: String,
    pub name: String,
    pub url: String,
    pub pool: PgPool,
}

impl TempDb {
    pub async fn create(prefix: &str) -> Option<Self> {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skip: TEST_DATABASE_URL is not set");
            return None;
        };
        let admin_url = dashboard_config::normalize_database_url(&url);
        let name = format!("{prefix}_{}", unique());
        let mut admin = PgConnection::connect(&admin_url).await.unwrap();
        sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name}")))
            .execute(&mut admin)
            .await
            .unwrap();
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&mut admin)
            .await
            .unwrap();
        let db_url = replace_db_name(&admin_url, &name);
        let settings = Settings::from_vars([("DATABASE_URL", db_url.as_str())]).unwrap();
        let pool = dashboard_store::connect(&settings).await.unwrap();
        dashboard_store::migrate::run(&pool).await.unwrap();
        Some(Self {
            admin_url,
            name,
            url: db_url,
            pool,
        })
    }

    pub async fn drop(self) {
        self.pool.close().await;
        let mut admin = PgConnection::connect(&self.admin_url).await.unwrap();
        sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        )))
        .execute(&mut admin)
        .await
        .unwrap();
    }
}

/// Управляемые часы (`Clock` Python-тестов).
#[derive(Clone)]
pub struct TestClock(Arc<Mutex<DateTime<Utc>>>);

impl TestClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self(Arc::new(Mutex::new(start)))
    }
    pub fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
    pub fn advance(&self, seconds: f64) {
        let micros = (seconds * 1e6).round() as i64;
        *self.0.lock().unwrap() += TimeDelta::microseconds(micros);
    }
    pub fn as_clock(&self) -> dashboard_importer::service::Clock {
        let inner = Arc::clone(&self.0);
        Arc::new(move || *inner.lock().unwrap())
    }
}

pub struct Env {
    pub db: TempDb,
    pub dir: PathBuf,
    pub source: PathBuf,
    pub clock: TestClock,
    pub worker: ImporterWorker,
    pub settings: Arc<Settings>,
}

impl Env {
    pub async fn new() -> Option<Self> {
        Self::with_vars(&[]).await
    }

    pub async fn with_vars(extra: &[(&str, &str)]) -> Option<Self> {
        let db = TempDb::create("rust_import").await?;
        let dir = std::env::temp_dir().join(format!("rust_import_{}", unique()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("character_problems.json");
        let source_str = source.to_string_lossy().into_owned();
        let mut vars = vec![
            ("DATABASE_URL", db.url.as_str()),
            ("JSON_SOURCE_PATH", source_str.as_str()),
            ("IMPORT_ERROR_THRESHOLD", "2"),
        ];
        vars.extend_from_slice(extra);
        let settings = Arc::new(Settings::from_vars(vars).unwrap());
        let clock = TestClock::new(t0());
        let service = ImportService::with_clock(db.pool.clone(), Arc::clone(&settings), clock.as_clock());
        Some(Self {
            worker: ImporterWorker::new(service),
            db,
            dir,
            source,
            clock,
            settings,
        })
    }

    pub async fn finish(self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        self.db.drop().await;
    }

    pub fn pool(&self) -> &PgPool {
        &self.db.pool
    }

    pub fn service(&self) -> &ImportService {
        self.worker.service()
    }

    /// Атомарная публикация, как рекомендует ARCHITECTURE.md.
    pub fn write_json(&self, records: &[Record]) {
        write_json(&self.source, records);
    }

    /// Запись «на месте» (недописанный файл).
    pub fn write_text(&self, text: &str) {
        std::fs::write(&self.source, text).unwrap();
    }

    pub async fn tick(&mut self) -> ImportOutcome {
        self.tick_after(1.0).await
    }

    pub async fn tick_after(&mut self, seconds: f64) -> ImportOutcome {
        self.clock.advance(seconds);
        self.worker.tick().await.expect("tick")
    }

    pub async fn row(&self, sql: &str) -> PgRow {
        sqlx::query(AssertSqlSafe(sql.to_owned()))
            .fetch_one(self.pool())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    pub async fn i64(&self, sql: &str) -> i64 {
        self.row(sql).await.get::<i64, _>(0)
    }

    pub async fn opt_i64(&self, sql: &str) -> Option<i64> {
        self.row(sql).await.get::<Option<i64>, _>(0)
    }

    pub async fn string(&self, sql: &str) -> Option<String> {
        self.row(sql).await.get::<Option<String>, _>(0)
    }

    pub async fn boolean(&self, sql: &str) -> bool {
        self.row(sql).await.get::<bool, _>(0)
    }

    pub async fn json(&self, sql: &str) -> Value {
        self.row(sql).await.get::<Value, _>(0)
    }

    pub async fn strings(&self, sql: &str) -> Vec<String> {
        self.row(sql).await.get::<Vec<String>, _>(0)
    }

    pub async fn time(&self, sql: &str) -> Option<DateTime<Utc>> {
        self.row(sql).await.get::<Option<DateTime<Utc>>, _>(0)
    }

    pub async fn counts(&self) -> [i64; 5] {
        let mut out = [0; 5];
        for (i, t) in [
            "incidents",
            "incident_changes",
            "change_events",
            "dashboard_events",
            "notifications",
        ]
        .iter()
        .enumerate()
        {
            out[i] = self.i64(&format!("SELECT count(*) FROM {t}")).await;
        }
        out
    }
}

pub fn write_json(path: &Path, records: &[Record]) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(records).unwrap()).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

pub fn s(v: &str) -> Value {
    Value::String(v.to_owned())
}
