//! HTTP-клиент OpenAI-совместимого `llama-server`: `POST {LLM_BASE_URL}/chat/completions`.

use std::time::Duration;

use serde_json::Value;

/// Предел размера ответа модели: выжимка и JSON-обёртка занимают единицы килобайт.
const MAX_RESPONSE_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// Сеть, таймаут, 429/5xx — стоит повторить позже.
    #[error("{0}")]
    Transient(String),
    /// Ответ, который повторный запрос не исправит: 4xx, обрыв по лимиту токенов, нет текста.
    #[error("{0}")]
    Permanent(String),
}

#[derive(Debug, Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    url: String,
}

impl LlmClient {
    /// `base_url` уже содержит `/v1` (например, `http://172.18.0.1:18080/v1`). Переменные прокси
    /// окружения не используются: модель доступна только по приватному адресу.
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .no_proxy()
            .build()?;
        Ok(Self {
            http,
            url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
        })
    }

    /// Отправляет запрос и возвращает `choices[0].message.content`.
    pub async fn chat(&self, body: &Value) -> Result<String, CallError> {
        let mut response = self
            .http
            .post(&self.url)
            .json(body)
            .send()
            .await
            .map_err(describe)?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(describe)? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(CallError::Permanent("response is larger than 1 MiB".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let message = format!("HTTP {}", status.as_u16());
            return Err(
                if status.as_u16() == 429 || status.is_server_error() || status.as_u16() == 408 {
                    CallError::Transient(message)
                } else {
                    CallError::Permanent(message)
                },
            );
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| CallError::Transient(format!("response is not JSON: {e}")))?;
        let choice = value
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| CallError::Permanent("response has no choices".into()))?;
        if choice.get("finish_reason").and_then(Value::as_str) == Some("length") {
            return Err(CallError::Permanent("generation stopped by max_tokens".into()));
        }
        choice
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| CallError::Permanent("empty message content".into()))
    }
}

fn describe(error: reqwest::Error) -> CallError {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection failed"
    } else {
        "request failed"
    };
    // Адрес модели и тело запроса в журнал не попадают.
    CallError::Transient(kind.to_owned())
}
