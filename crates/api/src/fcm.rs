use std::time::{Duration, SystemTime};

use anyhow::{Context, ensure};
use dashboard_config::Settings;
use dashboard_domain::constants::NotificationType;
use gcp_auth::{CustomServiceAccount, TokenProvider};
use reqwest::{Client, StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};

const SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const MAX_TTL_SECONDS: u64 = 3600;

pub(crate) struct FcmClient {
    http: Client,
    endpoint: String,
    credentials: CustomServiceAccount,
}

pub(crate) struct PushEvent {
    pub notification_id: i64,
    pub bell_kind: Option<String>,
    pub incident_id: String,
    pub sys_id: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SendError {
    InvalidRegistration,
    Rejected,
    Retry { after: Option<Duration> },
}

impl FcmClient {
    pub(crate) fn from_settings(settings: &Settings) -> anyhow::Result<Self> {
        let project_id = settings
            .firebase_project_id
            .as_deref()
            .context("FIREBASE_PROJECT_ID is required")?;
        let path = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS")
            .context("GOOGLE_APPLICATION_CREDENTIALS is required")?;
        let key = std::fs::read_to_string(path).context("Failed to read Firebase service account file")?;
        let credentials = service_account(&key)?;
        let http = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            http,
            credentials,
            endpoint: format!("https://fcm.googleapis.com/v1/projects/{project_id}/messages:send"),
        })
    }

    pub(crate) async fn send(
        &self,
        token: &str,
        target_type: &str,
        event: &PushEvent,
        ttl: Duration,
    ) -> Result<(), SendError> {
        let payload = message(token, target_type, event, ttl)?;
        // The auth provider caches and refreshes OAuth tokens by scope.
        let Ok(Ok(access)) =
            tokio::time::timeout(Duration::from_secs(12), self.credentials.token(&[SCOPE])).await
        else {
            tracing::warn!("FCM OAuth token unavailable");
            return Err(SendError::Retry {
                after: Some(Duration::from_secs(60)),
            });
        };
        post_message(&self.http, &self.endpoint, access.as_str(), &payload).await
    }
}

fn service_account(key: &str) -> anyhow::Result<CustomServiceAccount> {
    #[derive(Deserialize)]
    struct Endpoint<'a> {
        token_uri: &'a str,
    }
    // gcp_auth assumes its token URI is valid when it builds the OAuth request.
    // Restrict the assertion to Google's token endpoint before starting the worker.
    let endpoint: Endpoint<'_> = serde_json::from_str(key).context("Invalid service account JSON")?;
    ensure!(
        matches!(
            endpoint.token_uri,
            "https://oauth2.googleapis.com/token"
                | "https://www.googleapis.com/oauth2/v4/token"
                | "https://accounts.google.com/o/oauth2/token"
        ),
        "Invalid Google OAuth endpoint in Firebase service account"
    );
    CustomServiceAccount::from_json(key).context("Failed to load Firebase service account")
}

fn message(token: &str, target_type: &str, event: &PushEvent, ttl: Duration) -> Result<Value, SendError> {
    // Validate the target before using it as the field name. A mobile registration
    // can address only one installation, including on older token-based SDKs.
    if !matches!(target_type, "fid" | "token") {
        return Err(SendError::Rejected);
    }
    let (event_type, title, fallback) = match event.bell_kind.as_deref() {
        None => (
            NotificationType::CriticalRaised.as_str(),
            "Критический сбой",
            "Обнаружен критический инцидент",
        ),
        Some("critical") => (
            "header.critical",
            "Критический инцидент обновлён",
            "Проверьте критический инцидент",
        ),
        Some("waiting") => (
            "header.waiting",
            "Инцидент ожидает распределения",
            "Инцидент на координаторах больше 8 минут",
        ),
        Some("deadline") => (
            "header.deadline",
            "Предупреждение о сроке решения",
            "Проверьте срок решения инцидента",
        ),
        Some(_) => return Err(SendError::Rejected),
    };
    let incident_id = event.incident_id.as_str();
    let body = if !incident_id.is_empty()
        && incident_id.len() <= 80
        && incident_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        format!("Инцидент {incident_id}")
    } else {
        fallback.into()
    };
    let ttl_seconds = ttl.as_secs().min(MAX_TTL_SECONDS);
    let id = event.notification_id.to_string();
    // Notification messages always collapse on Android. Its client renders each data
    // message locally, using notificationId to deduplicate at-least-once delivery.
    Ok(json!({ "message": {
        (target_type): token,
        "data": {
            "type": event_type, "notificationId": id,
            // Oversized source identifiers stay in the dashboard. Never truncate
            // an identifier into a different valid one or exceed FCM's 4 KiB data limit.
            "incidentId": if incident_id.len() <= 256 { incident_id } else { "" },
            "sysId": if event.sys_id.len() <= 256 { event.sys_id.as_str() } else { "" },
            "title": title, "body": body
        },
        "android": { "priority": "high", "ttl": format!("{ttl_seconds}s") },
        "webpush": {
            "headers": { "TTL": ttl_seconds.to_string(), "Urgency": "high" },
            "notification": { "title": title, "body": body, "tag": id }
        }
    } }))
}

async fn post_message(http: &Client, endpoint: &str, access: &str, payload: &Value) -> Result<(), SendError> {
    let response = http
        .post(endpoint)
        .bearer_auth(access)
        .json(payload)
        .send()
        .await
        .map_err(|_| SendError::Retry { after: None })?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    // Google's error text may contain the registration token. Log only the status.
    tracing::warn!(%status, "FCM send failed");
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after);
    if status == StatusCode::NOT_FOUND {
        let error: Value = response.json().await.unwrap_or(Value::Null);
        if error
            .pointer("/error/details")
            .and_then(Value::as_array)
            .is_some_and(|details| {
                details.iter().any(|detail| {
                    detail["@type"] == "type.googleapis.com/google.firebase.fcm.v1.FcmError"
                        && detail["errorCode"] == "UNREGISTERED"
                })
            })
        {
            return Err(SendError::InvalidRegistration);
        }
    }
    match status {
        StatusCode::TOO_MANY_REQUESTS
        | StatusCode::UNAUTHORIZED
        | StatusCode::FORBIDDEN
        | StatusCode::NOT_FOUND => Err(SendError::Retry {
            after: Some(retry_after.unwrap_or_default().max(Duration::from_secs(60))),
        }),
        StatusCode::REQUEST_TIMEOUT => Err(SendError::Retry { after: retry_after }),
        status if status.is_server_error() => Err(SendError::Retry { after: retry_after }),
        _ => Err(SendError::Rejected),
    }
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    value.parse::<u64>().map(Duration::from_secs).ok().or_else(|| {
        httpdate::parse_http_date(value)
            .ok()
            .map(|date| date.duration_since(SystemTime::now()).unwrap_or_default())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, routing::post};

    fn event(bell_kind: Option<&str>) -> PushEvent {
        PushEvent {
            notification_id: 42,
            bell_kind: bell_kind.map(str::to_owned),
            incident_id: "INC123".into(),
            sys_id: "001".into(),
        }
    }

    async fn mock_fcm(headers: HeaderMap, Json(payload): Json<Value>) -> impl axum::response::IntoResponse {
        assert_eq!(headers["authorization"], "Bearer test-access");
        let message = &payload["message"];
        assert_eq!(
            message["data"],
            json!({
                "type": "critical.raised", "notificationId": "42", "incidentId": "INC123", "sysId": "001",
                "title": "Критический сбой", "body": "Инцидент INC123"
            })
        );
        assert!(message.get("notification").is_none());
        assert_eq!(message["android"], json!({"priority": "high", "ttl": "55s"}));
        assert_eq!(message["webpush"]["notification"]["tag"], "42");
        let target = match (message.get("fid"), message.get("token")) {
            (Some(fid), None) => {
                assert_eq!(fid, "installation");
                "ok"
            }
            (None, Some(token)) => token.as_str().unwrap(),
            _ => panic!("expected one installation target"),
        };
        let status = match target {
            "ok" => StatusCode::OK,
            "retired" | "wrong-project" => StatusCode::NOT_FOUND,
            "bad-payload" => StatusCode::BAD_REQUEST,
            "wrong-sender" => StatusCode::FORBIDDEN,
            "busy" => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        let details = if target == "retired" {
            json!([{"@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError", "errorCode": "UNREGISTERED"}])
        } else {
            json!([])
        };
        (
            status,
            [("retry-after", "90")],
            Json(json!({"error": {"details": details}})),
        )
    }

    #[tokio::test]
    async fn sends_authenticated_data_message_and_classifies_fcm_responses() {
        let app = Router::new().route("/v1/projects/test/messages:send", post(mock_fcm));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/v1/projects/test/messages:send",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let retry = || {
            Err(SendError::Retry {
                after: Some(Duration::from_secs(90)),
            })
        };
        let http = Client::new();
        for (target_type, token, expected) in [
            ("fid", "installation", Ok(())),
            ("token", "ok", Ok(())),
            ("token", "retired", Err(SendError::InvalidRegistration)),
            ("token", "bad-payload", Err(SendError::Rejected)),
            ("token", "wrong-project", retry()),
            ("token", "wrong-sender", retry()),
            ("token", "busy", retry()),
            ("token", "unavailable", retry()),
        ] {
            let payload = message(token, target_type, &event(None), Duration::from_secs(55)).unwrap();
            assert_eq!(
                post_message(&http, &endpoint, "test-access", &payload).await,
                expected
            );
        }
        server.abort();
    }

    #[test]
    fn all_header_signals_include_display_text_for_android_and_web() {
        for (kind, event_type, title) in [
            ("critical", "header.critical", "Критический инцидент обновлён"),
            ("waiting", "header.waiting", "Инцидент ожидает распределения"),
            ("deadline", "header.deadline", "Предупреждение о сроке решения"),
        ] {
            let event = event(Some(kind));
            let payload = message("installation", "fid", &event, Duration::from_secs(300)).unwrap();
            let msg = &payload["message"];
            let data = &msg["data"];
            assert_eq!(data["type"], event_type);
            assert_eq!(data["title"], title);
            assert_eq!(data["body"], "Инцидент INC123");
            assert_eq!(data["notificationId"], "42");
            assert_eq!(data["sysId"], "001");
            assert_eq!(data["incidentId"], "INC123");
            assert_eq!(msg["android"], json!({"priority": "high", "ttl": "300s"}));
            assert!(msg.get("notification").is_none());
            assert_eq!(
                msg["webpush"]["headers"],
                json!({"TTL": "300", "Urgency": "high"})
            );
            assert_eq!(
                msg["webpush"]["notification"],
                json!({"title": title, "body": data["body"], "tag": "42"})
            );
        }
    }

    #[test]
    fn rejects_unsupported_credential_endpoint_and_target() {
        let error = service_account(r#"{"token_uri":"not a Google token endpoint"}"#).unwrap_err();
        assert!(error.to_string().contains("Invalid Google OAuth endpoint"));
        assert_eq!(
            message("invalid", "topic", &event(None), Duration::ZERO),
            Err(SendError::Rejected)
        );
    }
}
