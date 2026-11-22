//! Настройки процесса — порт `backend/app/config.py`.
//!
//! Правила те же, что у pydantic-settings в Python-версии, чтобы `.env` был общим:
//! * имена переменных не зависят от регистра, неизвестные переменные игнорируются;
//! * переменные окружения важнее файла `.env` (файл ищется в текущем каталоге);
//! * `DATABASE_URL` обязателен; `postgresql+asyncpg://…` принимается и приводится к `postgres://…`;
//! * ограничения (`> 0`, `>= 1`, …) и валидаторы (`ITSM_*`, часовой пояс) совпадают;
//! * ошибки собираются все сразу, а не по одной.
//!
//! Интеграция с локальной LLM (`LLM_*`) по умолчанию выключена (`LLM_ENABLED=false`): без неё процессы
//! не ставят задачи в очередь и не обращаются к модели. Адрес модели (`LLM_BASE_URL`) нужен только
//! `ai-worker`, остальные процессы используют лишь идентификатор модели для очереди.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use chrono_tz::Tz;

pub mod dotenv;

/// Режим окружения (`ENVIRONMENT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Dev,
    Test,
    Prod,
}

impl Environment {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Test => "test",
            Self::Prod => "prod",
        }
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    // --- Общие ---
    pub app_name: String,
    pub environment: Environment,
    pub log_level: String,
    pub cors_origins: Vec<String>,
    pub default_user_id: String,
    pub coordinator_access_key: Option<CoordinatorAccessKey>,
    pub coordinator_accounts_enabled: bool,
    pub mobile_push_enabled: bool,
    pub firebase_project_id: Option<String>,

    // --- HTTP (только Rust-версия: у Python адрес задаётся аргументами uvicorn) ---
    pub api_host: String,
    pub api_port: u16,

    // --- PostgreSQL ---
    /// URL в форме, понятной sqlx (`postgres://…`), без `+asyncpg`.
    pub database_url: String,
    pub db_pool_size: u32,
    pub db_max_overflow: u32,
    pub db_echo: bool,

    // --- Источник JSON ---
    pub json_source_path: PathBuf,
    pub import_poll_interval: Duration,
    pub import_error_threshold: u32,
    pub import_stable_read_delay: Duration,
    pub source_naive_timezone: Tz,
    pub importer_embedded: bool,

    // --- Демо-режим (тестовые маршруты фронтенда) ---
    /// `POST /api/v1/demo` и `PUT /api/v1/incidents` переписывают файл источника. По умолчанию выключены.
    pub demo_api_enabled: bool,
    /// Эталонная выгрузка для сценария `reset`; пустое значение — сценарий недоступен.
    pub demo_seed_path: Option<PathBuf>,

    // --- Бизнес-правила ---
    pub coordinator_group_name: String,
    pub critical_resolved_window_hours: u32,
    pub deadline_warning_minutes: u32,

    // --- ITSM ---
    /// Без пробелов по краям и без завершающего `/`; пустое значение — `None`.
    pub itsm_base_url: Option<String>,
    /// Содержит `{sysId}` и/или `{id}`; пустое значение — `None`.
    pub itsm_url_template: Option<String>,

    // --- SSE / журнал событий ---
    pub sse_poll_interval: Duration,
    pub sse_heartbeat: Duration,
    pub events_retention: u32,

    // --- Локальная LLM (OpenAI-совместимый `llama-server`) ---
    /// Постановка задач (импортёр), выдача результатов (API) и вызовы модели (`ai-worker`).
    pub llm_enabled: bool,
    /// Базовый адрес вместе с `/v1`, без завершающего `/`. Обязателен только для `ai-worker`.
    pub llm_base_url: Option<String>,
    /// Имя модели в запросе (`model`), например алиас `qwen-local`.
    pub llm_model: String,
    /// Метка весов за тем же алиасом (например, начало SHA-256 GGUF). Её смена ставит задачи заново.
    pub llm_model_revision: Option<String>,
    pub llm_timeout: Duration,
    pub llm_max_tokens: u32,
    pub llm_max_attempts: u32,
    pub llm_poll_interval: Duration,
    /// Аренда задачи: после истечения её может забрать другой экземпляр worker.
    pub llm_lease: Duration,
    /// Ограничение длины каждого текстового поля инцидента в запросе (символы).
    pub llm_input_max_chars: u32,

    /// Строки `.env`, пропущенные при разборе (для предупреждения в логе после его настройки).
    pub env_file_warnings: Vec<String>,
}

/// Ошибка конфигурации: все найденные проблемы сразу.
#[derive(Debug, thiserror::Error)]
#[error("invalid configuration:\n  - {}", .problems.join("\n  - "))]
pub struct ConfigError {
    pub problems: Vec<String>,
}

#[derive(Clone)]
pub struct CoordinatorAccessKey(String);

impl CoordinatorAccessKey {
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for CoordinatorAccessKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl Settings {
    /// Читает `.env` из текущего каталога (если есть) и переменные окружения процесса.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_with_env_file(Path::new(".env"))
    }

    pub fn load_with_env_file(path: &Path) -> Result<Self, ConfigError> {
        let mut vars = HashMap::new();
        let mut warnings = Vec::new();
        // Сначала файл, затем окружение поверх — окружение важнее, как в pydantic-settings.
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let parsed = dotenv::parse(&text, &|k| std::env::var(k).ok());
                warnings = parsed
                    .warnings
                    .into_iter()
                    .map(|w| format!("{}: {w}", path.display()))
                    .collect();
                for (k, v) in parsed.vars {
                    vars.insert(k.to_lowercase(), v);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ConfigError {
                    problems: vec![format!("{}: {e}", path.display())],
                });
            }
        }
        for (k, v) in std::env::vars() {
            vars.insert(k.to_lowercase(), v);
        }
        let mut settings = Self::from_vars(vars)?;
        settings.env_file_warnings = warnings;
        Ok(settings)
    }

    /// Разбор из готового набора переменных (ключи — в любом регистре). Удобно для тестов.
    pub fn from_vars<I, K, V>(vars: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: Into<String>,
    {
        let vars: HashMap<String, String> = vars
            .into_iter()
            .map(|(k, v)| (k.as_ref().to_lowercase(), v.into()))
            .collect();
        let mut r = Reader {
            vars: &vars,
            problems: Vec::new(),
        };

        let environment = match r.raw("environment").map(str::trim) {
            None => Environment::Dev,
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "dev" => Environment::Dev,
                "test" => Environment::Test,
                "prod" => Environment::Prod,
                _ => {
                    r.problem(
                        "ENVIRONMENT",
                        format!("expected one of dev, test, prod, got {v:?}"),
                    );
                    Environment::Dev
                }
            },
        };

        let database_url = match r.raw("database_url") {
            None => {
                r.problem("DATABASE_URL", "is required".into());
                String::new()
            }
            Some(v) => normalize_database_url(v),
        };

        let source_naive_timezone = {
            let name = r.string("source_naive_timezone", "UTC");
            Tz::from_str(&name).unwrap_or_else(|_| {
                r.problem("SOURCE_NAIVE_TIMEZONE", format!("unknown timezone: {name}"));
                Tz::UTC
            })
        };

        let itsm_base_url = r
            .raw("itsm_base_url")
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| v.trim_end_matches('/').to_owned());

        let itsm_url_template = match r.raw("itsm_url_template") {
            None => Some("{base_url}/incidents/{sysId}".to_owned()),
            Some(v) if v.trim().is_empty() => None,
            Some(v) => {
                if !v.contains("{sysId}") && !v.contains("{id}") {
                    r.problem("ITSM_URL_TEMPLATE", "must contain {sysId} or {id}".into());
                }
                Some(v.trim().to_owned())
            }
        };

        let cors_origins = match r.raw("cors_origins") {
            None => Vec::new(),
            Some(v) if v.trim().is_empty() => Vec::new(),
            Some(v) => serde_json::from_str::<Vec<String>>(v).unwrap_or_else(|e| {
                r.problem("CORS_ORIGINS", format!("expected JSON array of strings: {e}"));
                Vec::new()
            }),
        };

        let llm_base_url = match r.raw("llm_base_url").map(str::trim) {
            None | Some("") => None,
            Some(v) => {
                let lower = v.to_ascii_lowercase();
                if !lower.starts_with("http://") && !lower.starts_with("https://") {
                    r.problem(
                        "LLM_BASE_URL",
                        format!("expected http:// or https:// URL, got {v:?}"),
                    );
                }
                Some(v.trim_end_matches('/').to_owned())
            }
        };
        let llm_model = r.string("llm_model", "qwen-local").trim().to_owned();
        if llm_model.is_empty() {
            r.problem("LLM_MODEL", "must not be empty".into());
        }
        let llm_model_revision = r
            .raw("llm_model_revision")
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        let llm_timeout = r.seconds("llm_timeout_seconds", 60.0, false);
        let llm_lease = r.seconds("llm_lease_seconds", 120.0, false);
        if llm_lease <= llm_timeout {
            r.problem(
                "LLM_LEASE_SECONDS",
                "must be greater than LLM_TIMEOUT_SECONDS".into(),
            );
        }

        let coordinator_accounts_enabled = r.boolean("coordinator_accounts_enabled", false);
        let mobile_push_enabled = r.boolean("mobile_push_enabled", false);
        let firebase_project_id = r
            .raw("firebase_project_id")
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        if mobile_push_enabled {
            if !coordinator_accounts_enabled {
                r.problem(
                    "MOBILE_PUSH_ENABLED",
                    "requires COORDINATOR_ACCOUNTS_ENABLED=true".into(),
                );
            }
            if !firebase_project_id.as_ref().is_some_and(|id| {
                id.len() <= 63 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            }) {
                r.problem(
                    "FIREBASE_PROJECT_ID",
                    "set the project ID from the Firebase console".into(),
                );
            }
        }
        let coordinator_access_key = if coordinator_accounts_enabled {
            None
        } else {
            r.raw("coordinator_access_key").and_then(|key| {
                if (32..=256).contains(&key.len()) && key.bytes().all(|byte| byte.is_ascii_graphic()) {
                    Some(CoordinatorAccessKey(key.to_owned()))
                } else {
                    r.problem(
                        "COORDINATOR_ACCESS_KEY",
                        "expected 32 to 256 printable ASCII characters without spaces".into(),
                    );
                    None
                }
            })
        };

        let settings = Settings {
            app_name: r.string("app_name", "MTS Incident Coordinator Dashboard"),
            environment,
            log_level: r.string("log_level", "INFO"),
            cors_origins,
            default_user_id: r.string("default_user_id", "coordinator"),
            coordinator_access_key,
            coordinator_accounts_enabled,
            mobile_push_enabled,
            firebase_project_id,
            api_host: r.string("api_host", "0.0.0.0"),
            api_port: r.number("api_port", 8080u16, |_| true, ""),
            database_url,
            db_pool_size: r.number("db_pool_size", 10u32, |_| true, ""),
            db_max_overflow: r.number("db_max_overflow", 10u32, |_| true, ""),
            db_echo: r.boolean("db_echo", false),
            json_source_path: PathBuf::from(r.string("json_source_path", "data/character_problems.json")),
            import_poll_interval: r.seconds("import_poll_interval_seconds", 1.0, false),
            import_error_threshold: r.number("import_error_threshold", 2u32, |v| *v >= 1, ">= 1"),
            import_stable_read_delay: r.seconds("import_stable_read_delay_seconds", 0.05, true),
            source_naive_timezone,
            importer_embedded: r.boolean("importer_embedded", false),
            demo_api_enabled: r.boolean("demo_api_enabled", false),
            demo_seed_path: vars
                .get("demo_seed_path")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            coordinator_group_name: r
                .string("coordinator_group_name", "КЦ ИТ Координаторы Массовых Инцидентов"),
            critical_resolved_window_hours: r.number(
                "critical_resolved_window_hours",
                24u32,
                |v| *v > 0,
                "> 0",
            ),
            deadline_warning_minutes: r.number("deadline_warning_minutes", 60u32, |v| *v > 0, "> 0"),
            itsm_base_url,
            itsm_url_template,
            sse_poll_interval: r.seconds("sse_poll_interval_seconds", 0.5, false),
            sse_heartbeat: r.seconds("sse_heartbeat_seconds", 15.0, false),
            events_retention: r.number("events_retention", 10_000u32, |v| *v >= 100, ">= 100"),
            llm_enabled: r.boolean("llm_enabled", false),
            llm_base_url,
            llm_model,
            llm_model_revision,
            llm_timeout,
            llm_max_tokens: r.number("llm_max_tokens", 200u32, |v| (16..=4096).contains(v), "16..=4096"),
            llm_max_attempts: r.number("llm_max_attempts", 3u32, |v| (1..=20).contains(v), "1..=20"),
            llm_poll_interval: r.seconds("llm_poll_interval_seconds", 1.0, false),
            llm_lease,
            llm_input_max_chars: r.number(
                "llm_input_max_chars",
                3000u32,
                |v| (200..=50_000).contains(v),
                "200..=50000",
            ),
            env_file_warnings: Vec::new(),
        };

        if r.problems.is_empty() {
            Ok(settings)
        } else {
            Err(ConfigError { problems: r.problems })
        }
    }

    /// Шаблон ссылки ITSM с подставленным `ITSM_BASE_URL`; `None`, если ссылку построить нельзя
    /// (порт `Settings.resolved_itsm_url_template`).
    pub fn resolved_itsm_url_template(&self) -> Option<String> {
        let mut template = self.itsm_url_template.clone()?;
        if template.contains("{base_url}") {
            template = template.replace("{base_url}", self.itsm_base_url.as_deref()?);
        }
        if self.environment == Environment::Prod && !template.starts_with("https://") {
            return None;
        }
        Some(template)
    }

    /// Идентификатор генерации в очереди (`analysis_jobs.model`): имя модели и, если задана, метка весов.
    /// Импортёр, API и `ai-worker` должны получать одинаковые `LLM_MODEL` и `LLM_MODEL_REVISION`.
    pub fn llm_job_model(&self) -> String {
        match &self.llm_model_revision {
            Some(rev) => format!("{}@{rev}", self.llm_model),
            None => self.llm_model.clone(),
        }
    }

    /// Размер пула sqlx: у SQLAlchemy это `pool_size + max_overflow`.
    pub fn db_max_connections(&self) -> u32 {
        (self.db_pool_size + self.db_max_overflow).max(1)
    }

    /// `LOG_LEVEL` в виде директивы `tracing` (`INFO` → `info`, `WARNING` → `warn`).
    pub fn tracing_level(&self) -> &'static str {
        match self.log_level.trim().to_ascii_uppercase().as_str() {
            "TRACE" => "trace",
            "DEBUG" => "debug",
            "WARN" | "WARNING" => "warn",
            "ERROR" | "CRITICAL" | "FATAL" => "error",
            _ => "info",
        }
    }
}

/// `postgresql+asyncpg://…`, `postgres+psycopg://…` → `postgres://…` (sqlx не знает суффикса драйвера).
pub fn normalize_database_url(url: &str) -> String {
    let url = url.trim();
    if let Some((scheme, rest)) = url.split_once("://") {
        let base = scheme.split('+').next().unwrap_or(scheme);
        if base.eq_ignore_ascii_case("postgresql") || base.eq_ignore_ascii_case("postgres") {
            return format!("postgres://{rest}");
        }
    }
    url.to_owned()
}

struct Reader<'a> {
    vars: &'a HashMap<String, String>,
    problems: Vec<String>,
}

impl<'a> Reader<'a> {
    fn raw(&self, key: &str) -> Option<&'a str> {
        self.vars.get(key).map(String::as_str)
    }

    fn problem(&mut self, key: &str, msg: String) {
        self.problems.push(format!("{key}: {msg}"));
    }

    fn string(&self, key: &str, default: &str) -> String {
        self.raw(key)
            .map(str::to_owned)
            .unwrap_or_else(|| default.to_owned())
    }

    fn number<T>(&mut self, key: &str, default: T, valid: impl Fn(&T) -> bool, rule: &str) -> T
    where
        T: FromStr + Copy + fmt::Display,
    {
        let Some(raw) = self.raw(key).map(str::trim) else {
            return default;
        };
        match raw.parse::<T>() {
            Ok(v) if valid(&v) => v,
            Ok(v) => {
                self.problem(&key.to_uppercase(), format!("must be {rule}, got {v}"));
                default
            }
            Err(_) => {
                self.problem(&key.to_uppercase(), format!("expected integer, got {raw:?}"));
                default
            }
        }
    }

    /// Секунды (`float`): `> 0` или, при `allow_zero`, `>= 0`.
    fn seconds(&mut self, key: &str, default: f64, allow_zero: bool) -> Duration {
        let value = match self.raw(key).map(str::trim) {
            None => default,
            Some(raw) => match raw.parse::<f64>() {
                Ok(v) if v.is_finite() && (v > 0.0 || (allow_zero && v == 0.0)) => v,
                Ok(_) => {
                    let rule = if allow_zero { ">= 0" } else { "> 0" };
                    self.problem(&key.to_uppercase(), format!("must be {rule}, got {raw}"));
                    default
                }
                Err(_) => {
                    self.problem(&key.to_uppercase(), format!("expected number, got {raw:?}"));
                    default
                }
            },
        };
        Duration::from_secs_f64(value)
    }

    /// Булевы значения в нотации pydantic: true/false, 1/0, yes/no, on/off, t/f, y/n.
    fn boolean(&mut self, key: &str, default: bool) -> bool {
        let Some(raw) = self.raw(key) else {
            return default;
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "t" | "yes" | "y" | "on" => true,
            "0" | "false" | "f" | "no" | "n" | "off" => false,
            _ => {
                self.problem(&key.to_uppercase(), format!("expected boolean, got {raw:?}"));
                default
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Vec<(&'static str, &'static str)> {
        vec![("DATABASE_URL", "postgresql+asyncpg://u:p@db:5432/d")]
    }

    fn with(extra: &[(&'static str, &'static str)]) -> Result<Settings, ConfigError> {
        let mut v = base();
        v.extend_from_slice(extra);
        Settings::from_vars(v)
    }

    #[test]
    fn defaults_match_python() {
        let s = with(&[]).unwrap();
        assert_eq!(s.database_url, "postgres://u:p@db:5432/d");
        assert_eq!(s.environment, Environment::Dev);
        assert_eq!(s.import_poll_interval, Duration::from_secs(1));
        assert_eq!(s.import_error_threshold, 2);
        assert_eq!(s.import_stable_read_delay, Duration::from_millis(50));
        assert_eq!(s.coordinator_group_name, "КЦ ИТ Координаторы Массовых Инцидентов");
        assert_eq!(s.critical_resolved_window_hours, 24);
        assert_eq!(s.deadline_warning_minutes, 60);
        assert_eq!(s.sse_poll_interval, Duration::from_millis(500));
        assert_eq!(s.sse_heartbeat, Duration::from_secs(15));
        assert_eq!(s.events_retention, 10_000);
        assert_eq!(
            s.itsm_url_template.as_deref(),
            Some("{base_url}/incidents/{sysId}")
        );
        assert_eq!(s.source_naive_timezone, Tz::UTC);
        assert_eq!(s.db_max_connections(), 20);
        assert_eq!(s.api_port, 8080);
        assert!(s.cors_origins.is_empty());
        assert!(s.coordinator_access_key.is_none());
        assert!(!s.coordinator_accounts_enabled);
    }

    #[test]
    fn coordinator_access_key_validation_redacts_value() {
        let short = "a-test-value-that-must-not-appear";
        for value in [
            "",
            &short[..16],
            "with a space and enough characters to pass the length check",
        ] {
            let error = with(&[("COORDINATOR_ACCESS_KEY", value)])
                .unwrap_err()
                .to_string();
            assert!(error.contains("COORDINATOR_ACCESS_KEY"));
            if !value.is_empty() {
                assert!(!error.contains(value));
            }
        }
        let test_key = "coordinator-session-key-use-only-in-tests";
        let settings = with(&[("COORDINATOR_ACCESS_KEY", test_key)]).unwrap();
        assert!(!format!("{settings:?}").contains(test_key));
    }

    #[test]
    fn accounts_mode_ignores_legacy_key_configuration() {
        let settings = with(&[
            ("COORDINATOR_ACCOUNTS_ENABLED", "true"),
            ("COORDINATOR_ACCESS_KEY", "legacy"),
        ])
        .unwrap();
        assert!(settings.coordinator_accounts_enabled);
        assert!(settings.coordinator_access_key.is_none());
    }

    #[test]
    fn database_url_is_required() {
        let err = Settings::from_vars(Vec::<(&str, &str)>::new()).unwrap_err();
        assert!(err.problems.iter().any(|p| p.starts_with("DATABASE_URL")));
    }

    #[test]
    fn database_url_schemes() {
        for (input, expected) in [
            ("postgresql+asyncpg://a@h/d", "postgres://a@h/d"),
            ("postgresql://a@h/d", "postgres://a@h/d"),
            ("postgres://a@h/d", "postgres://a@h/d"),
            ("  postgres+psycopg://a@h/d ", "postgres://a@h/d"),
        ] {
            assert_eq!(normalize_database_url(input), expected);
        }
    }

    #[test]
    fn keys_are_case_insensitive() {
        let s = with(&[("log_level", "debug"), ("Coordinator_Group_Name", "X")]).unwrap();
        assert_eq!(s.tracing_level(), "debug");
        assert_eq!(s.coordinator_group_name, "X");
    }

    #[test]
    fn itsm_template_rules() {
        // Пустой ITSM_BASE_URL при шаблоне с {base_url} -> ссылки нет.
        let s = with(&[("ITSM_BASE_URL", "")]).unwrap();
        assert_eq!(s.itsm_base_url, None);
        assert_eq!(s.resolved_itsm_url_template(), None);

        let s = with(&[("ITSM_BASE_URL", " https://itsm.example/ ")]).unwrap();
        assert_eq!(
            s.resolved_itsm_url_template().as_deref(),
            Some("https://itsm.example/incidents/{sysId}")
        );

        // Пустой шаблон -> None (а не значение по умолчанию).
        let s = with(&[("ITSM_URL_TEMPLATE", "  ")]).unwrap();
        assert_eq!(s.itsm_url_template, None);

        let err = with(&[("ITSM_URL_TEMPLATE", "https://x/{key}")]).unwrap_err();
        assert!(err.problems[0].starts_with("ITSM_URL_TEMPLATE"));

        // prod: только https.
        let s = with(&[("ENVIRONMENT", "prod"), ("ITSM_URL_TEMPLATE", "http://x/{id}")]).unwrap();
        assert_eq!(s.resolved_itsm_url_template(), None);
    }

    #[test]
    fn cors_origins_json_list() {
        let s = with(&[(
            "CORS_ORIGINS",
            r#"["http://localhost:5173","http://localhost:3000"]"#,
        )])
        .unwrap();
        assert_eq!(
            s.cors_origins,
            vec!["http://localhost:5173", "http://localhost:3000"]
        );
        assert!(with(&[("CORS_ORIGINS", "http://a,http://b")]).is_err());
    }

    #[test]
    fn constraints_are_reported_together() {
        let err = with(&[
            ("IMPORT_POLL_INTERVAL_SECONDS", "0"),
            ("IMPORT_ERROR_THRESHOLD", "0"),
            ("EVENTS_RETENTION", "99"),
            ("SOURCE_NAIVE_TIMEZONE", "Mars/Olympus"),
            ("IMPORTER_EMBEDDED", "maybe"),
            ("ENVIRONMENT", "staging"),
        ])
        .unwrap_err();
        assert_eq!(err.problems.len(), 6, "{:?}", err.problems);
        // 0 допустим для задержки устойчивого чтения (ge=0).
        assert!(with(&[("IMPORT_STABLE_READ_DELAY_SECONDS", "0")]).is_ok());
    }

    #[test]
    fn booleans_and_timezone() {
        let s = with(&[
            ("IMPORTER_EMBEDDED", "Yes"),
            ("SOURCE_NAIVE_TIMEZONE", "Europe/Moscow"),
        ])
        .unwrap();
        assert!(s.importer_embedded);
        assert_eq!(s.source_naive_timezone, chrono_tz::Europe::Moscow);
    }

    #[test]
    fn demo_api_is_disabled_by_default() {
        let s = with(&[]).unwrap();
        assert!(!s.demo_api_enabled);
        assert_eq!(s.demo_seed_path, None);
        let s = with(&[
            ("DEMO_API_ENABLED", "true"),
            ("DEMO_SEED_PATH", " /data/seed.json "),
        ])
        .unwrap();
        assert!(s.demo_api_enabled);
        assert_eq!(s.demo_seed_path, Some(PathBuf::from("/data/seed.json")));
    }

    /// `.env.example` в корне репозитория разбирается без ошибок.
    #[test]
    fn llm_is_disabled_by_default() {
        let s = with(&[]).unwrap();
        assert!(!s.llm_enabled);
        assert_eq!(s.llm_base_url, None);
        assert_eq!(s.llm_model, "qwen-local");
        assert_eq!(s.llm_job_model(), "qwen-local");
        assert_eq!(s.llm_timeout, Duration::from_secs(60));
        assert_eq!(s.llm_max_attempts, 3);
    }

    #[test]
    fn llm_settings_are_validated() {
        let s = with(&[
            ("LLM_ENABLED", "true"),
            ("LLM_BASE_URL", " http://172.18.0.1:18080/v1/ "),
            ("LLM_MODEL_REVISION", "99e7f220"),
        ])
        .unwrap();
        assert!(s.llm_enabled);
        assert_eq!(s.llm_base_url.as_deref(), Some("http://172.18.0.1:18080/v1"));
        assert_eq!(s.llm_job_model(), "qwen-local@99e7f220");

        let err = with(&[
            ("LLM_BASE_URL", "172.18.0.1:18080"),
            ("LLM_MODEL", " "),
            ("LLM_TIMEOUT_SECONDS", "200"),
            ("LLM_MAX_ATTEMPTS", "0"),
        ])
        .unwrap_err();
        let text = err.to_string();
        for key in [
            "LLM_BASE_URL",
            "LLM_MODEL:",
            "LLM_LEASE_SECONDS",
            "LLM_MAX_ATTEMPTS",
        ] {
            assert!(text.contains(key), "{key} not in {text}");
        }
    }

    #[test]
    fn shared_env_example_parses() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env.example");
        if !path.exists() {
            eprintln!("skip: {} not found", path.display());
            return;
        }
        let parsed = dotenv::parse(&std::fs::read_to_string(&path).unwrap(), &|_| None);
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        let s = Settings::from_vars(parsed.vars).unwrap();
        assert_eq!(s.database_url, "postgres://dashboard:change-me@db:5432/dashboard");
        assert_eq!(s.coordinator_group_name, "КЦ ИТ Координаторы Массовых Инцидентов");
        assert_eq!(s.cors_origins.len(), 2);
        assert_eq!(s.itsm_base_url, None);
    }
}
