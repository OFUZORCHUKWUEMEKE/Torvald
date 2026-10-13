//! Thin Claude Messages API client (raw HTTP — there is no official Rust SDK).
//!
//! Assistant `content` is kept as raw JSON and appended back verbatim so thinking
//! and fallback blocks survive round-trips untouched: the harness is append-only.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const MAX_ATTEMPTS: u32 = 6;

#[derive(Clone)]
pub struct Claude {
    http: Client,
    api_key: String,
    pub model: String,
    pub effort: String,
}

#[derive(Debug, Deserialize)]
pub struct Response {
    pub content: Vec<Value>,
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }
}

impl Claude {
    pub fn new(api_key: String, model: String, effort: String) -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self {
            http,
            api_key,
            model,
            effort,
        })
    }

    pub async fn create(&self, system: &str, tools: &[Value], messages: &[Value]) -> Result<Response> {
        let body = json!({
            "model": self.model,
            "max_tokens": 16000,
            "system": [{ "type": "text", "text": system, "cache_control": { "type": "ephemeral" } }],
            "tools": tools,
            "messages": messages,
            "thinking": { "type": "adaptive" },
            "output_config": { "effort": self.effort },
            // Automatic caching of the growing conversation tail.
            "cache_control": { "type": "ephemeral" },
            // On a safety-classifier refusal, let the API re-run the turn on a fallback model.
            "fallbacks": "default",
        });

        let mut attempt = 0;
        loop {
            attempt += 1;
            let sent = self
                .http
                .post(API_URL)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", "server-side-fallback-2026-07-01")
                .json(&body)
                .send()
                .await;

            let res = match sent {
                Ok(res) => res,
                Err(e) if attempt < MAX_ATTEMPTS => {
                    tracing::warn!(error = %e, attempt, "Claude request failed; retrying");
                    tokio::time::sleep(backoff(attempt, None)).await;
                    continue;
                }
                Err(e) => return Err(e).context("calling Claude"),
            };

            let status = res.status();
            if status.is_success() {
                return res.json::<Response>().await.context("decoding Claude response");
            }
            let retryable = status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::REQUEST_TIMEOUT
                || status.is_server_error()
                || status.as_u16() == 529;
            if retryable && attempt < MAX_ATTEMPTS {
                let retry_after = res
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());
                tracing::warn!(%status, attempt, "Claude API busy; retrying");
                tokio::time::sleep(backoff(attempt, retry_after)).await;
                continue;
            }
            bail!("Claude API HTTP {status}: {}", res.text().await.unwrap_or_default());
        }
    }
}

fn backoff(attempt: u32, retry_after_secs: Option<u64>) -> Duration {
    retry_after_secs
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(2u64.pow(attempt.min(6))))
}
