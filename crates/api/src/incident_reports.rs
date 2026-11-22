use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, State, rejection::JsonRejection};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use dashboard_importer::{ImportError, ImportService, NativeFormReport};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::{Uuid, Variant};

use crate::response::PyJson;
use crate::{ApiResult, AppError, AppState, mobile, request_id};

const FORM: &str = include_str!("../static/incident_reports/index.html");
const CSS: &str = include_str!("../static/incident_reports/form.css");
const JS: &str = include_str!("../static/incident_reports/form.js");
const FORM_BODY_LIMIT: usize = 32 * 1024;
const REPORT_BURST: u64 = 12;

struct SubmissionBudget {
    tokens: u64,
    last_refill: Instant,
}

impl SubmissionBudget {
    fn take(&mut self) -> bool {
        let elapsed_seconds = self.last_refill.elapsed().as_secs();
        if elapsed_seconds > 0 {
            self.tokens = (self.tokens + elapsed_seconds.min(REPORT_BURST)).min(REPORT_BURST);
            self.last_refill += Duration::from_secs(elapsed_seconds);
        }
        if let Some(remaining) = self.tokens.checked_sub(1) {
            self.tokens = remaining;
            true
        } else {
            false
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    // The ingress may be behind multiple proxies. Bound the submission rate without relying
    // on IP headers supplied by a client. The budget belongs to this router and its clones.
    let budget = Arc::new(Mutex::new(SubmissionBudget {
        tokens: REPORT_BURST,
        last_refill: Instant::now(),
    }));
    Router::new()
        .route("/incident-reports/form", get(form))
        .route(
            "/incident-reports/form.css",
            get(|| async { asset("text/css; charset=utf-8", CSS) }),
        )
        .route(
            "/incident-reports/form.js",
            get(|| async { asset("text/javascript; charset=utf-8", JS) }),
        )
        .route(
            "/incident-reports",
            post(submit)
                .route_layer(Extension(budget))
                .layer(DefaultBodyLimit::max(FORM_BODY_LIMIT)),
        )
}

fn asset(content_type: &'static str, contents: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        contents,
    )
        .into_response()
}

async fn form() -> Response {
    let mut response = asset("text/html; charset=utf-8", FORM);
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(concat!(
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; ",
            "base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
        )),
    );
    response
        .headers_mut()
        .insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportInput {
    submission_id: String,
    title: String,
    description: String,
    service: String,
    priority: Priority,
    allocated_minutes: Option<u32>,
}

#[derive(Deserialize)]
enum Priority {
    P1,
    P2,
    P3,
    P4,
}

impl Priority {
    fn dashboard_id(self) -> i64 {
        match self {
            Self::P1 => 4,
            Self::P2 => 3,
            Self::P3 => 2,
            Self::P4 => 1,
        }
    }
}

fn validated_text(input: String, limit: usize, message: &'static str) -> ApiResult<String> {
    let text = input.trim();
    if text.is_empty() || text.contains('\0') || text.chars().count() > limit {
        return Err(AppError::validation(message));
    }
    Ok(text.to_owned())
}

impl ReportInput {
    fn validate(self) -> ApiResult<NativeFormReport> {
        let uuid = Uuid::parse_str(&self.submission_id)
            .ok()
            .filter(|value| {
                value.get_version_num() == 4
                    && value.get_variant() == Variant::RFC4122
                    && value.to_string() == self.submission_id
            })
            .ok_or_else(|| AppError::validation("Поле submission_id должно содержать UUID версии 4."))?;
        if self
            .allocated_minutes
            .is_some_and(|minutes| !(1..=525_600).contains(&minutes))
        {
            return Err(AppError::validation("Укажите от 1 до 525600 минут на решение."));
        }
        Ok(NativeFormReport {
            submission_id: uuid.to_string(),
            title: validated_text(self.title, 160, "Укажите заголовок длиной до 160 символов.")?,
            description: validated_text(
                self.description,
                5000,
                "Укажите описание длиной до 5000 символов.",
            )?,
            service: validated_text(self.service, 160, "Укажите сервис длиной до 160 символов.")?,
            priority_id: self.priority.dashboard_id(),
            allocated_minutes: self.allocated_minutes,
        })
    }
}

async fn submit(
    State(state): State<AppState>,
    Extension(budget): Extension<Arc<Mutex<SubmissionBudget>>>,
    body: Result<Json<ReportInput>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(input) = body.map_err(mobile::json_error)?;
    let report = input.validate()?;
    if !budget.lock().await.take() {
        let mut response = AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "TOO_MANY_REQUESTS",
            "Слишком много заявок. Повторите отправку через секунду.",
        )
        .into_response();
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return Ok(response);
    }
    let service = ImportService::with_clock(state.pool, state.settings, state.clock);
    let receipt = service
        .submit_form_report(report)
        .await
        .map_err(|error| match error {
            ImportError::FormReportConflict => {
                AppError::conflict("Этот идентификатор уже использован для другой заявки.")
                    .with_code("FORM_REPORT_CONFLICT")
            }
            _ => {
                tracing::error!(request_id = %request_id::current(), "failed to store a form report");
                AppError::internal()
            }
        })?;
    let status = if receipt.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        PyJson(json!({"id": receipt.id, "created": receipt.created})),
    )
        .into_response())
}
