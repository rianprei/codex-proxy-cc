//! Shared test harness: a mock Codex backend, a temporary Codex home, and
//! helpers to drive the real router. No real credentials and no real inference
//! are used anywhere in the integration suite.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use claudecodex::server::AppState;
use tokio::net::TcpListener;

/// A fake Codex credentials file. Values are syntactically plausible but are
/// never sent anywhere real: the upstream is always the local mock.
pub fn write_fake_codex_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("claudecodex-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp codex home");
    let auth = serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": "test-access-token",
            "refresh_token": "test-refresh-token",
            "account_id": "test-account-id"
        },
        "last_refresh": "2026-01-01T00:00:00Z"
    });
    std::fs::write(dir.join("auth.json"), auth.to_string()).expect("write auth.json");
    dir
}

/// Recorded requests, so tests can assert on what the proxy actually sent.
#[derive(Default)]
pub struct Recorded {
    pub bodies: Mutex<Vec<String>>,
    pub auth_headers: Mutex<Vec<String>>,
    pub hits: AtomicU32,
}

#[derive(Clone)]
pub struct MockUpstream {
    pub recorded: Arc<Recorded>,
    /// SSE body the mock returns from POST /responses.
    pub sse: Arc<String>,
    /// Status returned by POST /responses.
    pub responses_status: Arc<Mutex<StatusCode>>,
    /// Fail the first N /responses calls with `fail_status` plus a
    /// `Retry-After` header, then behave normally. Drives the proxy's
    /// bounded retry path deterministically.
    pub fail_first: Arc<AtomicU32>,
    pub fail_status: StatusCode,
    pub retry_after: &'static str,
    /// When set, transient failures return this body instead of the default,
    /// so tests can shape quota-exhaustion responses.
    pub quota_body: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl MockUpstream {
    pub fn new(sse: &str) -> Self {
        Self {
            recorded: Arc::new(Recorded::default()),
            sse: Arc::new(sse.to_string()),
            responses_status: Arc::new(Mutex::new(StatusCode::OK)),
            fail_first: Arc::new(AtomicU32::new(0)),
            fail_status: StatusCode::TOO_MANY_REQUESTS,
            retry_after: "0",
            quota_body: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub fn with_status(self, status: StatusCode) -> Self {
        *self.responses_status.lock().unwrap() = status;
        self
    }

    /// Fail the first `n` calls with `status` plus `Retry-After: <retry_after>`.
    pub fn with_transient_failures(
        mut self,
        n: u32,
        status: StatusCode,
        retry_after: &'static str,
    ) -> Self {
        self.fail_first.store(n, Ordering::SeqCst);
        self.fail_status = status;
        self.retry_after = retry_after;
        self
    }

    pub fn hits(&self) -> u32 {
        self.recorded.hits.load(Ordering::SeqCst)
    }

    /// Calls that reached POST /responses only. `hits` also counts /models
    /// (catalog resolution), so retry-count assertions must use this.
    pub fn responses_calls(&self) -> usize {
        self.recorded.bodies.lock().unwrap().len()
    }

    pub fn last_body(&self) -> String {
        self.recorded
            .bodies
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }

    /// Start on an ephemeral loopback port. Returns the address and a handle.
    pub async fn spawn(self) -> (SocketAddr, Self, tokio::task::JoinHandle<()>) {
        let state = self.clone();
        let app = Router::new()
            .route("/models", get(mock_models))
            .route("/responses", post(mock_responses))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("mock addr");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, self, handle)
    }

    /// Point the real proxy at this mock.
    pub async fn state(&self, addr: SocketAddr, tag: &str, token: Option<String>) -> AppState {
        claudecodex::build_state(
            &format!("http://{addr}"),
            &write_fake_codex_home(tag),
            token,
            "medium",
        )
        .await
        .expect("build proxy state")
    }
}

/// Mock catalog: ALL FOUR main slugs plus a legacy one — hermetic coverage of
/// MAIN_SLUGS (fleet finding: 3 of 4 slugs were unasserted).
async fn mock_models(State(m): State<MockUpstream>) -> Response {
    m.recorded.hits.fetch_add(1, Ordering::SeqCst);
    let body = serde_json::json!({
        "models": [
            {"slug": "gpt-6-astra", "display_name": "GPT-6-Astra",
             "description": "Frontier",
             "supported_reasoning_levels": [
                {"effort":"low","description":"fast"},
                {"effort":"medium","description":"balanced"},
                {"effort":"high","description":"thorough"},
                {"effort":"xhigh","description":"maximum"}
             ]},
            {"slug": "gpt-5.6-sol", "display_name": "GPT-5.6-Sol",
             "supported_reasoning_levels": [
                {"effort":"low","description":"fast"},
                {"effort":"medium","description":"balanced"},
                {"effort":"high","description":"thorough"}
             ]},
            {"slug": "gpt-5.6-terra", "display_name": "GPT-5.6-Terra",
             "supported_reasoning_levels": [
                {"effort":"low","description":"fast"},
                {"effort":"high","description":"thorough"}
             ]},
            {"slug": "gpt-5.6-luna", "display_name": "GPT-5.6-Luna",
             "supported_reasoning_levels": [
                {"effort":"low","description":"fast"},
                {"effort":"medium","description":"balanced"}
             ]},
            {"slug": "gpt-5.5", "display_name": "GPT-5.5",
             "description": "Legacy coding model.",
             "supported_reasoning_levels": [
                {"effort":"low","description":"fast"},
                {"effort":"high","description":"thorough"}
             ]}
        ]
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn mock_responses(
    State(m): State<MockUpstream>,
    headers: HeaderMap,
    body: String,
) -> Response {
    m.recorded.hits.fetch_add(1, Ordering::SeqCst);
    m.recorded.bodies.lock().unwrap().push(body);
    if let Some(a) = headers.get("authorization") {
        m.recorded
            .auth_headers
            .lock()
            .unwrap()
            .push(a.to_str().unwrap_or("<binary>").to_string());
    }
    // Consume the transient-failure budget before applying the steady status.
    if m.fail_first.load(Ordering::SeqCst) > 0 {
        m.fail_first.fetch_sub(1, Ordering::SeqCst);
        let body = m
            .quota_body
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| r#"{"error":{"message":"transient"}}"#.to_string());
        return Response::builder()
            .status(m.fail_status)
            .header("retry-after", m.retry_after)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
    }
    let status = *m.responses_status.lock().unwrap();
    let sse = m.sse.clone();
    Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .body(Body::from(sse.as_bytes().to_vec()))
        .unwrap()
}

/// Collect an SSE body into a Vec of `event:`/`data:` pairs for assertions.
pub fn parse_sse(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut ev = String::new();
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("event: ") {
            ev = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data: ") {
            out.push((ev.clone(), rest.trim().to_string()));
            ev.clear();
        }
    }
    out
}
