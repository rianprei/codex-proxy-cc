//! axum HTTP surface: the Anthropic Messages API as Claude Code speaks it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::anthropic::{ApiError, ErrorBody, MessagesRequest, StreamEvent};
use crate::models::Catalog;
use crate::translate::{self, ResponsesToAnthropic, SseParser};
use crate::upstream::{ResponsesRequest, ResponsesStream, Upstream, UpstreamError};

/// Interval between SSE `ping` frames while the upstream is quiet.
/// 5s (ported from claude-nim): shorter pings sit further inside Claude
/// Code's streaming idle watchdog, so long silent reasoning phases never
/// trip it on slow upstreams.
const PING_INTERVAL: Duration = Duration::from_secs(5);
/// Give up on an upstream stream that has sent nothing for this long.
const MAX_STREAM_IDLE: Duration = Duration::from_secs(600);
/// How long a chain candidate that failed with a 429/5xx is skipped by the
/// circuit breaker (ported from claude-nim: one 60s window, per slug).
const CHAIN_COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct AppState {
    pub auth_token: Option<String>,
    pub upstream: Arc<Upstream>,
    pub catalog: Arc<Catalog>,
    pub default_effort: String,
    /// Opt-in same-provider fallback chain: Codex slugs in preference order,
    /// parsed once at startup from `CLAUDE_CODEX_CHAIN`. EMPTY = the feature
    /// is off and behavior must be byte-identical to the no-chain build.
    /// The chain only ever runs after the selected model failed with a
    /// transient upstream error — it is strictly same-provider and
    /// same-account, never a paid API or another provider.
    pub chain: Arc<Vec<String>>,
    /// Circuit breaker state for chain candidates, keyed by bare Codex slug:
    /// slug -> instant until which the slug is skipped. Only CANDIDATES are
    /// keyed here; the user's selected model never enters cooldown (its
    /// failure either surfaces or flows into the chain).
    pub chain_cooldowns: Arc<Mutex<HashMap<String, Instant>>>,
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .route("/api/hello", get(hello).head(hello))
        .merge(api)
        .layer(middleware::from_fn(log_request))
        .with_state(state)
}

fn error_response(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    (status, Json(ErrorBody::new(kind, message))).into_response()
}

/// Error response carrying a `retry-after` the client can act on.
///
/// Per the Claude Code gateway contract, `retry-after` is integer seconds and a
/// value above 60 stops the client's automatic retries and surfaces the error
/// immediately. Quota exhaustion ("usage limit") will not recover inside any
/// window the client would retry in, so it gets a terminal value; a transient
/// 429 that survived the proxy's own bounded retries forwards the upstream's
/// own header when present, or a short pause otherwise.
fn error_response_with_retry(
    status: StatusCode,
    kind: &str,
    message: impl Into<String>,
    retry_after_secs: u64,
) -> Response {
    let mut res = error_response(status, kind, message);
    if let Ok(v) = HeaderValue::from_str(&retry_after_secs.to_string()) {
        res.headers_mut().insert("retry-after", v);
    }
    res
}

async fn log_request(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = std::time::Instant::now();
    let res = next.run(req).await;
    tracing::info!(
        %method,
        %path,
        status = res.status().as_u16(),
        ms = started.elapsed().as_millis() as u64,
        "request"
    );
    res
}

async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.auth_token.as_deref() else {
        return next.run(req).await;
    };
    let headers = req.headers();
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(str::trim);
    let api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    if bearer == Some(expected) || api_key == Some(expected) {
        next.run(req).await
    } else {
        error_response(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid or missing auth token",
        )
    }
}

async fn hello() -> StatusCode {
    StatusCode::OK
}

async fn list_models(State(state): State<AppState>) -> Response {
    let models = state.catalog.public_models().await;
    let first_id = models.first().map(|m| m.id.clone());
    let last_id = models.last().map(|m| m.id.clone());
    Json(json!({
        "data": models,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id,
    }))
    .into_response()
}

async fn count_tokens(body: axum::body::Bytes) -> Response {
    // Rough estimate: the upstream exposes no token counter for the Codex backend.
    // Counts raw request bytes — re-serializing the parsed JSON would cost an
    // extra O(body) allocation on an endpoint Claude Code calls frequently.
    let chars = body.len() as u64;
    Json(json!({ "input_tokens": chars.div_ceil(4) })).into_response()
}

fn upstream_error_response(err: UpstreamError) -> Response {
    match err {
        UpstreamError::Status { status, body } => {
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| {
                    v.pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| body.chars().take(2000).collect());
            let kind = match status {
                StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "authentication_error",
                StatusCode::BAD_REQUEST => "invalid_request_error",
                s if s.is_server_error() => "api_error",
                _ => "api_error",
            };
            // Quota exhaustion is terminal for any window the client retries
            // in: tell it to stop (>60s per the gateway contract) so the user
            // sees the error at once instead of a silent retry loop.
            if status == StatusCode::TOO_MANY_REQUESTS
                && crate::upstream::is_quota_exhaustion(&body)
            {
                return error_response_with_retry(status, kind, message, 3600);
            }
            error_response(status, kind, message)
        }
        UpstreamError::Other(e) => error_response(
            StatusCode::BAD_GATEWAY,
            "api_error",
            format!("upstream: {e:#}"),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Opt-in same-provider chain fallback (CLAUDE_CODEX_CHAIN, ported from the
// claude-nim gateway design: chains like ultra→super→nano with a 60s
// per-slug cooldown). The hard contract is untouched: the proxy NEVER falls
// back to a paid API or another provider — every candidate is a Codex slug
// routed through the same upstream with the same account. An EMPTY chain
// (env unset/empty) disables the feature entirely.
// ---------------------------------------------------------------------------------------------

/// Is `slug` inside its circuit-breaker window?
fn in_cooldown(cooldowns: &Mutex<HashMap<String, Instant>>, slug: &str) -> bool {
    cooldowns
        .lock()
        // Poison recovery is always safe here: the map only holds
        // cooldown timestamps. A panicked peer must never wedge the
        // proxy for every later request.
        .unwrap_or_else(|p| p.into_inner())
        .get(slug)
        .is_some_and(|until| Instant::now() < *until)
}

/// Open (or refresh) the circuit-breaker window for `slug`.
fn set_cooldown(cooldowns: &Mutex<HashMap<String, Instant>>, slug: &str) {
    cooldowns
        .lock()
        // Poison recovery is always safe here: the map only holds
        // cooldown timestamps. A panicked peer must never wedge the
        // proxy for every later request.
        .unwrap_or_else(|p| p.into_inner())
        .insert(slug.to_string(), Instant::now() + CHAIN_COOLDOWN);
}

/// Compact, journal-safe description of a failed attempt for the loud
/// switch log: the HTTP status for upstream errors, the transport error
/// otherwise. Never the body (bodies can be large and are not redacted).
fn failure_brief(err: &UpstreamError) -> String {
    match err {
        UpstreamError::Status { status, .. } => status.as_u16().to_string(),
        UpstreamError::Other(e) => format!("transport: {e}"),
    }
}

/// Resolve effort for `slug`, build the upstream request and emit the
/// per-attempt journal lines. Runs once for the selected model and once for
/// every chain candidate, each time with the slug actually used, so the
/// journal shows the real chain of attempts.
///
/// `id_suffix_effort` is the `:<effort>` the client put on the requested
/// model id. Chain candidates are bare slugs and pass None: their effort
/// comes from the request-level channels (`output_config.effort` /
/// `thinking`) or the CLI default, clamped to what the CANDIDATE declares —
/// never the selected model's suffix, which may not exist there.
async fn build_attempt(
    state: &AppState,
    req: &MessagesRequest,
    request_effort: Option<&str>,
    session_id: Option<&str>,
    slug: &str,
    id_suffix_effort: Option<&str>,
) -> ResponsesRequest {
    let upstream_model = state.catalog.model(slug).await;
    let effort = translate::choose_effort(
        request_effort,
        id_suffix_effort,
        req.thinking.as_ref(),
        &state.default_effort,
        upstream_model.as_ref(),
    );
    let upstream_req = translate::anthropic_to_responses(req, slug, &effort, session_id);
    // Diagnostic chain (redacted: ids and effort levels only, never prompts,
    // tokens or credentials): selected id -> backend slug -> effort the client
    // sent (with its source) -> effort forwarded upstream. The raw client
    // string is sanitized: only declared effort levels are printable, so a
    // hostile output_config can never forge or flood the log.
    let effort_source = if request_effort.is_some() {
        "request"
    } else if id_suffix_effort.is_some() {
        "id_suffix"
    } else if req
        .thinking
        .as_ref()
        .map(|t| t.kind == "enabled" && t.budget_tokens.is_some())
        .unwrap_or(false)
    {
        "thinking"
    } else {
        "default"
    };
    let effort_received = request_effort.or(id_suffix_effort).unwrap_or("-");
    let printable = if effort_received == "-" || translate::is_known_effort(effort_received) {
        effort_received
    } else {
        "<invalid>"
    };
    tracing::info!(
        selected_model = %req.model,
        backend_slug = %slug,
        effort_received = %format!("{effort_source}:{printable}"),
        effort_sent = %effort,
        "model resolution"
    );
    tracing::debug!(
        model = %slug,
        %effort,
        stream = req.stream,
        tools = req.tools.len(),
        input_items = upstream_req.input.len(),
        "forwarding to codex"
    );
    upstream_req
}

/// Outcome of the post-failure chain walk.
enum ChainOutcome {
    /// A chain candidate answered; the response streams from it.
    Stream(ResponsesStream),
    /// Nothing left to try (chain off, non-trigger error, quota, or chain
    /// exhausted) — the terminal error response for the client.
    Terminal(Response),
}

/// Try the fallback chain after the selected model failed with `first_err`.
///
/// Guarantees (opt-in feature; EMPTY chain = all of this is skipped and the
/// error surfaces exactly as before the feature existed):
///
/// - Trigger: `UpstreamError::Status` with 429/5xx that is NOT quota
///   exhaustion. Quota means the ACCOUNT is out — every model would fail
///   identically — so the terminal error surfaces immediately on ANY attempt,
///   selected or candidate, and the walk stops.
/// - Selection: chain order, starting AFTER the selected slug. A selected
///   slug not in the chain does not consume the walk. Every candidate must
///   resolve in the catalog; a bad entry (operator typo in the env var) is
///   skipped with a loud warn, never silently.
/// - Circuit breaker: a candidate that failed with 429/5xx is skipped for
///   CHAIN_COOLDOWN on subsequent requests. The selected model is never
///   keyed — it is always attempted first.
/// - Loudness: every switch names the original slug, the failing status,
///   the new slug and the remaining candidates; exhaustion is logged too.
/// - No looping: each request walks the chain at most once and surfaces the
///   LAST error through the existing error mapping.
async fn run_chain(
    state: &AppState,
    req: &MessagesRequest,
    request_effort: Option<&str>,
    session_id: Option<&str>,
    selected_slug: &str,
    first_err: UpstreamError,
) -> ChainOutcome {
    // Trigger guard: only a transient upstream Status (429/5xx) starts the
    // walk — never quota exhaustion, never other transport/4xx errors.
    let triggers = matches!(
        &first_err,
        UpstreamError::Status { status, body }
            if (*status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                && !crate::upstream::is_quota_exhaustion(body)
    );
    if !triggers || state.chain.is_empty() {
        return ChainOutcome::Terminal(upstream_error_response(first_err));
    }
    // Candidates: chain entries AFTER the selected slug (which was just
    // attempted). A selected slug not in the chain leaves the walk intact
    // and the chain applies from the top.
    let start = state
        .chain
        .iter()
        .position(|s| s == selected_slug)
        .map_or(0, |i| i + 1);
    let candidates = &state.chain[start..];

    let mut last_err = first_err;
    for (i, slug) in candidates.iter().enumerate() {
        // Circuit breaker: skip a candidate that failed inside the window —
        // it would fail the same way again.
        if in_cooldown(&state.chain_cooldowns, slug) {
            tracing::warn!(slug, "chain candidate in cooldown; skipping");
            continue;
        }
        // Validity: the candidate must exist in the catalog (same upstream,
        // same account). A bad entry is an operator config mistake — loud,
        // then skipped.
        if state
            .catalog
            .resolve(&format!("claude-{slug}"))
            .await
            .is_none()
        {
            tracing::warn!(slug, "chain candidate not in catalog; skipping");
            continue;
        }
        // LOUD switch record: what was asked, what failed and how, what we
        // switch to, what is left. Never silent.
        let remaining = if i + 1 < candidates.len() {
            candidates[i + 1..].join(",")
        } else {
            "-".to_string()
        };
        tracing::warn!(
            original_slug = %selected_slug,
            failing_status = %failure_brief(&last_err),
            new_slug = %slug,
            remaining_candidates = %remaining,
            "chain fallback: switching model"
        );
        let upstream_req = build_attempt(state, req, request_effort, session_id, slug, None).await;
        match state.upstream.responses(&upstream_req).await {
            Ok(s) => return ChainOutcome::Stream(s),
            Err(e) => {
                let chain_failure = matches!(
                    &e,
                    UpstreamError::Status { status, .. }
                        if *status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
                );
                // Symmetric with the trigger guard above: quota is quota on
                // any status that carries its body (429 in practice, 5xx
                // defensively) — the walk must stop identically everywhere.
                let quota = matches!(
                    &e,
                    UpstreamError::Status { status, body }
                        if (*status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                            && crate::upstream::is_quota_exhaustion(body)
                );
                if chain_failure {
                    // Circuit breaker: this slug just failed 429/5xx.
                    set_cooldown(&state.chain_cooldowns, slug);
                }
                if quota {
                    // Account-level terminal state on ANY attempt: stop the
                    // walk at once and surface the terminal quota error.
                    return ChainOutcome::Terminal(upstream_error_response(e));
                }
                last_err = e;
            }
        }
    }
    tracing::warn!(
        original_slug = %selected_slug,
        chain = %state.chain.join(","),
        "chain exhausted; surfacing last upstream error"
    );
    ChainOutcome::Terminal(upstream_error_response(last_err))
}

/// Content block types this proxy can faithfully translate to the Codex
/// Responses API. Anything else must fail loudly, not silently vanish.
const SUPPORTED_BLOCKS: &[&str] = &[
    "text",
    "image",
    "tool_use",
    "tool_result",
    "thinking",
    "redacted_thinking",
];

/// Walks every content array in the raw request and rejects block types the
/// translator does not support, naming the offending type so the client can
/// act on it. Runs before typed deserialization because `#[serde(other)]`
/// maps unknown blocks to `Unknown` and discards the tag.
///
/// Two shapes carry blocks: `messages[*].content[*]` and a `system` that is
/// itself an array of blocks (system blocks have no `content` wrapper).
fn validate_content_blocks(value: &Value) -> Result<(), String> {
    let reject = |kind: &str| -> String {
        format!(
            "unsupported content block type '{kind}': this proxy translates only \
             [{}]. The Codex backend for ChatGPT accounts accepts text and image \
             inputs, so '{kind}' cannot be forwarded; remove or convert it before \
             retrying.",
            SUPPORTED_BLOCKS.join(", ")
        )
    };

    if let Some(blocks) = value.get("system").and_then(|s| s.as_array()) {
        for block in blocks {
            if let Some(kind) = block.get("type").and_then(|t| t.as_str()) {
                if !SUPPORTED_BLOCKS.contains(&kind) {
                    return Err(reject(kind));
                }
            }
        }
    }

    // Nested blocks: a tool_result carries its own content array that the
    // translator walks too. IMPORTANT (cycle-3 #16): at the NESTED position
    // the translator only implements Text and Image — every other name, even
    // a "supported" one like thinking, is silently dropped by tool_result_text.
    // Reject by name here so nothing supported-by-name-but-untranslatable
    // passes. Also recurse into nested tool_results.
    const NESTED_SUPPORTED: &[&str] = &["text", "image"];
    fn check_blocks(blocks: &[Value], reject: &dyn Fn(&str) -> String) -> Result<(), String> {
        // Top level: full SUPPORTED_BLOCKS (tool_use/tool_result/thinking are
        // translatable here). Only the DESCENT into tool_result.content uses
        // the restricted set — that is where the translator drops silently.
        check_blocks_inner(blocks, reject, SUPPORTED_BLOCKS)
    }
    fn check_blocks_inner(
        blocks: &[Value],
        reject: &dyn Fn(&str) -> String,
        allowed: &[&str],
    ) -> Result<(), String> {
        for block in blocks {
            if let Some(kind) = block.get("type").and_then(|t| t.as_str()) {
                if !allowed.contains(&kind) {
                    return Err(reject(kind));
                }
            }
            if kind_is_tool_result(block) {
                if let Some(nested) = block.get("content").and_then(|c| c.as_array()) {
                    check_blocks_inner(nested, reject, NESTED_SUPPORTED)?;
                }
            }
        }
        Ok(())
    }
    fn kind_is_tool_result(block: &Value) -> bool {
        block.get("type").and_then(|t| t.as_str()) == Some("tool_result")
    }

    if let Some(messages) = value.get("messages").and_then(|m| m.as_array()) {
        for message in messages {
            if let Some(blocks) = message.get("content").and_then(|c| c.as_array()) {
                check_blocks(blocks, &reject)?;
            }
            // tool_result blocks can also appear flattened here when content
            // is a single-block array; check_blocks covers the nesting.
        }
    }
    Ok(())
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Single parse of the body (fleet finding: it was deserialized twice per
    // request). The Value feeds both block validation and, via from_value,
    // the typed request — one byte-level parse instead of two.
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("malformed request body: {e}"),
            )
        }
    };
    // Reject unsupported content blocks explicitly instead of letting the
    // translator drop them: a silently discarded block changes what the model
    // sees relative to what the client sent.
    if let Err(message) = validate_content_blocks(&value) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_request_error", message);
    }
    let req: MessagesRequest = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => {
            // Cycle-3 #13: from_value errors carry no line/column. Re-derive
            // the positioned message with a second pass — error path only, so
            // the happy path keeps its single-parse win.
            let positioned = serde_json::from_slice::<MessagesRequest>(&body)
                .err()
                .map(|e2| e2.to_string())
                .unwrap_or_else(|| e.to_string());
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("malformed request body: {positioned}"),
            );
        }
    };

    let Some(resolved) = state.catalog.resolve(&req.model).await else {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found_error",
            format!("model not found: {}", req.model),
        );
    };
    let request_effort = req
        .output_config
        .as_ref()
        .and_then(|o| o.effort.as_deref())
        .filter(|e| !e.is_empty());
    let session_id = header_str(&headers, "x-claude-code-session-id");
    // Attempt 1 is always the model the client selected — the chain (if
    // enabled) only ever runs AFTER this fails with a transient error.
    let upstream_req = build_attempt(
        &state,
        &req,
        request_effort,
        session_id,
        &resolved.slug,
        resolved.effort.as_deref(),
    )
    .await;
    let stream = match state.upstream.responses(&upstream_req).await {
        Ok(s) => s,
        Err(e) => {
            match run_chain(&state, &req, request_effort, session_id, &resolved.slug, e).await {
                ChainOutcome::Stream(s) => s,
                ChainOutcome::Terminal(resp) => return resp,
            }
        }
    };
    let mut extra_headers = Vec::new();
    for (name, value) in &stream.headers {
        if let (Ok(n), Ok(v)) = (
            header::HeaderName::from_bytes(name.as_bytes()),
            header::HeaderValue::from_str(value),
        ) {
            extra_headers.push((n, v));
        }
    }

    let public_model = req.model.clone();
    if req.stream {
        let (tx, rx) = mpsc::channel::<String>(64);
        tokio::spawn(pump_stream(stream.body, public_model, tx));
        let body =
            Body::from_stream(ReceiverStream::new(rx).map(Ok::<_, std::convert::Infallible>));
        let mut res = Response::new(body);
        let h = res.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            "text/event-stream".parse().expect("static"),
        );
        h.insert(header::CACHE_CONTROL, "no-cache".parse().expect("static"));
        for (n, v) in extra_headers {
            h.insert(n, v);
        }
        res
    } else {
        let (events, _idle_aborted) = collect_events(stream.body, public_model.clone()).await;
        // On idle abort the last event is an explicit api_error: assemble()
        // returns Err and the client gets a real error status, never a
        // truncated 200.
        match translate::assemble(public_model, &events) {
            Ok(msg) => {
                let mut res = Json(msg).into_response();
                for (n, v) in extra_headers {
                    res.headers_mut().insert(n, v);
                }
                res
            }
            Err(ApiError { kind, message }) => {
                let status = match kind.as_str() {
                    "rate_limit_error" => StatusCode::TOO_MANY_REQUESTS,
                    "invalid_request_error" => StatusCode::BAD_REQUEST,
                    _ => StatusCode::BAD_GATEWAY,
                };
                error_response(status, &kind, message)
            }
        }
    }
}

type UpstreamBody =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

/// Drives the upstream SSE body through the translator, writing Anthropic SSE frames to `tx`
/// and interleaving `ping` frames while the upstream is silent.
async fn pump_stream(mut body: UpstreamBody, model: String, tx: mpsc::Sender<String>) {
    let mut parser = SseParser::default();
    let mut translator = ResponsesToAnthropic::new(model);
    let ping = StreamEvent::Ping.to_sse();
    let mut last_activity = Instant::now();
    loop {
        let next = tokio::time::timeout(PING_INTERVAL, body.next()).await;
        match next {
            Err(_elapsed) => {
                if last_activity.elapsed() >= MAX_STREAM_IDLE {
                    tracing::warn!("upstream stream idle for {MAX_STREAM_IDLE:?}; aborting");
                    let ev = StreamEvent::Error {
                        error: ApiError {
                            kind: "api_error".into(),
                            message: "upstream stream idle timeout".into(),
                        },
                    };
                    let _ = tx.send(ev.to_sse()).await;
                    return;
                }
                if tx.send(ping.clone()).await.is_err() {
                    return;
                }
            }
            Ok(Some(Ok(chunk))) => {
                last_activity = Instant::now();
                for frame in parser.push(&chunk) {
                    let Ok(value) = serde_json::from_str::<Value>(&frame.data) else {
                        tracing::debug!(event = ?frame.event, "non-json sse frame ignored");
                        continue;
                    };
                    for ev in translator.handle(&value) {
                        if tx.send(ev.to_sse()).await.is_err() {
                            return;
                        }
                    }
                }
                if translator.finished() {
                    return;
                }
            }
            Ok(Some(Err(e))) => {
                tracing::warn!(error = %e, "upstream stream error");
                let ev = StreamEvent::Error {
                    error: ApiError {
                        kind: "api_error".into(),
                        message: format!("upstream stream error: {e}"),
                    },
                };
                let _ = tx.send(ev.to_sse()).await;
                return;
            }
            Ok(None) => {
                for ev in translator.end_of_stream() {
                    if tx.send(ev.to_sse()).await.is_err() {
                        return;
                    }
                }
                return;
            }
        }
    }
}

async fn collect_events(mut body: UpstreamBody, model: String) -> (Vec<StreamEvent>, bool) {
    let mut parser = SseParser::default();
    let mut translator = ResponsesToAnthropic::new(model);
    let mut events = Vec::new();
    // Same idle guard as the streaming path (fleet finding: a wedged upstream
    // sending headers but no bytes would hang the non-streaming request
    // forever — reqwest only has a connect timeout).
    let mut last_activity = Instant::now();
    let mut idle_aborted = false;
    loop {
        let next = match tokio::time::timeout(PING_INTERVAL, body.next()).await {
            Ok(next) => next,
            Err(_elapsed) => {
                if last_activity.elapsed() >= MAX_STREAM_IDLE {
                    tracing::warn!("non-streaming upstream idle for {MAX_STREAM_IDLE:?}; aborting");
                    idle_aborted = true;
                    break;
                }
                continue;
            }
        };
        let Some(chunk) = next else { break };
        match chunk {
            Ok(chunk) => {
                last_activity = Instant::now();
                for frame in parser.push(&chunk) {
                    if let Ok(value) = serde_json::from_str::<Value>(&frame.data) {
                        events.extend(translator.handle(&value));
                    }
                }
                if translator.finished() {
                    return (events, false);
                }
            }
            Err(e) => {
                events.push(StreamEvent::Error {
                    error: ApiError {
                        kind: "api_error".into(),
                        message: format!("upstream stream error: {e}"),
                    },
                });
                return (events, false);
            }
        }
    }
    // Fleet cycle-3 #4: an idle-aborted stream must NOT fabricate a normal
    // finish (end_of_stream synthesizes end_turn + MessageStop from partial
    // events, yielding a 200 with truncated content). Error out instead —
    // same contract as the streaming path.
    if idle_aborted {
        events.push(StreamEvent::Error {
            error: ApiError {
                kind: "api_error".into(),
                message: "upstream stream idle timeout".into(),
            },
        });
        return (events, true);
    }
    events.extend(translator.end_of_stream());
    (events, false)
}
