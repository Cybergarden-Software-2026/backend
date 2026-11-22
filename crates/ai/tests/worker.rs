//! Очередь `analysis_jobs`, `ai-worker` на имитации модели и выдача через `/analysis`.
//! Нужен `TEST_DATABASE_URL`; без него тесты пропускаются. Реальная модель не нужна.

#[path = "../../importer/tests/common/mod.rs"]
mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use common::{Env, Record, records, update};
use dashboard_ai::{CallError, LlmClient, Outcome, Worker};
use dashboard_domain::analysis::candidate_teams;
use dashboard_store::analysis_repo::{self, JobSpec};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::sync::watch;
use tower::ServiceExt;

/// Что отвечает имитация `llama-server`.
#[derive(Clone)]
enum Reply {
    Content(String),
    Status(u16),
    Length,
}

#[derive(Clone)]
struct MockLlm {
    reply: Arc<Mutex<Reply>>,
    /// Ответ на проверку группы (системный промпт про массовый сбой).
    cluster: Arc<Mutex<Reply>>,
    calls: Arc<AtomicUsize>,
    last: Arc<Mutex<Option<Value>>>,
    url: String,
}

impl MockLlm {
    async fn start() -> Self {
        let mock = Self {
            reply: Arc::new(Mutex::new(Reply::Content(r#"{"summary":"Не печатаются чеки.","team":null}"#.into()))),
            cluster: Arc::new(Mutex::new(Reply::Content(
                r#"{"related":true,"title":"Кассы не печатают чеки","summary":"Во всех записях кассы перестали печатать чеки."}"#.into(),
            ))),
            calls: Arc::new(AtomicUsize::new(0)),
            last: Arc::new(Mutex::new(None)),
            url: String::new(),
        };
        let state = mock.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<Value>| {
                let state = state.clone();
                async move {
                    state.calls.fetch_add(1, Ordering::SeqCst);
                    let is_cluster = body["messages"][0]["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("массового сбоя"));
                    *state.last.lock().unwrap() = Some(body);
                    let reply = if is_cluster { &state.cluster } else { &state.reply }.lock().unwrap().clone();
                    match reply {
                        Reply::Content(content) => (
                            StatusCode::OK,
                            axum::Json(json!({"choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": "stop"}]})),
                        ),
                        Reply::Length => (
                            StatusCode::OK,
                            axum::Json(json!({"choices": [{"message": {"content": "{\"summary\":\"Не"}, "finish_reason": "length"}]})),
                        ),
                        Reply::Status(code) => (StatusCode::from_u16(code).unwrap(), axum::Json(json!({"error": "x"}))),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url: format!("http://{addr}/v1"),
            ..mock
        }
    }

    fn set(&self, reply: Reply) {
        *self.reply.lock().unwrap() = reply;
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

const LLM_VARS: &[(&str, &str)] = &[("LLM_ENABLED", "true"), ("LLM_MAX_ATTEMPTS", "2")];

struct Ctx {
    env: Env,
    llm: MockLlm,
    worker: Worker,
    app: axum::Router,
    recs: Vec<Record>,
    stop: watch::Receiver<bool>,
    _stop_tx: watch::Sender<bool>,
}

impl Ctx {
    async fn new(extra: &[(&str, &str)]) -> Option<Self> {
        let mut vars = LLM_VARS.to_vec();
        vars.extend_from_slice(extra);
        let mut env = Env::with_vars(&vars).await?;
        let recs = records();
        env.write_json(&recs);
        env.tick().await;
        let llm = MockLlm::start().await;
        let client = LlmClient::new(&llm.url, Duration::from_secs(5)).unwrap();
        let worker = Worker::new(env.settings.clone(), env.pool().clone(), client);
        let app = dashboard_api::router(dashboard_api::AppState::new(
            env.settings.clone(),
            env.pool().clone(),
        ));
        let (stop_tx, stop) = watch::channel(false);
        Some(Self {
            env,
            llm,
            worker,
            app,
            recs,
            stop,
            _stop_tx: stop_tx,
        })
    }

    async fn process(&mut self) -> Option<Outcome> {
        self.worker.process_one(&mut self.stop).await.unwrap()
    }

    async fn analysis(&self, key: &str) -> Value {
        let req = Request::get(format!("/api/v1/incidents/{key}/analysis"))
            .body(Body::empty())
            .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn scalar(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(self.env.pool())
            .await
            .unwrap()
    }

    /// Ставит в начало очереди задачу карточки `id` (остальные откладываются).
    async fn only(&self, id: &str) {
        sqlx::query(
            "UPDATE analysis_jobs SET available_at = CASE WHEN incident_id = $1 THEN now() - interval '1 hour' \
             ELSE now() + interval '1 day' END WHERE status = 'queued'",
        )
        .bind(id)
        .execute(self.env.pool())
        .await
        .unwrap();
    }

    async fn reimport(&mut self) {
        self.env.write_json(&self.recs);
        self.env.tick().await;
    }

    async fn finish(self) {
        self.env.finish().await;
    }
}

fn present(recs: &[Record]) -> i64 {
    i64::try_from(recs.len()).unwrap()
}

#[tokio::test]
async fn import_enqueues_once_per_content_and_api_serves_llm_summary() {
    let Some(mut ctx) = Ctx::new(&[]).await else {
        return;
    };
    let total = present(&ctx.recs);
    assert_eq!(ctx.scalar("SELECT count(*) FROM analysis_jobs").await, total);
    assert_eq!(
        ctx.scalar("SELECT count(*) FROM analysis_jobs WHERE status = 'queued' AND kind = 'summary' AND model = 'qwen-local'")
            .await,
        total
    );

    // Неизменившийся импорт и повторная постановка (backfill) не создают задач.
    ctx.reimport().await;
    assert_eq!(ctx.worker.backfill().await.unwrap(), 0);
    assert_eq!(ctx.scalar("SELECT count(*) FROM analysis_jobs").await, total);

    let first = ctx.recs[0]["id"].as_str().unwrap().to_owned();
    let other = ctx.recs[1]["id"].as_str().unwrap().to_owned();
    // До готовности — разбор по правилам.
    assert_eq!(ctx.analysis(&first).await["provider"], "rules");

    ctx.only(&first).await;
    assert_eq!(ctx.process().await, Some(Outcome::Completed));
    assert_eq!(ctx.llm.calls(), 1);
    let request = ctx.llm.last.lock().unwrap().clone().unwrap();
    assert_eq!(request["model"], "qwen-local");
    assert_eq!(request["chat_template_kwargs"]["enable_thinking"], false);

    let analysis = ctx.analysis(&first).await;
    assert_eq!(analysis["provider"], "rules");
    assert_eq!(analysis["summary"], "Разбор ИИ. Не печатаются чеки.");
    for field in [
        "sysId",
        "revision",
        "generatedAt",
        "facts",
        "attention",
        "missing",
        "candidates",
    ] {
        assert!(analysis.get(field).is_some(), "{field}");
    }
    assert_eq!(ctx.analysis(&other).await["provider"], "rules");

    // Повторное открытие не вызывает генерацию.
    ctx.analysis(&first).await;
    assert_eq!(ctx.llm.calls(), 1);

    // Правка другого инцидента не инвалидирует выжимку.
    update(&mut ctx.recs[1], &[("description", json!("Другое описание"))]);
    ctx.reimport().await;
    assert_eq!(
        ctx.analysis(&first).await["summary"],
        "Разбор ИИ. Не печатаются чеки."
    );
    assert_eq!(ctx.scalar("SELECT count(*) FROM analysis_jobs").await, total + 1);

    // Правка описания этой карточки: старая выжимка сразу не выдаётся, ставится новая задача.
    update(
        &mut ctx.recs[0],
        &[("description", json!("Новое описание проблемы"))],
    );
    ctx.reimport().await;
    assert_eq!(ctx.analysis(&first).await["provider"], "rules");
    assert_eq!(ctx.scalar("SELECT count(*) FROM analysis_jobs").await, total + 2);
    ctx.only(&first).await;
    ctx.llm.set(Reply::Content(
        r#"{"summary":"Обновлённая выжимка.","team":null}"#.into(),
    ));
    assert_eq!(ctx.process().await, Some(Outcome::Completed));
    assert_eq!(
        ctx.analysis(&first).await["summary"],
        "Разбор ИИ. Обновлённая выжимка."
    );
    ctx.finish().await;
}

#[tokio::test]
async fn disabled_integration_does_not_enqueue_or_serve() {
    let Some(env) = Env::with_vars(&[]).await else {
        return;
    };
    let mut env = env;
    env.write_json(&records());
    env.tick().await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM analysis_jobs")
        .fetch_one(env.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    env.finish().await;
}

#[tokio::test]
async fn queued_job_with_stale_input_is_closed_without_calling_model() {
    let Some(mut ctx) = Ctx::new(&[]).await else {
        return;
    };
    let first = ctx.recs[0]["id"].as_str().unwrap().to_owned();
    let spec = JobSpec::summary(&ctx.env.settings);
    // Задача взята, но вход изменился до обращения к модели.
    ctx.only(&first).await;
    let mut conn = ctx.env.pool().acquire().await.unwrap();
    let job = analysis_repo::claim(&mut conn, &spec, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE incidents SET description = 'изменено', incident_revision = incident_revision + 1 WHERE id = $1")
        .bind(&first)
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        !analysis_repo::input_is_current(&mut conn, job.incident_pk, &job.input)
            .await
            .unwrap()
    );
    // Ожидающая задача с устаревшим входом снимается при постановке.
    sqlx::query("UPDATE analysis_jobs SET status = 'queued', attempts = 0 WHERE id = $1")
        .bind(job.id)
        .execute(&mut *conn)
        .await
        .unwrap();
    analysis_repo::enqueue(&mut conn, &spec).await.unwrap();
    let error: Option<String> = sqlx::query_scalar("SELECT error FROM analysis_jobs WHERE id = $1")
        .bind(job.id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(error.as_deref(), Some("obsolete"));
    drop(conn);

    // Новая задача с текущим входом обрабатывается; старая больше не берётся.
    ctx.only(&first).await;
    assert_eq!(ctx.process().await, Some(Outcome::Completed));
    assert_eq!(ctx.llm.calls(), 1);
    ctx.finish().await;
}

#[tokio::test]
async fn transient_errors_retry_then_fail_and_api_falls_back_to_rules() {
    let Some(mut ctx) = Ctx::new(&[]).await else {
        return;
    };
    let first = ctx.recs[0]["id"].as_str().unwrap().to_owned();
    ctx.llm.set(Reply::Status(503));
    ctx.only(&first).await;
    assert_eq!(ctx.process().await, Some(Outcome::Retry("HTTP 503".into())));
    assert_eq!(
        ctx.scalar("SELECT count(*) FROM analysis_jobs WHERE status = 'queued' AND attempts = 1 AND available_at > now()")
            .await,
        1
    );
    assert_eq!(ctx.analysis(&first).await["provider"], "rules");

    // Последняя попытка (LLM_MAX_ATTEMPTS=2) — задача завершается неуспехом.
    ctx.llm.set(Reply::Content("не JSON".into()));
    ctx.only(&first).await;
    let outcome = ctx.process().await.unwrap();
    assert!(
        matches!(outcome, Outcome::Failed(ref e) if e.starts_with("invalid output")),
        "{outcome:?}"
    );
    assert_eq!(
        ctx.scalar("SELECT count(*) FROM analysis_jobs WHERE status = 'failed' AND attempts = 2")
            .await,
        1
    );
    assert_eq!(ctx.analysis(&first).await["provider"], "rules");

    // Обрыв по лимиту токенов и 4xx не повторяются.
    let second = ctx.recs[1]["id"].as_str().unwrap().to_owned();
    ctx.llm.set(Reply::Length);
    ctx.only(&second).await;
    assert_eq!(
        ctx.process().await,
        Some(Outcome::Failed("generation stopped by max_tokens".into()))
    );
    ctx.finish().await;
}

#[tokio::test]
async fn late_answer_does_not_overwrite_newer_attempt() {
    let Some(ctx) = Ctx::new(&[]).await else { return };
    let first = ctx.recs[0]["id"].as_str().unwrap().to_owned();
    let spec = JobSpec::summary(&ctx.env.settings);
    ctx.only(&first).await;
    let mut conn = ctx.env.pool().acquire().await.unwrap();
    let old = analysis_repo::claim(&mut conn, &spec, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.attempt, 1);
    // Аренда истекла — задачу забирает следующая попытка.
    sqlx::query("UPDATE analysis_jobs SET locked_until = now() - interval '1 second' WHERE id = $1")
        .bind(old.id)
        .execute(&mut *conn)
        .await
        .unwrap();
    let new = analysis_repo::claim(&mut conn, &spec, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((new.id, new.attempt), (old.id, 2));

    let late = json!({"summary": "Старый ответ.", "team": null, "candidates": []});
    assert!(
        !analysis_repo::complete(&mut conn, &old, "llm", &late)
            .await
            .unwrap()
    );
    assert!(!analysis_repo::fail(&mut conn, &old, "x", None).await.unwrap());
    let fresh = json!({"summary": "Новый ответ.", "team": null, "candidates": []});
    assert!(
        analysis_repo::complete(&mut conn, &new, "llm", &fresh)
            .await
            .unwrap()
    );
    drop(conn);
    assert_eq!(ctx.analysis(&first).await["summary"], "Разбор ИИ. Новый ответ.");

    // Истёкшая аренда на последней попытке завершает задачу.
    let second = ctx.recs[1]["id"].as_str().unwrap().to_owned();
    ctx.only(&second).await;
    let mut conn = ctx.env.pool().acquire().await.unwrap();
    for _ in 0..2 {
        analysis_repo::claim(&mut conn, &spec, Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        sqlx::query(
            "UPDATE analysis_jobs SET locked_until = now() - interval '1 second' WHERE status = 'running'",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
    }
    ctx.only("none").await;
    assert!(
        analysis_repo::claim(&mut conn, &spec, Duration::from_secs(60))
            .await
            .unwrap()
            .is_none()
    );
    let status: String = sqlx::query_scalar("SELECT status FROM analysis_jobs WHERE incident_id = $1")
        .bind(&second)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(status, "failed");
    drop(conn);
    ctx.finish().await;
}

#[tokio::test]
async fn team_is_offered_only_from_rule_candidates() {
    let Some(mut ctx) = Ctx::new(&[]).await else {
        return;
    };
    // Карточка, для которой правила находят кандидатов.
    let mut conn = ctx.env.pool().acquire().await.unwrap();
    let rows = dashboard_store::read_repo::present_incidents(&mut conn)
        .await
        .unwrap();
    let teams = analysis_repo::team_names(&mut conn).await.unwrap();
    drop(conn);
    let coord = ctx.env.settings.coordinator_group_name.clone();
    let (item, candidate) = rows
        .iter()
        .find_map(|row| {
            candidate_teams(row, &rows, &coord)
                .into_iter()
                .find(|c| teams.iter().any(|t| t == c.team))
                .map(|c| (row, c))
        })
        .expect("incident with rule candidates");
    let id = item.id.clone();
    let team = candidate.team.to_owned();
    let evidence: Vec<String> = candidate.evidence_ids.iter().map(|s| (*s).to_owned()).collect();

    ctx.llm.set(Reply::Content(
        json!({"summary": "Описание сбоя.", "team": team}).to_string(),
    ));
    ctx.only(&id).await;
    assert_eq!(ctx.process().await, Some(Outcome::Completed));
    let request = ctx.llm.last.lock().unwrap().clone().unwrap();
    let user: Value = serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(user["candidates"].as_array().unwrap().contains(&json!(team)));

    let analysis = ctx.analysis(&id).await;
    assert_eq!(analysis["provider"], "llm");
    assert_eq!(analysis["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(analysis["candidates"][0]["team"], json!(team));
    assert_eq!(analysis["candidates"][0]["evidenceIds"], json!(evidence));

    // Если команды больше нет в справочнике, рекомендация не выдаётся.
    sqlx::query("UPDATE analysis_jobs SET result = jsonb_set(result, '{team}', '\"Выдуманная\"') WHERE incident_id = $1")
        .bind(&id)
        .execute(ctx.env.pool())
        .await
        .unwrap();
    let analysis = ctx.analysis(&id).await;
    assert_eq!(analysis["provider"], "llm");
    assert_eq!(analysis["candidates"], json!([]));

    // Модель вернула команду не из списка: сохраняется null.
    ctx.llm.set(Reply::Content(
        r#"{"summary":"Описание сбоя.","team":"Выдуманная"}"#.into(),
    ));
    update(&mut ctx.recs[0], &[("description", json!("другое"))]);
    ctx.reimport().await;
    let first = ctx.recs[0]["id"].as_str().unwrap().to_owned();
    ctx.only(&first).await;
    assert_eq!(ctx.process().await, Some(Outcome::Completed));
    let stored: Value = sqlx::query_scalar(
        "SELECT result FROM analysis_jobs WHERE incident_id = $1 AND status = 'completed' ORDER BY finished_at DESC LIMIT 1",
    )
    .bind(&first)
    .fetch_one(ctx.env.pool())
    .await
    .unwrap();
    assert_eq!(stored["team"], Value::Null);
    ctx.finish().await;
}

#[tokio::test]
async fn coordinator_analysis_uses_matching_active_evidence_and_falls_back_to_rules() {
    let Some(mut ctx) = Ctx::new(&[]).await else {
        return;
    };
    let coordinator = ctx.env.settings.coordinator_group_name.clone();
    let target = "INC000000008001";
    let active = "INC000000008002";
    let team = "Техническая команда";
    let record = |id: &str, day: u8, state: &str, group: &str| {
        serde_json::from_value::<Record>(json!({
            "id": id,
            "sysId": id,
            "header": "Сбой доставки",
            "submitDate": format!("2026-11-{day:02}T12:00:00Z"),
            "priorityId": 3,
            "stateEnum": state,
            "assignedGroupName": group,
            "service": "Доставка",
            "productName": "Заказы",
            "scenarioName": "Отправка"
        }))
        .unwrap()
    };
    ctx.recs = vec![
        record(target, 10, "Assigned", &coordinator),
        record(active, 12, "WorkInProgress", team),
        record("INC000000008003", 14, "Assigned", "В очереди команды"),
        record("INC000000008004", 16, "WorkInProgress", "AutoClosedMi"),
    ];
    ctx.reimport().await;

    ctx.llm.set(Reply::Content(
        json!({"summary": "Сбой доставки.", "team": team}).to_string(),
    ));
    ctx.only(target).await;
    assert_eq!(ctx.process().await, Some(Outcome::Completed));
    let request = ctx.llm.last.lock().unwrap().clone().unwrap();
    let user: Value = serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(user["candidates"], json!([team]));

    let analysis = ctx.analysis(target).await;
    assert_eq!(analysis["provider"], "llm");
    assert_eq!(analysis["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(analysis["candidates"][0]["team"], team);
    assert_eq!(analysis["candidates"][0]["evidenceIds"], json!([active]));

    for (description, choice) in [
        ("Не проходит оформление доставки.", Value::Null),
        ("Закрывается форма доставки.", json!("Выдуманная команда")),
    ] {
        // Новое описание ставит в очередь свежий запрос к модели.
        update(&mut ctx.recs[0], &[("description", json!(description))]);
        ctx.reimport().await;
        ctx.llm.set(Reply::Content(
            json!({"summary": "Сбой доставки.", "team": choice}).to_string(),
        ));
        ctx.only(target).await;
        assert_eq!(ctx.process().await, Some(Outcome::Completed));
        let fallback = ctx.analysis(target).await;
        assert_eq!(fallback["provider"], "rules");
        assert_eq!(fallback["summary"], "Разбор ИИ. Сбой доставки.");
        assert_eq!(fallback["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(fallback["candidates"][0]["team"], team);
        assert_eq!(fallback["candidates"][0]["evidenceIds"], json!([active]));
    }

    // Совпадения продукта со сценарием достаточно и во время исследования причин.
    update(
        &mut ctx.recs[1],
        &[
            ("service", json!("Другой сервис")),
            ("stateEnum", json!("Research")),
        ],
    );
    ctx.reimport().await;
    let analysis = ctx.analysis(target).await;
    assert_eq!(analysis["candidates"][0]["evidenceIds"], json!([active]));

    let completed = "INC000000008005";
    let mut resolved = record(completed, 18, "Resolved", "Команда из истории");
    update(
        &mut resolved,
        &[("completionDate", json!("2026-11-18T13:00:00Z"))],
    );
    ctx.recs.push(resolved);
    ctx.reimport().await;
    let analysis = ctx.analysis(target).await;
    assert_eq!(analysis["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(analysis["candidates"][0]["evidenceIds"], json!([completed]));
    ctx.recs.pop();

    // Продукт без совпавшего сценария не даёт оснований. В снимке остаются
    // исключённые команды, в том числе другая карточка координаторов в работе.
    update(&mut ctx.recs[1], &[("scenarioName", json!("Другой сценарий"))]);
    update(
        &mut ctx.recs[2],
        &[
            ("stateEnum", json!("WorkInProgress")),
            ("assignedGroupName", json!(coordinator)),
        ],
    );
    ctx.reimport().await;
    assert_eq!(ctx.analysis(target).await["candidates"], json!([]));
    ctx.finish().await;
}

#[tokio::test]
async fn model_revision_change_requeues_and_old_results_are_not_served() {
    let Some(mut ctx) = Ctx::new(&[]).await else {
        return;
    };
    let first = ctx.recs[0]["id"].as_str().unwrap().to_owned();
    let rules_summary = ctx.analysis(&first).await["summary"].clone();
    ctx.only(&first).await;
    assert_eq!(ctx.process().await, Some(Outcome::Completed));
    assert_eq!(
        ctx.analysis(&first).await["summary"],
        "Разбор ИИ. Не печатаются чеки."
    );

    let mut vars = vec![("DATABASE_URL", ctx.env.db.url.as_str())];
    vars.extend_from_slice(LLM_VARS);
    vars.push(("LLM_MODEL_REVISION", "99e7f220"));
    let settings = Arc::new(dashboard_config::Settings::from_vars(vars).unwrap());
    let spec = JobSpec::summary(&settings);
    assert_eq!(spec.model, "qwen-local@99e7f220");
    let mut conn = ctx.env.pool().acquire().await.unwrap();
    let queued = analysis_repo::enqueue(&mut conn, &spec).await.unwrap();
    assert_eq!(i64::try_from(queued).unwrap(), present(&ctx.recs));
    // Задачи прежней генерации снимаются с очереди.
    let stale: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM analysis_jobs WHERE model = 'qwen-local' AND status = 'queued'",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(stale, 0);
    drop(conn);
    let app = dashboard_api::router(dashboard_api::AppState::new(settings, ctx.env.pool().clone()));
    let req = Request::get(format!("/api/v1/incidents/{first}/analysis"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let body: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["provider"], "rules");
    assert_eq!(body["summary"], rules_summary);
    ctx.finish().await;
}

#[tokio::test]
async fn unreachable_model_is_transient() {
    let client = LlmClient::new("http://127.0.0.1:1/v1", Duration::from_secs(2)).unwrap();
    let err = client.chat(&json!({})).await.unwrap_err();
    assert!(matches!(err, CallError::Transient(_)), "{err:?}");
}

#[tokio::test]
async fn mass_problem_groups_are_reviewed_without_changing_membership() {
    let Some(mut ctx) = Ctx::new(&[]).await else {
        return;
    };
    // Две записи одного сервиса в пределах суток — автоматическая группа.
    let date = "2026-11-19T12:00:00Z";
    for (i, header) in [
        (0usize, "Кассы не печатают чеки"),
        (1, "Не печатаются чеки на кассах"),
    ] {
        update(
            &mut ctx.recs[i],
            &[
                ("service", json!("Кассовый сервис")),
                ("header", json!(header)),
                ("submitDate", json!(date)),
                ("stateEnum", json!("Assigned")),
                ("stateId", json!(0)),
            ],
        );
    }
    ctx.reimport().await;
    let get_groups = |app: axum::Router| async move {
        let req = Request::get("/api/v1/problems/clusters")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        body
    };
    let sys_a = ctx.recs[0]["sysId"].as_str().unwrap().to_owned();
    let find = |body: &Value| {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| {
                g["sysIds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|id| id == &json!(sys_a))
            })
            .cloned()
            .expect("group with first record")
    };
    let before = find(&get_groups(ctx.app.clone()).await);
    assert_eq!(before["title"], "Кассовый сервис");

    let reviews_before = ctx
        .scalar("SELECT count(*) FROM analysis_jobs WHERE kind = 'cluster'")
        .await;
    assert!(ctx.worker.sync_reviews().await.unwrap() >= 1);
    assert!(
        ctx.scalar("SELECT count(*) FROM analysis_jobs WHERE kind = 'cluster'")
            .await
            > reviews_before
    );
    // Повторная сверка без изменений ничего не добавляет.
    assert_eq!(ctx.worker.sync_reviews().await.unwrap(), 0);

    // Проверки групп берутся раньше выжимок.
    let mut done = 0;
    while ctx
        .scalar("SELECT count(*) FROM analysis_jobs WHERE kind = 'cluster' AND status = 'queued'")
        .await
        > 0
    {
        assert_eq!(ctx.process().await, Some(Outcome::Completed));
        done += 1;
        assert!(done < 100);
    }
    let after = find(&get_groups(ctx.app.clone()).await);
    assert_eq!(after["id"], before["id"]);
    assert_eq!(after["sysIds"], before["sysIds"]);
    assert_eq!(after["title"], "Кассы не печатают чеки");
    assert_eq!(
        after["reason"],
        "Один затронутый сервис; регистрации в пределах 24 часов. ИИ: Во всех записях кассы перестали печатать чеки."
    );

    // Изменение участника меняет вход: старая проверка не применяется, ставится новая.
    update(&mut ctx.recs[1], &[("description", json!("Другое описание"))]);
    ctx.reimport().await;
    let changed = find(&get_groups(ctx.app.clone()).await);
    assert_eq!(changed["title"], "Кассовый сервис");
    assert!(ctx.worker.sync_reviews().await.unwrap() >= 1);

    // Неподтверждённая группа остаётся, но с пометкой.
    ctx.llm.cluster.lock().unwrap().clone_from(&Reply::Content(
        r#"{"related":false,"title":"Разные проблемы","summary":"Записи описывают разные сбои."}"#.into(),
    ));
    while ctx
        .scalar("SELECT count(*) FROM analysis_jobs WHERE kind = 'cluster' AND status = 'queued'")
        .await
        > 0
    {
        assert_eq!(ctx.process().await, Some(Outcome::Completed));
    }
    let rejected = find(&get_groups(ctx.app.clone()).await);
    assert_eq!(rejected["title"], "Кассовый сервис");
    assert!(
        rejected["reason"]
            .as_str()
            .unwrap()
            .contains("ИИ не подтвердил общую проблему")
    );
    ctx.finish().await;
}
