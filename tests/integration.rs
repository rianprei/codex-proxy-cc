//! Integration tests for the Anthropic surface, driven entirely against a mock
//! Codex upstream. Real inference is covered separately by live acceptance.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{parse_sse, MockUpstream};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// A well-formed Codex Responses SSE stream with text and a tool call.
fn good_sse() -> String {
    [
        r#"event: response.created"#,
        r#"data: {"type":"response.created","response":{"id":"resp_1"}}"#,
        "",
        r#"event: response.output_item.added"#,
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant"}}"#,
        "",
        r#"event: response.output_text.delta"#,
        r#"data: {"type":"response.output_text.delta","delta":"Hello"}"#,
        "",
        r#"event: response.output_text.delta"#,
        r#"data: {"type":"response.output_text.delta","delta":" world"}"#,
        "",
        r#"event: response.output_item.added"#,
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"Read","arguments":""}}"#,
        "",
        r#"event: response.function_call_arguments.delta"#,
        r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"file_path\":"}"#,
        "",
        r#"event: response.function_call_arguments.delta"#,
        r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"\"/etc/hostname\"}"}"#,
        "",
        r#"event: response.function_call_arguments.done"#,
        r#"data: {"type":"response.function_call_arguments.done","output_index":1,"arguments":"{\"file_path\":\"/etc/hostname\"}"}"#,
        "",
        r#"event: response.output_text.done"#,
        r#"data: {"type":"response.output_text.done","text":"Hello world"}"#,
        "",
        r#"event: response.completed"#,
        r#"data: {"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":11,"output_tokens":7}}}"#,
        "",
        "",
    ]
    .join("\n")
}

async fn post_messages(
    state: &claudecodex::server::AppState,
    token: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = req.body(Body::from(body.to_string())).unwrap();
    let resp = claudecodex::server::router(state.clone())
        .oneshot(req)
        .await
        .expect("router call");
    let status = resp.status();
    let raw = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&raw).to_string())
}

fn simple_request() -> serde_json::Value {
    serde_json::json!({
        "model": "claude-gpt-6-astra:xhigh",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role":"user","content":"hi"}]
    })
}

// ---------------------------------------------------------------------------
// Security
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rejects_request_without_auth_token() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "noauth", Some("secret".into())).await;

    let (status, _) = post_messages(&state, None, simple_request()).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "must reject unauthenticated"
    );
    assert_eq!(mock.hits(), 0, "must not reach upstream when auth fails");
}

#[tokio::test]
async fn rejects_wrong_auth_token() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "badauth", Some("secret".into())).await;

    let (status, _) = post_messages(&state, Some("wrong"), simple_request()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(mock.hits(), 0, "must not reach upstream with a bad token");
}

#[tokio::test]
async fn accepts_correct_auth_token() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "okauth", Some("secret".into())).await;

    let (status, _) = post_messages(&state, Some("secret"), simple_request()).await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Streaming state machine
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_event_order_and_tool_assembly() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "stream", None).await;

    let (status, raw) = post_messages(&state, None, simple_request()).await;
    assert_eq!(status, StatusCode::OK);

    let events = parse_sse(&raw);
    let names: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();

    assert_eq!(
        names.first(),
        Some(&"message_start"),
        "must start with message_start"
    );
    assert_eq!(
        names.last(),
        Some(&"message_stop"),
        "must end with message_stop"
    );

    // Indexes must be monotonic.
    let mut indexes: Vec<i64> = Vec::new();
    for (_, data) in &events {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
            if let Some(i) = v.get("index").and_then(|x| x.as_i64()) {
                indexes.push(i);
            }
        }
    }
    assert!(
        indexes.windows(2).all(|w| w[1] >= w[0]),
        "indexes must be monotonic: {indexes:?}"
    );

    // Text must be delivered incrementally, in two deltas.
    let text: String = events
        .iter()
        .filter(|(_, d)| d.contains("\"text_delta\""))
        .filter_map(|(_, d)| serde_json::from_str::<serde_json::Value>(d).ok())
        .filter_map(|v| {
            v.pointer("/delta/text")
                .and_then(|t| t.as_str())
                .map(str::to_string)
        })
        .collect();
    assert_eq!(text, "Hello world", "text deltas must stream incrementally");

    // Tool arguments must assemble into valid JSON.
    let args: String = events
        .iter()
        .filter(|(_, d)| d.contains("input_json_delta"))
        .filter_map(|(_, d)| serde_json::from_str::<serde_json::Value>(d).ok())
        .filter_map(|v| {
            v.pointer("/delta/partial_json")
                .and_then(|t| t.as_str())
                .map(str::to_string)
        })
        .collect();
    let parsed: serde_json::Value =
        serde_json::from_str(&args).expect("tool args must be valid JSON");
    assert_eq!(parsed["file_path"], "/etc/hostname");
}

#[tokio::test]
async fn malformed_upstream_stream_does_not_hang() {
    let broken = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n"
    );
    let (addr, _mock, _h) = MockUpstream::new(broken).spawn().await;
    let state = _mock.state(addr, "trunc", None).await;

    let res = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        post_messages(&state, None, simple_request()),
    )
    .await
    .expect("must not hang on a truncated stream");

    let (_, raw) = res;
    assert!(
        raw.contains("message_stop") || raw.contains("error"),
        "truncated stream must terminate with message_stop or an error"
    );
}

#[tokio::test]
async fn upstream_error_is_mapped_to_anthropic_error() {
    let (addr, _mock, _h) = MockUpstream::new(&good_sse())
        .with_status(StatusCode::TOO_MANY_REQUESTS)
        .spawn()
        .await;
    let state = _mock.state(addr, "ratelimit", None).await;

    let (status, raw) = post_messages(&state, None, simple_request()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "rate_limit_error");
}

/// A 403 from upstream must surface as an authentication error, not a model error.
#[tokio::test]
async fn upstream_403_is_auth_error_not_model_error() {
    let (addr, _mock, _h) = MockUpstream::new(&good_sse())
        .with_status(StatusCode::FORBIDDEN)
        .spawn()
        .await;
    let state = _mock.state(addr, "up403", None).await;

    let (status, raw) = post_messages(&state, None, simple_request()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    assert_eq!(
        v["error"]["type"], "authentication_error",
        "403 must not be reported as a model error; body was {raw}"
    );
}

/// A 400 from upstream must surface as invalid_request_error.
#[tokio::test]
async fn upstream_400_is_invalid_request_error() {
    let (addr, _mock, _h) = MockUpstream::new(&good_sse())
        .with_status(StatusCode::BAD_REQUEST)
        .spawn()
        .await;
    let state = _mock.state(addr, "up400", None).await;

    let (status, raw) = post_messages(&state, None, simple_request()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    assert_eq!(
        v["error"]["type"], "invalid_request_error",
        "body was {raw}"
    );
}

// ---------------------------------------------------------------------------
// Model discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn catalog_is_exactly_the_main_base_models_once_each() {
    let (addr, _mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = _mock.state(addr, "discovery", None).await;

    let req = Request::builder()
        .uri("/v1/models")
        .body(Body::empty())
        .unwrap();
    let resp = claudecodex::server::router(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let raw = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();

    let mut ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap_or_default())
        .collect();
    ids.sort();
    // Hermetic: ALL FOUR main slugs asserted (fleet finding: only astra was).
    assert_eq!(
        ids,
        vec![
            "claude-gpt-5.6-luna",
            "claude-gpt-5.6-sol",
            "claude-gpt-5.6-terra",
            "claude-gpt-6-astra"
        ],
        "catalog must be exactly the 4 main base models"
    );
    // No effort variants over HTTP (fleet finding: never asserted).
    assert!(
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| !m["id"].as_str().unwrap().contains(':')),
        "no ':effort' ids in the HTTP catalog"
    );
    assert!(
        ids.iter().all(|i| i.starts_with("claude-")),
        "all ids need the claude- prefix for gateway discovery: {ids:?}"
    );
    // Legacy must NOT be advertised...
    assert!(
        !ids.contains(&"claude-gpt-5.5"),
        "legacy model must not be advertised"
    );
    // ...but MUST still resolve end-to-end (fleet finding: untested over HTTP).
    let (status, _) = post_messages(
        &state,
        None,
        serde_json::json!({
            "model": "claude-gpt-5.5",
            "max_tokens": 32,
            "stream": true,
            "messages": [{"role":"user","content":"hi"}]
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "legacy slug must still resolve via POST"
    );
}

#[tokio::test]
async fn effort_suffix_maps_to_reasoning_effort() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "effort", None).await;

    let mut req = simple_request();
    req["model"] = serde_json::json!("claude-gpt-6-astra:xhigh");
    let (status, _) = post_messages(&state, None, req).await;
    assert_eq!(status, StatusCode::OK);

    let sent = mock.last_body();
    assert!(
        sent.contains("\"gpt-6-astra\""),
        "slug must reach upstream: {sent}"
    );
    assert!(
        sent.contains("\"xhigh\""),
        "effort suffix must become reasoning effort: {sent}"
    );
}

/// The NATIVE effort channel (output_config.effort) must flow end-to-end:
/// request body -> server extraction -> choose_effort -> upstream reasoning
/// effort. Fleet finding: the wiring had ZERO end-to-end coverage.
#[tokio::test]
async fn native_output_config_effort_flows_end_to_end() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "native-effort", None).await;

    let req = serde_json::json!({
        "model": "claude-gpt-6-astra",
        "max_tokens": 64,
        "stream": true,
        "output_config": {"effort": "high"},
        "messages": [{"role":"user","content":"hi"}]
    });
    let (status, raw) = post_messages(&state, None, req).await;
    assert_eq!(status, StatusCode::OK, "body was {raw}");

    let sent = mock.last_body();
    assert!(
        sent.contains("\"high\""),
        "native effort must reach the upstream reasoning effort: {sent}"
    );
}

/// Native effort WINS over the id suffix — changing effort must never require
/// switching model ids.
#[tokio::test]
async fn native_effort_wins_over_id_suffix() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "effort-precedence", None).await;

    let req = serde_json::json!({
        "model": "claude-gpt-6-astra:low",
        "max_tokens": 64,
        "stream": true,
        "output_config": {"effort": "high"},
        "messages": [{"role":"user","content":"hi"}]
    });
    let (status, _) = post_messages(&state, None, req).await;
    assert_eq!(status, StatusCode::OK);

    let sent = mock.last_body();
    assert!(sent.contains("\"high\""), "native effort must win: {sent}");
    assert!(!sent.contains("\"low\""), "suffix must not leak: {sent}");
}

/// Effort change must not change the base model.
#[tokio::test]
async fn effort_change_never_changes_base_model() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "effort-model", None).await;

    for effort in ["low", "medium", "high"] {
        let req = serde_json::json!({
            "model": "claude-gpt-6-astra",
            "max_tokens": 64,
            "stream": true,
            "output_config": {"effort": effort},
            "messages": [{"role":"user","content":"hi"}]
        });
        let (status, _) = post_messages(&state, None, req).await;
        assert_eq!(status, StatusCode::OK);
        let sent = mock.last_body();
        assert!(
            sent.contains("\"gpt-6-astra\""),
            "base model changed with effort {effort}: {sent}"
        );
        assert!(
            sent.contains(&format!("\"{effort}\"")),
            "effort {effort} lost: {sent}"
        );
    }
}

/// An invented effort value from the client must NOT reach the upstream.
#[tokio::test]
async fn invented_effort_never_reaches_upstream() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "effort-invented", None).await;

    let req = serde_json::json!({
        "model": "claude-gpt-6-astra",
        "max_tokens": 64,
        "stream": true,
        "output_config": {"effort": "ultracode-fake-effort-value"},
        "messages": [{"role":"user","content":"hi"}]
    });
    let (status, _) = post_messages(&state, None, req).await;
    assert_eq!(status, StatusCode::OK);

    let sent = mock.last_body();
    assert!(
        !sent.contains("ultracode"),
        "invented effort leaked upstream: {sent}"
    );
}

// ---------------------------------------------------------------------------
// count_tokens
// ---------------------------------------------------------------------------

#[tokio::test]
async fn count_tokens_never_hits_upstream() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "count", None).await;

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "model":"claude-gpt-6-astra",
                "messages":[{"role":"user","content":"hello world"}]
            })
            .to_string(),
        ))
        .unwrap();
    let resp = claudecodex::server::router(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let raw = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();

    assert!(
        v["input_tokens"].as_u64().unwrap_or(0) > 0,
        "must return a positive estimate: {v}"
    );
    assert_eq!(mock.hits(), 0, "count_tokens must be local-only");
}

// ---------------------------------------------------------------------------
// Request translation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn system_prompt_and_multiturn_are_forwarded_in_order() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "multiturn", None).await;

    let req = serde_json::json!({
        "model": "claude-gpt-6-astra",
        "max_tokens": 64,
        "stream": true,
        "system": "You are terse.",
        "messages": [
            {"role":"user","content":"first"},
            {"role":"assistant","content":"ack"},
            {"role":"user","content":"second"}
        ]
    });
    let (status, _) = post_messages(&state, None, req).await;
    assert_eq!(status, StatusCode::OK);

    let sent = mock.last_body();
    assert!(
        sent.contains("You are terse."),
        "system prompt must be forwarded"
    );
    assert!(
        sent.find("first").unwrap() < sent.find("second").unwrap(),
        "message order must be preserved: {sent}"
    );
}

#[tokio::test]
async fn tool_result_cycle_is_forwarded_as_function_call_output() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "toolresult", None).await;

    let req = serde_json::json!({
        "model": "claude-gpt-6-astra",
        "max_tokens": 64,
        "stream": true,
        "messages": [
            {"role":"user","content":"read it"},
            {"role":"assistant","content":[
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/etc/hostname"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"myhost"}]}
            ]}
        ]
    });
    let (status, _) = post_messages(&state, None, req).await;
    assert_eq!(status, StatusCode::OK);

    let sent = mock.last_body();
    assert!(
        sent.contains("function_call_output"),
        "tool_result must become function_call_output: {sent}"
    );
    assert!(
        sent.contains("myhost"),
        "tool output text must be forwarded"
    );
    assert!(sent.contains("toolu_1"), "tool_use_id must correlate");
}

// ---------------------------------------------------------------------------
// Regressions for the hardening pass
// ---------------------------------------------------------------------------

/// Regression: document blocks used to deserialize as `Unknown` and were
/// silently dropped by the translator, changing what the model saw.
#[tokio::test]
async fn unsupported_document_block_is_rejected_explicitly() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = mock.state(addr, "docblock", None).await;

    let req = serde_json::json!({
        "model": "claude-gpt-6-astra",
        "max_tokens": 64,
        "stream": true,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0="}},
                {"type": "text", "text": "summarize"}
            ]
        }]
    });
    let (status, raw) = post_messages(&state, None, req).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "body was {raw}");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert!(
        raw.contains("'document'"),
        "error must name the offending block type: {raw}"
    );
    assert_eq!(
        mock.hits(),
        0,
        "an unsupported request must never reach the upstream"
    );
}

/// Regression: unknown blocks in the system array must be rejected too.
#[tokio::test]
async fn unsupported_block_in_system_is_rejected() {
    let (addr, _mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = _mock.state(addr, "sysblock", None).await;

    let req = serde_json::json!({
        "model": "claude-gpt-6-astra",
        "max_tokens": 64,
        "stream": true,
        "system": [{"type": "server_tool_use", "name": "web_search"}],
        "messages": [{"role": "user", "content": "hi"}]
    });
    let (status, raw) = post_messages(&state, None, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body was {raw}");
    assert!(raw.contains("'server_tool_use'"), "body was {raw}");
}

/// A supported block set still passes validation untouched.
#[tokio::test]
async fn supported_blocks_pass_validation() {
    let (addr, _mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = _mock.state(addr, "validblocks", None).await;

    let req = serde_json::json!({
        "model": "claude-gpt-6-astra",
        "max_tokens": 64,
        "stream": true,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}}
            ]
        }]
    });
    let (status, _) = post_messages(&state, None, req).await;
    assert_eq!(status, StatusCode::OK);
}

/// Regression: 429 used to surface immediately; now retried (bounded).
#[tokio::test]
async fn transient_429_is_retried_and_recovers() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse())
        .with_transient_failures(2, StatusCode::TOO_MANY_REQUESTS, "0")
        .spawn()
        .await;
    let state = mock.state(addr, "retry429", None).await;

    let (status, raw) = post_messages(&state, None, simple_request()).await;
    assert_eq!(status, StatusCode::OK, "body was {raw}");
    assert_eq!(
        mock.responses_calls(),
        3,
        "two transient failures then success => exactly 3 /responses calls"
    );
}

/// Persistent 5xx exhausts retries and surfaces the error, without hanging.
#[tokio::test]
async fn persistent_5xx_exhausts_retries_and_surfaces_error() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse())
        .with_transient_failures(50, StatusCode::BAD_GATEWAY, "0")
        .spawn()
        .await;
    let state = mock.state(addr, "retry5xx", None).await;

    let res = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        post_messages(&state, None, simple_request()),
    )
    .await
    .expect("must not hang");
    let (status, _) = res;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        mock.responses_calls(),
        3,
        "1 initial attempt + 2 retries => exactly 3 /responses calls, got {}",
        mock.responses_calls()
    );
}

/// A non-retriable 4xx must NOT be retried.
#[tokio::test]
async fn non_retriable_400_is_not_retried() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse())
        .with_transient_failures(50, StatusCode::BAD_REQUEST, "0")
        .spawn()
        .await;
    let state = mock.state(addr, "noretry400", None).await;

    let (status, _) = post_messages(&state, None, simple_request()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mock.responses_calls(), 1, "400 must not be retried");
}

/// Quota exhaustion is distinct from a transient rate limit: it must surface
/// immediately without retry, instead of burning retries on the client's
/// own backoff.
#[tokio::test]
async fn quota_exhaustion_429_is_not_retried() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse())
        .with_transient_failures(50, StatusCode::TOO_MANY_REQUESTS, "5")
        .spawn()
        .await;
    *mock.quota_body.lock().await =
        Some(r#"{"error":{"message":"The usage limit has been reached"}}"#.to_string());
    let state = mock.state(addr, "quota", None).await;

    let started = std::time::Instant::now();
    let (status, raw) = post_messages(&state, None, simple_request()).await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "body was {raw}");
    assert_eq!(
        mock.responses_calls(),
        1,
        "quota exhaustion must not be retried"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "must fail fast (no Retry-After sleep), took {elapsed:?}"
    );
}

/// Quota-exhaustion 429 must carry a terminal retry-after (>60s) so the
/// client stops its retry loop at once, per the gateway contract.
#[tokio::test]
async fn quota_429_carries_terminal_retry_after() {
    let (addr, mock, _h) = MockUpstream::new(&good_sse())
        .with_transient_failures(50, StatusCode::TOO_MANY_REQUESTS, "5")
        .spawn()
        .await;
    *mock.quota_body.lock().await =
        Some(r#"{"error":{"message":"The usage limit has been reached"}}"#.to_string());
    let state = mock.state(addr, "quota-header", None).await;

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(simple_request().to_string()))
        .unwrap();
    let resp = claudecodex::server::router(state.clone())
        .oneshot(req)
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let ra = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .expect("quota 429 must send retry-after");
    assert!(ra > 60, "retry-after must be terminal (>60s), got {ra}");
}

/// The Retry-After parser honours the header and clamps absurd values.
#[test]
fn retry_after_is_capped() {
    use claudecodex::upstream::retry_after_delay_for_test;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use std::time::Duration;

    let cap = Duration::from_secs(20);
    let mut h = HeaderMap::new();
    h.insert(RETRY_AFTER, HeaderValue::from_static("3"));
    assert_eq!(retry_after_delay_for_test(&h, cap), Duration::from_secs(3));
    h.insert(RETRY_AFTER, HeaderValue::from_static("3600"));
    assert_eq!(
        retry_after_delay_for_test(&h, cap),
        cap,
        "must clamp to the cap"
    );
    h.remove(RETRY_AFTER);
    assert_eq!(
        retry_after_delay_for_test(&h, cap),
        Duration::from_secs(2),
        "missing header falls back to a short pause"
    );
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_sessions_are_isolated() {
    let (addr, _mock, _h) = MockUpstream::new(&good_sse()).spawn().await;
    let state = _mock.state(addr, "concurrent", None).await;

    let mut set = tokio::task::JoinSet::new();
    for i in 0..8 {
        let st = state.clone();
        set.spawn(async move {
            let mut req = simple_request();
            req["messages"][0]["content"] = serde_json::json!(format!("msg-{i}"));
            post_messages(&st, None, req).await.0
        });
    }

    let mut ok = 0;
    while let Some(r) = set.join_next().await {
        if r.unwrap() == StatusCode::OK {
            ok += 1;
        }
    }
    assert_eq!(ok, 8, "all concurrent sessions must succeed");
}
