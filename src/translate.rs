//! Translation between the Anthropic Messages API and the OpenAI Responses API.
//!
//! Request direction: [`anthropic_to_responses`]. Response direction: the
//! [`ResponsesToAnthropic`] state machine, which turns Responses SSE events into Anthropic
//! stream events, plus [`assemble`] for non-streaming replies.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use serde_json::{json, Map, Value};

use crate::anthropic::{
    ApiError, ContentBlock, Delta, ImageSource, MessageContent, MessageDeltaBody, MessageResponse,
    MessageStartBody, MessagesRequest, OutBlock, StreamEvent, SystemPrompt, Thinking,
    ToolResultContent, Usage,
};
use crate::upstream::{Reasoning, ResponsesRequest, UpstreamModel};

/// Prefix of the Anthropic `signature` we use to smuggle Codex encrypted reasoning back and
/// forth: `codexenc:<item id>:<encrypted_content>`.
pub const SIGNATURE_PREFIX: &str = "codexenc:";

// ---------------------------------------------------------------------------------------------
// Request direction
// ---------------------------------------------------------------------------------------------

/// Choose the reasoning effort, highest priority first:
///
/// 1. `output_config.effort` — the native Claude Code effort control (/effort,
///    effortLevel settings). When present it is the user's current explicit
///    choice and wins over everything: changing effort must never require
///    switching model ids.
/// 2. `:<effort>` suffix on the model id — the direct-API channel, for
///    callers that do not send output_config.
/// 3. Anthropic thinking budget, mapped to a coarse level.
/// 4. The CLI default (operator's explicit choice), clamped to what the
///    backend advertises for the model; an unsupported value falls back to
///    the backend-declared default, never to an invented level.
pub fn choose_effort(
    request_effort: Option<&str>,
    suffix: Option<&str>,
    thinking: Option<&Thinking>,
    cli_default: &str,
    model: Option<&UpstreamModel>,
) -> String {
    let wanted = request_effort
        .map(str::to_string)
        .or_else(|| suffix.map(str::to_string))
        .or_else(|| {
            thinking
                .filter(|t| t.kind == "enabled")
                .and_then(|t| t.budget_tokens)
                .map(|b| {
                    if b < 4096 {
                        "low"
                    } else if b < 16384 {
                        "medium"
                    } else {
                        "high"
                    }
                    .to_string()
                })
        });
    let Some(model) = model else {
        // Sem metadado do modelo nao ha como validar: recusa esforco arbitrario
        // (string controlada pelo cliente) e cai no default do operador.
        return wanted
            .filter(|e| KNOWN_EFFORTS.contains(&e.as_str()))
            .unwrap_or_else(|| cli_default.to_string());
    };
    // The CLI default is the operator's explicit choice and wins over the
    // backend-declared default; the backend default only applies as the
    // clamp below when the chosen level is not supported by the model.
    let wanted = wanted.unwrap_or_else(|| cli_default.to_string());
    if model.supported_reasoning_levels.is_empty() {
        // Catalogue gap: without declared levels, accept only observed levels.
        return if model_ok_uncatalogued(&wanted) {
            wanted
        } else {
            cli_default.to_string()
        };
    }
    if model
        .supported_reasoning_levels
        .iter()
        .any(|l| l.effort == wanted)
    {
        return wanted;
    }
    model
        .default_reasoning_level
        .clone()
        .or_else(|| {
            model
                .supported_reasoning_levels
                .first()
                .map(|l| l.effort.clone())
        })
        .unwrap_or(wanted)
}

/// Effort levels the backend has been observed to declare across its models
/// (dump autenticado 2026-10-13: low/medium/high/xhigh/max/ultra). Guard de
/// emergencia para os caminhos sem metadado do modelo — nunca inventa nivel.
const KNOWN_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];

fn model_ok_uncatalogued(e: &str) -> bool {
    KNOWN_EFFORTS.contains(&e)
}

/// Public: is this an effort level the backend has been observed to declare?
/// Used by the redacted diagnostic log (only printable levels reach journald).
pub fn is_known_effort(e: &str) -> bool {
    KNOWN_EFFORTS.contains(&e)
}

pub fn system_text(system: Option<&SystemPrompt>) -> String {
    match system {
        None => String::new(),
        Some(SystemPrompt::Text(s)) => s.clone(),
        Some(SystemPrompt::Blocks(blocks)) => blocks
            .iter()
            .map(|b| b.text.as_str())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

fn image_item(src: &ImageSource) -> Option<Value> {
    let url = match src.kind.as_str() {
        "base64" if !src.data.is_empty() => {
            format!("data:{};base64,{}", src.media_type, src.data)
        }
        "url" => src.url.clone()?,
        _ => return None,
    };
    Some(json!({"type": "input_image", "image_url": url, "detail": "auto"}))
}

fn tool_result_text(content: Option<&ToolResultContent>) -> (String, Vec<Value>) {
    match content {
        None => (String::new(), Vec::new()),
        Some(ToolResultContent::Text(s)) => (s.clone(), Vec::new()),
        Some(ToolResultContent::Blocks(blocks)) => {
            let mut text = Vec::new();
            let mut images = Vec::new();
            for b in blocks {
                match b {
                    ContentBlock::Text { text: t } => text.push(t.as_str()),
                    ContentBlock::Image { source } => images.extend(image_item(source)),
                    _ => {}
                }
            }
            (text.join("\n"), images)
        }
    }
}

/// Parse a signature produced by [`signature_for`].
fn parse_signature(sig: &str) -> Option<(Option<String>, String)> {
    let rest = sig.strip_prefix(SIGNATURE_PREFIX)?;
    let (id, enc) = rest.split_once(':')?;
    if enc.is_empty() {
        return None;
    }
    let id = if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    };
    Some((id, enc.to_string()))
}

pub fn signature_for(item_id: Option<&str>, encrypted: &str) -> String {
    format!("{SIGNATURE_PREFIX}{}:{encrypted}", item_id.unwrap_or(""))
}

struct InputBuilder {
    items: Vec<Value>,
    user_parts: Vec<Value>,
    assistant_parts: Vec<Value>,
}

impl InputBuilder {
    fn flush_user(&mut self) {
        if !self.user_parts.is_empty() {
            let content = std::mem::take(&mut self.user_parts);
            self.items
                .push(json!({"type": "message", "role": "user", "content": content}));
        }
    }

    fn flush_assistant(&mut self) {
        if !self.assistant_parts.is_empty() {
            let content = std::mem::take(&mut self.assistant_parts);
            self.items
                .push(json!({"type": "message", "role": "assistant", "content": content}));
        }
    }

    fn flush(&mut self) {
        self.flush_user();
        self.flush_assistant();
    }
}

pub fn build_input(req: &MessagesRequest) -> Vec<Value> {
    let mut b = InputBuilder {
        items: Vec::new(),
        user_parts: Vec::new(),
        assistant_parts: Vec::new(),
    };
    for msg in &req.messages {
        let is_user = msg.role != "assistant";
        match &msg.content {
            MessageContent::Text(text) => {
                b.flush();
                if is_user {
                    b.user_parts
                        .push(json!({"type": "input_text", "text": text}));
                } else {
                    b.assistant_parts
                        .push(json!({"type": "output_text", "text": text}));
                }
            }
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            if is_user {
                                b.flush_assistant();
                                b.user_parts
                                    .push(json!({"type": "input_text", "text": text}));
                            } else {
                                b.flush_user();
                                b.assistant_parts
                                    .push(json!({"type": "output_text", "text": text}));
                            }
                        }
                        ContentBlock::Image { source } => {
                            if let Some(img) = image_item(source) {
                                b.flush_assistant();
                                b.user_parts.push(img);
                            }
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            b.flush();
                            let arguments = if input.is_null() {
                                "{}".to_string()
                            } else {
                                input.to_string()
                            };
                            b.items.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": arguments,
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            b.flush();
                            let (mut text, images) = tool_result_text(content.as_ref());
                            if *is_error && !text.starts_with("Error") {
                                text = format!("Error: {text}");
                            }
                            b.items.push(json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": text,
                            }));
                            if !images.is_empty() {
                                b.items.push(json!({
                                    "type": "message", "role": "user", "content": images
                                }));
                            }
                        }
                        ContentBlock::ToolReference { name, id } => {
                            // Claude Code 2.1.300+ sends tool_reference blocks
                            // when referencing a previously declared tool.
                            // The Codex backend has no equivalent — convert
                            // to a text reference so the model sees the context.
                            let text = if let Some(rid) = id {
                                format!("[tool_reference: {name} ({rid})]")
                            } else {
                                format!("[tool_reference: {name}]")
                            };
                            if is_user {
                                b.flush_assistant();
                                b.user_parts
                                    .push(json!({"type": "input_text", "text": text}));
                            } else {
                                b.flush_user();
                                b.assistant_parts
                                    .push(json!({"type": "output_text", "text": text}));
                            }
                        }
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        } => {
                            let Some((id, enc)) = signature.as_deref().and_then(parse_signature)
                            else {
                                // Cross-model history: an Anthropic-signed
                                // thinking block cannot replay to Codex — but
                                // dropping it silently contradicts the
                                // fail-loudly contract, so log it (no content).
                                log::warn!("dropping thinking block with non-codex signature");
                                continue;
                            };
                            b.flush();
                            let mut item = Map::new();
                            item.insert("type".into(), json!("reasoning"));
                            if let Some(id) = id {
                                item.insert("id".into(), json!(id));
                            }
                            let summary = if thinking.is_empty() {
                                json!([])
                            } else {
                                json!([{"type": "summary_text", "text": thinking}])
                            };
                            item.insert("summary".into(), summary);
                            item.insert("encrypted_content".into(), json!(enc));
                            b.items.push(Value::Object(item));
                        }
                        ContentBlock::RedactedThinking | ContentBlock::Unknown => {}
                    }
                }
                b.flush();
            }
        }
    }
    b.flush();
    b.items
}

pub fn build_tools(req: &MessagesRequest) -> Vec<Value> {
    req.tools
        .iter()
        .filter_map(|t| {
            if let Some(kind) = t.kind.as_deref() {
                if kind != "custom" {
                    // Redacted: the name is client-controlled JSON — only printable
                    // ASCII, capped, or the journal can be forged (fleet finding).
                    let safe_name: String = t
                        .name
                        .chars()
                        .filter(|c| c.is_ascii_graphic())
                        .take(64)
                        .collect();
                    log::warn!("skipping unsupported server tool: {safe_name}");
                    return None;
                }
            }
            if t.name.is_empty() {
                return None;
            }
            let parameters = t
                .input_schema
                .clone()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(json!({
                "type": "function",
                "name": t.name,
                "description": t.description.clone().unwrap_or_default(),
                "strict": false,
                "parameters": parameters,
            }))
        })
        .collect()
}

pub fn build_tool_choice(req: &MessagesRequest) -> Value {
    match req.tool_choice.as_ref() {
        Some(tc) if tc.kind == "any" => json!("required"),
        Some(tc) if tc.kind == "none" => json!("none"),
        Some(tc) if tc.kind == "tool" => match &tc.name {
            Some(name) => json!({"type": "function", "name": name}),
            None => json!("auto"),
        },
        _ => json!("auto"),
    }
}

fn hash_hex(s: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

pub fn anthropic_to_responses(
    req: &MessagesRequest,
    slug: &str,
    effort: &str,
    session_id: Option<&str>,
) -> ResponsesRequest {
    let instructions = system_text(req.system.as_ref());
    let prompt_cache_key = Some(
        session_id
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| hash_hex(&instructions)),
    );
    ResponsesRequest {
        model: slug.to_string(),
        instructions,
        input: build_input(req),
        tools: build_tools(req),
        tool_choice: build_tool_choice(req),
        parallel_tool_calls: true,
        reasoning: Reasoning {
            effort: effort.to_string(),
            summary: "auto",
        },
        store: false,
        stream: true,
        include: vec!["reasoning.encrypted_content"],
        prompt_cache_key,
        text: None,
        temperature: req.temperature,
        top_p: req.top_p,
    }
}

// ---------------------------------------------------------------------------------------------
// SSE framing
// ---------------------------------------------------------------------------------------------

/// Minimal server-sent-events parser: feed bytes, get `(event, data)` frames.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some((frame_bytes, sep_len)) = find_frame_end(&self.buf) {
            // Zero-copy: parse directly from the buffer slice instead of
            // draining into a fresh Vec<u8> per frame. SSE payloads are UTF-8
            // JSON; invalid UTF-8 frames are dropped.
            let text = match std::str::from_utf8(&self.buf[..frame_bytes]) {
                Ok(t) => t,
                Err(_) => {
                    let _ = self.buf.drain(..frame_bytes + sep_len);
                    continue;
                }
            };
            if let Some(f) = parse_frame(text) {
                frames.push(f);
            }
            let _ = self.buf.drain(..frame_bytes + sep_len);
        }
        frames
    }
}

fn find_frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

fn parse_frame(text: &str) -> Option<SseFrame> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(SseFrame {
        event,
        data: data.join("\n"),
    })
}

// ---------------------------------------------------------------------------------------------
// Response direction
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    ToolUse,
    Thinking,
}

#[derive(Debug)]
struct Block {
    index: usize,
    kind: Kind,
    streamed: bool,
    closed: bool,
    summary_parts: usize,
}

/// Converts Responses API events into Anthropic Messages stream events.
pub struct ResponsesToAnthropic {
    model: String,
    msg_id: String,
    started: bool,
    finished: bool,
    next_index: usize,
    blocks: BTreeMap<u64, Block>,
    open: Option<u64>,
    saw_tool_use: bool,
}

fn parse_usage(usage: &Value) -> Usage {
    let input = usage["input_tokens"].as_u64().unwrap_or(0);
    let cached = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    let cache_write = usage["input_tokens_details"]["cache_write_tokens"]
        .as_u64()
        .unwrap_or(0);
    Usage {
        input_tokens: input.saturating_sub(cached),
        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
        cache_creation_input_tokens: cache_write,
        cache_read_input_tokens: cached,
    }
}

impl ResponsesToAnthropic {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            msg_id: format!("msg_{}", crate::util::unique_id()),
            started: false,
            finished: false,
            next_index: 0,
            blocks: BTreeMap::new(),
            open: None,
            saw_tool_use: false,
        }
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    fn ensure_started(&mut self, out: &mut Vec<StreamEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(StreamEvent::MessageStart {
            message: MessageStartBody {
                id: self.msg_id.clone(),
                kind: "message",
                role: "assistant",
                model: self.model.clone(),
                content: Vec::new(),
                stop_reason: None,
                stop_sequence: None,
                usage: Usage::default(),
            },
        });
    }

    fn close_open(&mut self, out: &mut Vec<StreamEvent>) {
        if let Some(oi) = self.open.take() {
            if let Some(b) = self.blocks.get_mut(&oi) {
                if !b.closed {
                    b.closed = true;
                    out.push(StreamEvent::ContentBlockStop { index: b.index });
                }
            }
        }
    }

    /// Open (or return) the block for an output index. Returns the Anthropic block index, or
    /// None when the block was already closed.
    fn block_for(
        &mut self,
        output_index: u64,
        kind: Kind,
        start: impl FnOnce() -> OutBlock,
        out: &mut Vec<StreamEvent>,
    ) -> Option<usize> {
        self.ensure_started(out);
        if let Some(b) = self.blocks.get(&output_index) {
            return if b.closed { None } else { Some(b.index) };
        }
        self.close_open(out);
        let index = self.next_index;
        self.next_index += 1;
        if kind == Kind::ToolUse {
            self.saw_tool_use = true;
        }
        out.push(StreamEvent::ContentBlockStart {
            index,
            content_block: start(),
        });
        self.blocks.insert(
            output_index,
            Block {
                index,
                kind,
                streamed: false,
                closed: false,
                summary_parts: 0,
            },
        );
        self.open = Some(output_index);
        Some(index)
    }

    fn open_item(&mut self, output_index: u64, item: &Value, out: &mut Vec<StreamEvent>) {
        match item["type"].as_str().unwrap_or("") {
            "message" => {
                self.block_for(
                    output_index,
                    Kind::Text,
                    || OutBlock::Text {
                        text: String::new(),
                    },
                    out,
                );
            }
            "function_call" => {
                let id = item["call_id"]
                    .as_str()
                    .or_else(|| item["id"].as_str())
                    .unwrap_or("call_unknown")
                    .to_string();
                let name = item["name"].as_str().unwrap_or("").to_string();
                self.block_for(
                    output_index,
                    Kind::ToolUse,
                    || OutBlock::ToolUse {
                        id,
                        name,
                        input: json!({}),
                    },
                    out,
                );
            }
            "reasoning" => {
                self.block_for(
                    output_index,
                    Kind::Thinking,
                    || OutBlock::Thinking {
                        thinking: String::new(),
                        signature: String::new(),
                    },
                    out,
                );
            }
            _ => {}
        }
    }

    fn delta(&mut self, output_index: u64, kind: Kind, delta: Delta, out: &mut Vec<StreamEvent>) {
        let start = move || match kind {
            Kind::Text => OutBlock::Text {
                text: String::new(),
            },
            Kind::ToolUse => OutBlock::ToolUse {
                id: "call_unknown".into(),
                name: String::new(),
                input: json!({}),
            },
            Kind::Thinking => OutBlock::Thinking {
                thinking: String::new(),
                signature: String::new(),
            },
        };
        let Some(index) = self.block_for(output_index, kind, start, out) else {
            return;
        };
        if let Some(b) = self.blocks.get_mut(&output_index) {
            b.streamed = true;
        }
        out.push(StreamEvent::ContentBlockDelta { index, delta });
    }

    fn item_done(&mut self, output_index: u64, item: &Value, out: &mut Vec<StreamEvent>) {
        self.open_item(output_index, item, out);
        let Some(b) = self.blocks.get(&output_index) else {
            return;
        };
        if b.closed {
            return;
        }
        let index = b.index;
        let streamed = b.streamed;
        match b.kind {
            Kind::Text => {
                if !streamed {
                    let text: String = item["content"]
                        .as_array()
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|p| p["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default();
                    if !text.is_empty() {
                        out.push(StreamEvent::ContentBlockDelta {
                            index,
                            delta: Delta::Text { text },
                        });
                    }
                }
            }
            Kind::ToolUse => {
                if !streamed {
                    let args = item["arguments"].as_str().unwrap_or("").to_string();
                    if !args.is_empty() {
                        out.push(StreamEvent::ContentBlockDelta {
                            index,
                            delta: Delta::InputJson { partial_json: args },
                        });
                    }
                }
            }
            Kind::Thinking => {
                if !streamed {
                    let text: String = item["summary"]
                        .as_array()
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|p| p["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                        .unwrap_or_default();
                    if !text.is_empty() {
                        out.push(StreamEvent::ContentBlockDelta {
                            index,
                            delta: Delta::Thinking { thinking: text },
                        });
                    }
                }
                if let Some(enc) = item["encrypted_content"].as_str() {
                    if !enc.is_empty() {
                        out.push(StreamEvent::ContentBlockDelta {
                            index,
                            delta: Delta::Signature {
                                signature: signature_for(item["id"].as_str(), enc),
                            },
                        });
                    }
                }
            }
        }
        if let Some(b) = self.blocks.get_mut(&output_index) {
            b.closed = true;
        }
        if self.open == Some(output_index) {
            self.open = None;
        }
        out.push(StreamEvent::ContentBlockStop { index });
    }

    fn finish(&mut self, stop_reason: &str, usage: Usage, out: &mut Vec<StreamEvent>) {
        if self.finished {
            return;
        }
        self.ensure_started(out);
        self.close_open(out);
        self.finished = true;
        out.push(StreamEvent::MessageDelta {
            delta: MessageDeltaBody {
                stop_reason: Some(stop_reason.to_string()),
                stop_sequence: None,
            },
            usage,
        });
        out.push(StreamEvent::MessageStop);
    }

    fn fail(&mut self, message: String, out: &mut Vec<StreamEvent>) {
        if self.finished {
            return;
        }
        self.finished = true;
        out.push(StreamEvent::Error {
            error: ApiError {
                kind: "api_error".into(),
                message,
            },
        });
    }

    /// Feed one Responses API event (the JSON of an SSE `data:` line).
    pub fn handle(&mut self, ev: &Value) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        let kind = ev["type"].as_str().unwrap_or("");
        let oi = ev["output_index"].as_u64().unwrap_or(0);
        let text_delta = || ev["delta"].as_str().unwrap_or("").to_string();
        match kind {
            "response.created" | "response.in_progress" => self.ensure_started(&mut out),
            "response.output_item.added" => self.open_item(oi, &ev["item"], &mut out),
            "response.output_text.delta" => {
                self.delta(oi, Kind::Text, Delta::Text { text: text_delta() }, &mut out)
            }
            "response.reasoning_summary_part.added" => {
                if let Some(b) = self.blocks.get_mut(&oi) {
                    if b.kind == Kind::Thinking && b.summary_parts > 0 && b.streamed {
                        let index = b.index;
                        out.push(StreamEvent::ContentBlockDelta {
                            index,
                            delta: Delta::Thinking {
                                thinking: "\n\n".into(),
                            },
                        });
                    }
                    b.summary_parts += 1;
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => self
                .delta(
                    oi,
                    Kind::Thinking,
                    Delta::Thinking {
                        thinking: text_delta(),
                    },
                    &mut out,
                ),
            "response.function_call_arguments.delta" => self.delta(
                oi,
                Kind::ToolUse,
                Delta::InputJson {
                    partial_json: text_delta(),
                },
                &mut out,
            ),
            "response.output_item.done" => self.item_done(oi, &ev["item"], &mut out),
            "response.completed" | "response.incomplete" => {
                let response = &ev["response"];
                let usage = parse_usage(&response["usage"]);
                let incomplete_reason = response["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("");
                let stop =
                    if kind == "response.incomplete" && incomplete_reason == "max_output_tokens" {
                        "max_tokens"
                    } else if self.saw_tool_use {
                        "tool_use"
                    } else {
                        "end_turn"
                    };
                self.finish(stop, usage, &mut out);
            }
            "response.failed" => {
                let msg = ev["response"]["error"]["message"]
                    .as_str()
                    .unwrap_or("upstream response failed")
                    .to_string();
                self.fail(msg, &mut out);
            }
            "error" => {
                let msg = ev["message"]
                    .as_str()
                    .or_else(|| ev["error"]["message"].as_str())
                    .unwrap_or("upstream error")
                    .to_string();
                self.fail(msg, &mut out);
            }
            _ => {}
        }
        out
    }

    /// Called when the upstream stream ends without a terminal event.
    pub fn end_of_stream(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if !self.finished {
            if self.started {
                let stop = if self.saw_tool_use {
                    "tool_use"
                } else {
                    "end_turn"
                };
                self.finish(stop, Usage::default(), &mut out);
            } else {
                self.fail(
                    "upstream stream ended before any response event".into(),
                    &mut out,
                );
            }
        }
        out
    }
}

/// Fold stream events into a complete (non-streaming) message.
pub fn assemble(model: &str, events: &[StreamEvent]) -> Result<MessageResponse, ApiError> {
    let mut msg = MessageResponse::new(model.to_string());
    let mut json_buf: BTreeMap<usize, String> = BTreeMap::new();
    for ev in events {
        match ev {
            StreamEvent::MessageStart { message } => msg.id = message.id.clone(),
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                while msg.content.len() < *index {
                    msg.content.push(OutBlock::Text {
                        text: String::new(),
                    });
                }
                msg.content.push(content_block.clone());
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                let Some(block) = msg.content.get_mut(*index) else {
                    continue;
                };
                match (block, delta) {
                    (OutBlock::Text { text }, Delta::Text { text: d }) => text.push_str(d),
                    (OutBlock::ToolUse { .. }, Delta::InputJson { partial_json }) => {
                        json_buf.entry(*index).or_default().push_str(partial_json)
                    }
                    (OutBlock::Thinking { thinking, .. }, Delta::Thinking { thinking: d }) => {
                        thinking.push_str(d)
                    }
                    (OutBlock::Thinking { signature, .. }, Delta::Signature { signature: s }) => {
                        signature.push_str(s)
                    }
                    _ => {}
                }
            }
            StreamEvent::ContentBlockStop { .. } | StreamEvent::Ping => {}
            StreamEvent::MessageDelta { delta, usage } => {
                msg.stop_reason = delta.stop_reason.clone();
                msg.usage = usage.clone();
            }
            StreamEvent::MessageStop => {}
            StreamEvent::Error { error } => return Err(error.clone()),
        }
    }
    for (index, raw) in json_buf {
        if let Some(OutBlock::ToolUse { input, .. }) = msg.content.get_mut(index) {
            *input = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
        }
    }
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(raw: &str) -> MessagesRequest {
        serde_json::from_str(raw).expect("request parses")
    }

    #[test]
    fn two_turn_tool_round_trip_matches_exact_json() {
        let r = req(r#"{
            "model":"claude-gpt-5.5","max_tokens":100,
            "system":[{"type":"text","text":"You are terse."},{"type":"text","text":"Use tools."}],
            "messages":[
              {"role":"user","content":"What time is it?"},
              {"role":"assistant","content":[
                 {"type":"thinking","thinking":"need clock","signature":"codexenc:rs_1:ENC"},
                 {"type":"text","text":"Checking."},
                 {"type":"tool_use","id":"call_1","name":"clock","input":{"tz":"UTC"}}]},
              {"role":"user","content":[
                 {"type":"tool_result","tool_use_id":"call_1","content":[{"type":"text","text":"12:00"}]},
                 {"type":"text","text":"thanks"}]}
            ],
            "tools":[{"name":"clock","description":"Read the clock","input_schema":{"type":"object","properties":{"tz":{"type":"string"}}}},
                     {"type":"web_search_20250305","name":"web_search"}],
            "tool_choice":{"type":"any"}
        }"#);
        let out = anthropic_to_responses(&r, "gpt-5.5", "low", Some("sess-1"));
        let got = serde_json::to_value(&out).expect("serializes");
        let want = json!({
            "model":"gpt-5.5",
            "instructions":"You are terse.\n\nUse tools.",
            "input":[
              {"type":"message","role":"user","content":[{"type":"input_text","text":"What time is it?"}]},
              {"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"need clock"}],"encrypted_content":"ENC"},
              {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Checking."}]},
              {"type":"function_call","call_id":"call_1","name":"clock","arguments":"{\"tz\":\"UTC\"}"},
              {"type":"function_call_output","call_id":"call_1","output":"12:00"},
              {"type":"message","role":"user","content":[{"type":"input_text","text":"thanks"}]}
            ],
            "tools":[{"type":"function","name":"clock","description":"Read the clock","strict":false,
                      "parameters":{"type":"object","properties":{"tz":{"type":"string"}}}}],
            "tool_choice":"required",
            "parallel_tool_calls":true,
            "reasoning":{"effort":"low","summary":"auto"},
            "store":false,
            "stream":true,
            "include":["reasoning.encrypted_content"],
            "prompt_cache_key":"sess-1"
        });
        assert_eq!(got, want);
    }

    #[test]
    fn effort_selection() {
        let thinking = |b: u64| Thinking {
            kind: "enabled".into(),
            budget_tokens: Some(b),
        };
        // New signature: (request_effort, suffix, thinking, cli_default, model)
        // Suffix channel (direct API, no output_config).
        assert_eq!(
            choose_effort(None, Some("xhigh"), None, "medium", None),
            "xhigh"
        );
        // Native control wins over the id suffix.
        assert_eq!(
            choose_effort(Some("low"), Some("xhigh"), None, "medium", None),
            "low"
        );
        // Thinking-budget mapping.
        assert_eq!(
            choose_effort(None, None, Some(&thinking(1024)), "medium", None),
            "low"
        );
        assert_eq!(
            choose_effort(None, None, Some(&thinking(8000)), "low", None),
            "medium"
        );
        assert_eq!(
            choose_effort(None, None, Some(&thinking(32000)), "low", None),
            "high"
        );
        assert_eq!(choose_effort(None, None, None, "medium", None), "medium");
        let model: UpstreamModel = serde_json::from_value(json!({
            "slug":"m","default_reasoning_level":"low",
            "supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]
        }))
        .expect("model");
        // Backend default applies when the client sends nothing.
        assert_eq!(
            choose_effort(None, None, None, "medium", Some(&model)),
            "low"
        );
        assert_eq!(
            choose_effort(None, None, None, "high", Some(&model)),
            "high"
        );
        // Unsupported request effort clamps to the backend default, never an
        // invented level.
        assert_eq!(
            choose_effort(Some("ultracode"), None, None, "medium", Some(&model)),
            "low"
        );
        // Supported request effort passes through.
        assert_eq!(
            choose_effort(Some("high"), None, None, "medium", Some(&model)),
            "high"
        );
        // Without model metadata, an invented value falls back to the CLI
        // default (never reaches the backend).
        assert_eq!(
            choose_effort(Some("ultracode"), None, None, "medium", None),
            "medium"
        );
    }

    #[test]
    fn output_config_effort_is_parsed() {
        // The native Claude Code effort control arrives as output_config.effort.
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-gpt-6-astra",
            "messages": [{"role":"user","content":"hi"}],
            "output_config": {"effort": "high"}
        }))
        .expect("parses");
        assert_eq!(
            req.output_config.as_ref().and_then(|o| o.effort.as_deref()),
            Some("high")
        );
        // Absent output_config stays None.
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-gpt-6-astra",
            "messages": [{"role":"user","content":"hi"}]
        }))
        .expect("parses");
        assert!(req.output_config.is_none());
    }

    fn drive(events: &[Value]) -> Vec<StreamEvent> {
        let mut sm = ResponsesToAnthropic::new("claude-gpt-5.5");
        let mut out = Vec::new();
        for e in events {
            out.extend(sm.handle(e));
        }
        out.extend(sm.end_of_stream());
        out
    }

    fn names(events: &[StreamEvent]) -> Vec<&'static str> {
        events.iter().map(|e| e.event_name()).collect()
    }

    #[test]
    fn text_only_sequence() {
        let events = vec![
            json!({"type":"response.created","response":{"id":"resp_1"}}),
            json!({"type":"response.in_progress"}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","content":[]}}),
            json!({"type":"response.content_part.added","output_index":0}),
            json!({"type":"response.output_text.delta","output_index":0,"delta":"Hi "}),
            json!({"type":"response.output_text.delta","output_index":0,"delta":"there"}),
            json!({"type":"response.output_text.done","output_index":0,"text":"Hi there"}),
            json!({"type":"response.content_part.done","output_index":0}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"Hi there"}]}}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed",
                "usage":{"input_tokens":120,"input_tokens_details":{"cached_tokens":100,"cache_write_tokens":5},"output_tokens":3,"total_tokens":123}}}),
        ];
        let out = drive(&events);
        assert_eq!(
            names(&out),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        let msg = assemble("claude-gpt-5.5", &out).expect("assembles");
        assert_eq!(
            msg.content,
            vec![OutBlock::Text {
                text: "Hi there".into()
            }]
        );
        assert_eq!(msg.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(
            msg.usage,
            Usage {
                input_tokens: 20,
                output_tokens: 3,
                cache_creation_input_tokens: 5,
                cache_read_input_tokens: 100
            }
        );
    }

    #[test]
    fn single_function_call_sequence() {
        let events = vec![
            json!({"type":"response.created","response":{}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"clock","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"tz\":"}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"\"UTC\"}"}),
            json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"tz\":\"UTC\"}"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"clock","arguments":"{\"tz\":\"UTC\"}"}}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":4}}}),
        ];
        let out = drive(&events);
        assert!(matches!(
            &out[1],
            StreamEvent::ContentBlockStart { index: 0, content_block: OutBlock::ToolUse { id, name, .. } }
                if id == "call_9" && name == "clock"
        ));
        let msg = assemble("m", &out).expect("assembles");
        assert_eq!(msg.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(
            msg.content,
            vec![OutBlock::ToolUse {
                id: "call_9".into(),
                name: "clock".into(),
                input: json!({"tz":"UTC"})
            }]
        );
    }

    #[test]
    fn function_call_without_deltas_emits_full_arguments() {
        let events = vec![
            json!({"type":"response.created"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"f","arguments":"{\"a\":1}"}}),
            json!({"type":"response.completed","response":{}}),
        ];
        let out = drive(&events);
        let deltas: Vec<_> = out
            .iter()
            .filter(|e| matches!(e, StreamEvent::ContentBlockDelta { .. }))
            .collect();
        assert_eq!(deltas.len(), 1);
        let msg = assemble("m", &out).expect("assembles");
        assert_eq!(msg.content.len(), 1);
        assert!(
            matches!(&msg.content[0], OutBlock::ToolUse { input, .. } if input == &json!({"a":1}))
        );
    }

    #[test]
    fn reasoning_then_text_sequence() {
        let events = vec![
            json!({"type":"response.created"}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}),
            json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":0}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"Think"}),
            json!({"type":"response.reasoning_summary_part.done","output_index":0}),
            json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":1}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"more"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Think"},{"type":"summary_text","text":"more"}],"encrypted_content":"ENC"}}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","content":[]}}),
            json!({"type":"response.output_text.delta","output_index":1,"delta":"Answer"}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","content":[{"type":"output_text","text":"Answer"}]}}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}),
            json!({"type":"some.future.event","weird":[1,2,{"x":null}]}),
        ];
        let out = drive(&events);
        let msg = assemble("m", &out).expect("assembles");
        assert_eq!(
            msg.content,
            vec![
                OutBlock::Thinking {
                    thinking: "Think\n\nmore".into(),
                    signature: "codexenc:rs_1:ENC".into()
                },
                OutBlock::Text {
                    text: "Answer".into()
                }
            ]
        );
        assert_eq!(
            names(&out).iter().filter(|n| **n == "message_stop").count(),
            1
        );
        // Round trip: the signature parses back into a reasoning item.
        assert_eq!(
            parse_signature("codexenc:rs_1:ENC"),
            Some((Some("rs_1".into()), "ENC".into()))
        );
        assert_eq!(parse_signature("codexenc::ENC"), Some((None, "ENC".into())));
        assert_eq!(parse_signature("sig_from_anthropic"), None);
    }

    #[test]
    fn failure_and_incomplete() {
        let out = drive(&[
            json!({"type":"response.created"}),
            json!({"type":"response.failed","response":{"error":{"message":"boom"}}}),
        ]);
        assert!(
            matches!(out.last(), Some(StreamEvent::Error { error }) if error.message == "boom")
        );
        assert!(assemble("m", &out).is_err());

        let out = drive(&[
            json!({"type":"response.created"}),
            json!({"type":"response.output_text.delta","output_index":0,"delta":"partial"}),
            json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{}}}),
        ]);
        let msg = assemble("m", &out).expect("assembles");
        assert_eq!(msg.stop_reason.as_deref(), Some("max_tokens"));
        assert_eq!(msg.content.len(), 1);
    }

    #[test]
    fn sse_parser_handles_split_frames() {
        let mut p = SseParser::default();
        let a = p.push(b"event: response.created\ndata: {\"a\":");
        assert!(a.is_empty());
        let b = p.push(b"1}\n\nevent: x\ndata: {\"b\":2}\r\n\r\n: comment\n\n");
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].event.as_deref(), Some("response.created"));
        assert_eq!(b[0].data, "{\"a\":1}");
        assert_eq!(b[1].data, "{\"b\":2}");
    }
}
