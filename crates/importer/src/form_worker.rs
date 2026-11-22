use std::time::Duration;

use chrono::{DateTime, Utc};
use dashboard_config::IncidentFormRelay;
use dashboard_store::import_repo;
use reqwest::{Client, StatusCode, redirect::Policy};
use serde::Deserialize;
use serde_json::Value;
use uuid::{Uuid, Variant};

use crate::service::ImportService;
use crate::worker::StopSignal;

pub(crate) const FORM_ID: &str = "incident-web";
const PAGE_LIMIT: usize = 100;
const BODY_LIMIT: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

pub(crate) struct FormReport {
    pub id: i64,
    pub answer_id: String,
    pub received_at: DateTime<Utc>,
    pub title: String,
    pub description: String,
    pub service: String,
    pub priority_id: i64,
}

#[derive(Deserialize)]
struct RelayPage {
    items: Vec<RelayItem>,
    next_after: i64,
    has_more: bool,
}

#[derive(Deserialize)]
struct RelayItem {
    id: i64,
    form_id: String,
    answer_id: String,
    received_at: DateTime<Utc>,
    payload: Value,
}

#[derive(Deserialize)]
struct ReportPayload {
    answers: ReportAnswers,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReportAnswers {
    title: String,
    description: String,
    service: String,
    priority_id: i64,
}

#[derive(Debug, thiserror::Error)]
enum PollError {
    #[error("не удалось выполнить HTTPS-запрос")]
    Request,
    #[error("приёмник вернул HTTP {0}")]
    Status(StatusCode),
    #[error("ответ приёмника превышает допустимый размер")]
    BodyLimit,
    #[error("некорректная страница после курсора {0}")]
    Page(i64),
    #[error("некорректные данные заявки {0}")]
    Report(i64),
    #[error("не удалось прочитать курсор заявок")]
    Cursor,
    #[error("не удалось сохранить страницу после курсора {0}")]
    Save(i64),
}

impl RelayItem {
    fn into_report(self) -> Result<FormReport, PollError> {
        let id = self.id;
        let invalid = || PollError::Report(id);
        let uuid = Uuid::parse_str(&self.answer_id).map_err(|_| invalid())?;
        if uuid.get_version_num() != 4
            || uuid.get_variant() != Variant::RFC4122
            || uuid.to_string() != self.answer_id
        {
            return Err(invalid());
        }
        let ReportPayload { answers } = serde_json::from_value(self.payload).map_err(|_| invalid())?;
        let bounded = |value: &str, limit| !value.trim().is_empty() && value.chars().count() <= limit;
        if !bounded(&answers.title, 160)
            || !bounded(&answers.description, 5000)
            || !bounded(&answers.service, 160)
            || !(1..=4).contains(&answers.priority_id)
        {
            return Err(invalid());
        }
        Ok(FormReport {
            id,
            answer_id: self.answer_id,
            received_at: self.received_at,
            title: answers.title,
            description: answers.description,
            service: answers.service,
            priority_id: answers.priority_id,
        })
    }
}

async fn fetch_page(client: &Client, relay: &IncidentFormRelay, after: i64) -> Result<RelayPage, PollError> {
    let mut endpoint = relay.endpoint.clone();
    endpoint
        .query_pairs_mut()
        .append_pair("after", &after.to_string())
        .append_pair("limit", &PAGE_LIMIT.to_string());
    let mut response = client
        .get(endpoint)
        .bearer_auth(relay.read_token())
        .send()
        .await
        .map_err(|_| PollError::Request)?;
    if response.status() != StatusCode::OK {
        return Err(PollError::Status(response.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| PollError::Request)? {
        if chunk.len() > BODY_LIMIT - body.len() {
            return Err(PollError::BodyLimit);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| PollError::Page(after))
}

async fn poll(client: &Client, service: &ImportService, relay: &IncidentFormRelay) -> Result<(), PollError> {
    let mut after = {
        let mut conn = service.pool().acquire().await.map_err(|_| PollError::Cursor)?;
        import_repo::forms_cursor(&mut conn)
            .await
            .map_err(|_| PollError::Cursor)?
    };
    loop {
        let RelayPage {
            items,
            next_after,
            has_more,
        } = fetch_page(client, relay, after).await?;
        let mut last = after;
        if items.len() > PAGE_LIMIT || (items.is_empty() && has_more) {
            return Err(PollError::Page(after));
        }
        let mut reports = Vec::new();
        for item in items {
            if item.id <= last {
                return Err(PollError::Page(after));
            }
            last = item.id;
            if item.form_id == FORM_ID {
                reports.push(item.into_report()?);
            }
        }
        if last != next_after {
            return Err(PollError::Page(after));
        }
        if next_after == after {
            return Ok(());
        }
        let applied = service
            .apply_form_page(after, next_after, reports)
            .await
            .map_err(|_| PollError::Save(after))?;
        if !applied || !has_more {
            return Ok(());
        }
        after = next_after;
    }
}

pub(crate) async fn run(service: ImportService, mut stop: StopSignal) {
    let Some(relay) = service.settings().incident_form_relay.as_ref() else {
        return;
    };
    let client = match Client::builder()
        .https_only(relay.endpoint.scheme() == "https")
        .redirect(Policy::none())
        .no_proxy()
        .timeout(REQUEST_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            tracing::error!("Не удалось создать HTTP-клиент для импорта заявок");
            return;
        }
    };
    tracing::info!(
        interval_secs = relay.poll_interval.as_secs(),
        "Запущен импорт заявок из веб-формы"
    );
    let delay_cap = Duration::from_secs(30).max(relay.poll_interval);
    let mut delay = relay.poll_interval;
    while !*stop.borrow() {
        let outcome = tokio::select! {
            _ = stop.wait_for(|stopped| *stopped) => return,
            outcome = poll(&client, &service, relay) => outcome,
        };
        if let Err(error) = &outcome {
            tracing::warn!(%error, retry_in_secs = delay.as_secs(), "Сбой импорта заявок из веб-формы");
        } else {
            delay = relay.poll_interval;
        }
        tokio::select! {
            _ = stop.wait_for(|stopped| *stopped) => return,
            _ = tokio::time::sleep(delay) => {}
        }
        if outcome.is_err() {
            delay = delay.saturating_mul(2).min(delay_cap);
        }
    }
}
