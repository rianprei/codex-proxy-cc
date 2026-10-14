//! HTTP client for the Codex backend (`chatgpt.com/backend-api/codex`).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::Stream;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{AuthStore, Credentials};

pub const DEFAULT_BASE: &str = "https://chatgpt.com/backend-api/codex";
const CLIENT_VERSION: &str = "0.153.4";
const ORIGINATOR: &str = "codex_cli_rs";

/// Response headers worth surfacing to the client (rate-limit telemetry).
pub const FORWARDED_HEADERS: &[&str] = &[
    "x-codex-primary-used-percent",
    "x-codex-primary-reset-after-seconds",
    "x-codex-plan-type",
];

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("upstream returned HTTP {status}: {body}")]
    Status { status: StatusCode, body: String },
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReasoningLevel {
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpstreamModel {
    pub slug: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<ReasoningLevel>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    models: Vec<UpstreamModel>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reasoning {
    pub effort: String,
    pub summary: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextOptions {
    pub verbosity: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub instructions: String,
    pub input: Vec<Value>,
    pub tools: Vec<Value>,
    pub tool_choice: Value,
    pub parallel_tool_calls: bool,
    pub reasoning: Reasoning,
    pub store: bool,
    pub stream: bool,
    pub include: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextOptions>,
}

pub struct ResponsesStream {
    pub headers: Vec<(String, String)>,
    pub body: std::pin::Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
}

pub struct Upstream {
    base: String,
    http: reqwest::Client,
    auth: Arc<AuthStore>,
    session_id: String,
}

/// Quota-exhaustion detector: the ONE predicate for "this 429 is a spent plan
/// limit, not a transient throttle". Shared by the retry decision here and the
/// terminal retry-after in server.rs — a single point to update if the
/// upstream ever rewords the message (fleet finding: the substring was
/// duplicated in two independent sites).
pub fn is_quota_exhaustion(body: &str) -> bool {
    body.contains("usage limit")
}

/// Parse the upstream `Retry-After` header and clamp it to `cap`.
///
/// Only the delta-seconds form is honoured: the HTTP-date form is rare in
/// practice for these backends and parsing it adds a timezone dependency for
/// no observed benefit. Missing or malformed values fall back to a short
/// fixed pause so a retriable failure still gets its retry.
fn retry_after_delay(headers: &HeaderMap, cap: Duration) -> Duration {
    let fallback = Duration::from_secs(2);
    let Some(raw) = headers.get("retry-after").and_then(|v| v.to_str().ok()) else {
        return fallback;
    };
    let Ok(secs) = raw.trim().parse::<u64>() else {
        return fallback;
    };
    Duration::from_secs(secs).min(cap)
}

/// Test-only re-export so the integration suite can exercise the clamp
/// without depending on crate-private visibility rules.
pub fn retry_after_delay_for_test(headers: &HeaderMap, cap: Duration) -> Duration {
    retry_after_delay(headers, cap)
}

impl Upstream {
    pub fn new(base: String, auth: Arc<AuthStore>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .build()
            .context("build http client")?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            auth,
            session_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    fn headers(&self, creds: &Credentials) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        let mut bearer = HeaderValue::from_str(&format!("Bearer {}", creds.access_token))
            .context("access token is not a valid header value")?;
        bearer.set_sensitive(true);
        h.insert(AUTHORIZATION, bearer);
        h.insert(
            "chatgpt-account-id",
            HeaderValue::from_str(&creds.account_id).context("account id header")?,
        );
        h.insert("originator", HeaderValue::from_static(ORIGINATOR));
        h.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("codex_cli_rs/", "0.153.4")),
        );
        h.insert(
            "OpenAI-Beta",
            HeaderValue::from_static("responses=experimental"),
        );
        h.insert("version", HeaderValue::from_static(CLIENT_VERSION));
        h.insert(
            "session-id",
            HeaderValue::from_str(&self.session_id).context("session id header")?,
        );
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(h)
    }

    /// Fetch the model catalogue. Cheap enough to call on demand; cached by `models::Catalog`.
    pub async fn list_models(
        &self,
        timeout: Duration,
    ) -> Result<Vec<UpstreamModel>, UpstreamError> {
        let url = format!("{}/models?client_version={}", self.base, CLIENT_VERSION);
        let mut creds = self.auth.credentials().await?;
        for attempt in 0..2 {
            let resp = self
                .http
                .get(&url)
                .headers(self.headers(&creds)?)
                .timeout(timeout)
                .send()
                .await
                .context("GET /models")?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && attempt == 0 {
                creds = self.auth.refresh_after_401(&creds.access_token).await?;
                continue;
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(UpstreamError::Status { status, body });
            }
            let parsed: ModelsResponse = resp.json().await.context("parse /models")?;
            return Ok(parsed.models);
        }
        Err(anyhow::anyhow!("unreachable: /models retry loop exhausted").into())
    }

    /// Start a streaming `/responses` call. Non-2xx statuses are returned as errors with the
    /// body; a 401 triggers one token refresh and one retry. 429 and 5xx are
    /// retried up to `MAX_UPSTREAM_RETRIES` times, honouring (and capping)
    /// the upstream `Retry-After`. Quota exhaustion is NOT retried: a spent
    /// plan limit will not recover inside a retry window. Retrying here is
    /// safe: nothing has been streamed to the client until this returns Ok.
    pub async fn responses(
        &self,
        req: &ResponsesRequest,
    ) -> Result<ResponsesStream, UpstreamError> {
        const MAX_UPSTREAM_RETRIES: u32 = 2;
        /// Never sleep longer than this on a Retry-After, even if the upstream
        /// asks for more: the client is synchronously waiting and a long pause
        /// reads as a hang.
        const RETRY_AFTER_CAP: Duration = Duration::from_secs(20);

        let url = format!("{}/responses", self.base);
        let mut creds = self.auth.credentials().await?;
        let mut rate_limit_retries = 0;
        let mut auth_retries = 0;
        // 1 initial attempt + 1 auth retry + MAX rate-limit retries. The
        // internal counters bound each class; this bound guarantees termination.
        for _ in 0..(1 + 1 + MAX_UPSTREAM_RETRIES) {
            let mut headers = self.headers(&creds)?;
            headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
            headers.insert(
                "x-client-request-id",
                HeaderValue::from_str(&uuid::Uuid::new_v4().to_string())
                    .context("request id header")?,
            );
            let resp = self
                .http
                .post(&url)
                .headers(headers)
                .json(req)
                .send()
                .await
                .context("POST /responses")?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && auth_retries == 0 {
                auth_retries += 1;
                tracing::info!("upstream 401; refreshing token and retrying once");
                creds = self.auth.refresh_after_401(&creds.access_token).await?;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS && rate_limit_retries < MAX_UPSTREAM_RETRIES
            {
                // Distinguish quota exhaustion from a transient rate limit: a
                // spent plan limit will not recover within a retry window, so
                // retrying only delays the inevitable error the client waits
                // for (and stacks on the client's own backoff).
                let retry_after = retry_after_delay(resp.headers(), RETRY_AFTER_CAP);
                let body_text = resp.text().await.unwrap_or_default();
                if is_quota_exhaustion(&body_text) {
                    tracing::warn!("upstream reports usage limit reached; not retrying");
                    return Err(UpstreamError::Status {
                        status,
                        body: body_text,
                    });
                }
                rate_limit_retries += 1;
                tracing::warn!(
                    attempt = rate_limit_retries,
                    delay_ms = retry_after.as_millis() as u64,
                    "upstream rate-limited; retrying"
                );
                tokio::time::sleep(retry_after).await;
                continue;
            }
            if status.is_server_error() && rate_limit_retries < MAX_UPSTREAM_RETRIES {
                rate_limit_retries += 1;
                let delay = retry_after_delay(resp.headers(), RETRY_AFTER_CAP);
                tracing::warn!(
                    attempt = rate_limit_retries,
                    delay_ms = delay.as_millis() as u64,
                    "upstream 5xx; retrying"
                );
                tokio::time::sleep(delay).await;
                continue;
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(UpstreamError::Status { status, body });
            }
            let headers = FORWARDED_HEADERS
                .iter()
                .filter_map(|name| {
                    resp.headers()
                        .get(*name)
                        .and_then(|v| v.to_str().ok())
                        .map(|v| (name.to_string(), v.to_string()))
                })
                .collect();
            return Ok(ResponsesStream {
                headers,
                body: Box::pin(resp.bytes_stream()),
            });
        }
        Err(anyhow::anyhow!(
            "upstream retries exhausted after {} attempts",
            1 + 1 + MAX_UPSTREAM_RETRIES
        )
        .into())
    }
}
