//! Streaming translation between the two dialects' server-sent events.
//!
//! Both protocols stream, and they stream very differently. OpenAI sends a flat
//! run of `chat.completion.chunk` objects terminated by `data: [DONE]`.
//! Anthropic sends a structured sequence -- `message_start`,
//! `content_block_start`, a run of deltas, `content_block_stop`,
//! `message_delta`, `message_stop` -- with the event name in the SSE frame.
//!
//! The translators here are incremental state machines: bytes in, bytes out,
//! nothing held but the few fields needed to close the stream correctly. A
//! translated stream is still a stream, and a long generation costs no more
//! memory than a short one.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::util::now_millis;

/// One parsed server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE parser. Chunk boundaries fall wherever TCP puts them, so
/// this holds a partial frame until the blank line that ends it arrives.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes, get whatever complete events they finished.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        loop {
            let Some((frame_len, sep_len)) = find_frame_end(&self.buf) else {
                break;
            };
            let frame = self.buf[..frame_len].to_vec();
            self.buf.drain(..frame_len + sep_len);
            if let Some(ev) = parse_frame(&frame) {
                events.push(ev);
            }
        }
        events
    }

    /// Flush a trailing frame that was never terminated by a blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let frame = std::mem::take(&mut self.buf);
        parse_frame(&frame).into_iter().collect()
    }
}

/// Returns (frame length, separator length) for the first complete frame.
fn find_frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == b'\n' {
            // "\n\n"
            if buf.get(i + 1) == Some(&b'\n') {
                return Some((i, 2));
            }
            // "\n\r\n"
            if buf.get(i + 1) == Some(&b'\r') && buf.get(i + 2) == Some(&b'\n') {
                return Some((i, 3));
            }
        }
        i += 1;
    }
    None
}

fn parse_frame(frame: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(frame);
    let mut event = None;
    let mut data_lines: Vec<&str> = Vec::new();

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data_lines.push(value),
            _ => {}
        }
    }

    if event.is_none() && data_lines.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data_lines.join("\n"),
    })
}

/// A streaming translator: upstream bytes in, client-facing bytes out.
pub trait StreamTranslator: Send + 'static {
    /// Translate a chunk of upstream body.
    fn push(&mut self, chunk: &[u8]) -> Vec<u8>;
    /// Called once the upstream body ends, to close the stream properly even
    /// if the provider hung up early.
    fn finish(&mut self) -> Vec<u8>;
}

fn emit_openai(out: &mut Vec<u8>, value: &Value) {
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(value.to_string().as_bytes());
    out.extend_from_slice(b"\n\n");
}

fn emit_anthropic(out: &mut Vec<u8>, event: &str, value: &Value) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(value.to_string().as_bytes());
    out.extend_from_slice(b"\n\n");
}

// ---------------------------------------------------------------------------
// Anthropic -> OpenAI
// ---------------------------------------------------------------------------

/// Turns an Anthropic Messages stream into OpenAI `chat.completion.chunk`s.
pub struct AnthropicToOpenai {
    parser: SseParser,
    /// The model name the *client* asked for, echoed back unchanged.
    model: String,
    id: String,
    created: u64,
    /// Anthropic content-block index -> OpenAI tool_calls index.
    tool_slots: HashMap<u64, usize>,
    next_tool_slot: usize,
    finish_reason: Option<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
    saw_usage: bool,
    role_sent: bool,
    done: bool,
}

impl AnthropicToOpenai {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            parser: SseParser::new(),
            model: model.into(),
            id: "chatcmpl-translated".into(),
            created: now_millis() / 1000,
            tool_slots: HashMap::new(),
            next_tool_slot: 0,
            finish_reason: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            saw_usage: false,
            role_sent: false,
            done: false,
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }],
        })
    }

    fn close(&mut self, out: &mut Vec<u8>) {
        if self.done {
            return;
        }
        self.done = true;
        let reason = self.finish_reason.clone().unwrap_or_else(|| "stop".into());
        let mut final_chunk = self.chunk(json!({}), Some(&reason));
        if self.saw_usage {
            final_chunk["usage"] = json!({
                "prompt_tokens": self.prompt_tokens,
                "completion_tokens": self.completion_tokens,
                "total_tokens": self.prompt_tokens + self.completion_tokens,
            });
        }
        emit_openai(out, &final_chunk);
        out.extend_from_slice(b"data: [DONE]\n\n");
    }

    fn handle(&mut self, ev: &SseEvent, out: &mut Vec<u8>) {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return;
        };
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .or(ev.event.as_deref())
            .unwrap_or("");

        match kind {
            "message_start" => {
                if let Some(msg) = v.get("message") {
                    if let Some(id) = msg.get("id").and_then(Value::as_str) {
                        self.id = id.to_owned();
                    }
                    if let Some(u) = msg.get("usage") {
                        if let Some(n) = u.get("input_tokens").and_then(Value::as_u64) {
                            self.prompt_tokens = n;
                            self.saw_usage = true;
                        }
                    }
                }
                if !self.role_sent {
                    self.role_sent = true;
                    let c = self.chunk(json!({"role": "assistant", "content": ""}), None);
                    emit_openai(out, &c);
                }
            }
            "content_block_start" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = v.get("content_block");
                if block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use") {
                    let slot = self.next_tool_slot;
                    self.next_tool_slot += 1;
                    self.tool_slots.insert(index, slot);
                    let c = self.chunk(
                        json!({"tool_calls": [{
                            "index": slot,
                            "id": block.and_then(|b| b.get("id")).and_then(Value::as_str).unwrap_or(""),
                            "type": "function",
                            "function": {
                                "name": block.and_then(|b| b.get("name")).and_then(Value::as_str).unwrap_or(""),
                                "arguments": "",
                            },
                        }]}),
                        None,
                    );
                    emit_openai(out, &c);
                }
            }
            "content_block_delta" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let delta = v.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta
                            .and_then(|d| d.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !text.is_empty() {
                            let c = self.chunk(json!({"content": text}), None);
                            emit_openai(out, &c);
                        }
                    }
                    Some("input_json_delta") => {
                        let partial = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let slot = self.tool_slots.get(&index).copied().unwrap_or(0);
                        let c = self.chunk(
                            json!({"tool_calls": [{
                                "index": slot,
                                "function": {"arguments": partial},
                            }]}),
                            None,
                        );
                        emit_openai(out, &c);
                    }
                    // Extended thinking has no Chat Completions equivalent.
                    // `reasoning_content` is what the OpenAI-compatible
                    // providers that do expose it have settled on.
                    Some("thinking_delta") => {
                        let text = delta
                            .and_then(|d| d.get("thinking"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !text.is_empty() {
                            let c = self.chunk(json!({"reasoning_content": text}), None);
                            emit_openai(out, &c);
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(r) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.finish_reason = Some(
                        super::stop_reason_to_finish_reason(Some(r), !self.tool_slots.is_empty())
                            .to_owned(),
                    );
                }
                if let Some(n) = v
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                {
                    self.completion_tokens = n;
                    self.saw_usage = true;
                }
            }
            "message_stop" => self.close(out),
            "error" => {
                let body = super::error_to_openai(&v, "upstream stream error");
                emit_openai(out, &body);
                self.close(out);
            }
            _ => {}
        }
    }
}

impl StreamTranslator for AnthropicToOpenai {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let events = self.parser.push(chunk);
        let mut out = Vec::new();
        for ev in events {
            self.handle(&ev, &mut out);
        }
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        let events = self.parser.finish();
        let mut out = Vec::new();
        for ev in events {
            self.handle(&ev, &mut out);
        }
        self.close(&mut out);
        out
    }
}

// ---------------------------------------------------------------------------
// OpenAI -> Anthropic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    Text(u64),
    Tool(u64),
}

impl OpenBlock {
    fn index(self) -> u64 {
        match self {
            OpenBlock::Text(i) | OpenBlock::Tool(i) => i,
        }
    }
}

/// Turns an OpenAI chunk stream into an Anthropic Messages event stream.
pub struct OpenaiToAnthropic {
    parser: SseParser,
    model: String,
    id: String,
    started: bool,
    open: Option<OpenBlock>,
    next_block: u64,
    /// OpenAI tool_calls index -> Anthropic content-block index.
    tool_blocks: HashMap<u64, u64>,
    stop_reason: Option<String>,
    had_tool_calls: bool,
    input_tokens: u64,
    output_tokens: u64,
    done: bool,
}

impl OpenaiToAnthropic {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            parser: SseParser::new(),
            model: model.into(),
            id: "msg_translated".into(),
            started: false,
            open: None,
            next_block: 0,
            tool_blocks: HashMap::new(),
            stop_reason: None,
            had_tool_calls: false,
            input_tokens: 0,
            output_tokens: 0,
            done: false,
        }
    }

    fn start(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        emit_anthropic(
            out,
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": self.input_tokens, "output_tokens": 0},
                },
            }),
        );
    }

    fn close_open_block(&mut self, out: &mut Vec<u8>) {
        if let Some(block) = self.open.take() {
            emit_anthropic(
                out,
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": block.index()}),
            );
        }
    }

    fn ensure_text_block(&mut self, out: &mut Vec<u8>) -> u64 {
        if let Some(OpenBlock::Text(i)) = self.open {
            return i;
        }
        self.close_open_block(out);
        let index = self.next_block;
        self.next_block += 1;
        self.open = Some(OpenBlock::Text(index));
        emit_anthropic(
            out,
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "text", "text": ""},
            }),
        );
        index
    }

    fn close(&mut self, out: &mut Vec<u8>) {
        if self.done {
            return;
        }
        self.done = true;
        self.start(out);
        self.close_open_block(out);
        let reason = self.stop_reason.clone().unwrap_or_else(|| {
            super::finish_reason_to_stop_reason(None, self.had_tool_calls).to_owned()
        });
        emit_anthropic(
            out,
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": reason, "stop_sequence": Value::Null},
                "usage": {"output_tokens": self.output_tokens},
            }),
        );
        emit_anthropic(out, "message_stop", &json!({"type": "message_stop"}));
    }

    fn handle(&mut self, ev: &SseEvent, out: &mut Vec<u8>) {
        let data = ev.data.trim();
        if data == "[DONE]" {
            self.close(out);
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return;
        };

        // Some providers signal a mid-stream failure as a bare error object.
        if v.get("error").is_some() && v.get("choices").is_none() {
            self.start(out);
            self.close_open_block(out);
            let body = super::error_to_anthropic(&v, "upstream stream error");
            emit_anthropic(out, "error", &body);
            self.done = true;
            return;
        }

        if let Some(id) = v.get("id").and_then(Value::as_str) {
            if !self.started {
                self.id = id.to_owned();
            }
        }
        if let Some(u) = v.get("usage") {
            if let Some(n) = u.get("prompt_tokens").and_then(Value::as_u64) {
                self.input_tokens = n;
            }
            if let Some(n) = u.get("completion_tokens").and_then(Value::as_u64) {
                self.output_tokens = n;
            }
        }

        self.start(out);

        let Some(choice) = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return;
        };

        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    let index = self.ensure_text_block(out);
                    emit_anthropic(
                        out,
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": {"type": "text_delta", "text": text},
                        }),
                    );
                }
            }

            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                self.had_tool_calls = true;
                let slot = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block_index = match self.tool_blocks.get(&slot).copied() {
                    Some(i) => i,
                    None => {
                        self.close_open_block(out);
                        let index = self.next_block;
                        self.next_block += 1;
                        self.tool_blocks.insert(slot, index);
                        self.open = Some(OpenBlock::Tool(index));
                        let f = call.get("function");
                        emit_anthropic(
                            out,
                            "content_block_start",
                            &json!({
                                "type": "content_block_start",
                                "index": index,
                                "content_block": {
                                    "type": "tool_use",
                                    "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                                    "name": f.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or(""),
                                    "input": {},
                                },
                            }),
                        );
                        index
                    }
                };
                if let Some(args) = call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                {
                    if !args.is_empty() {
                        emit_anthropic(
                            out,
                            "content_block_delta",
                            &json!({
                                "type": "content_block_delta",
                                "index": block_index,
                                "delta": {"type": "input_json_delta", "partial_json": args},
                            }),
                        );
                    }
                }
            }
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = Some(
                super::finish_reason_to_stop_reason(Some(reason), self.had_tool_calls).to_owned(),
            );
        }
    }
}

impl StreamTranslator for OpenaiToAnthropic {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let events = self.parser.push(chunk);
        let mut out = Vec::new();
        for ev in events {
            self.handle(&ev, &mut out);
        }
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        let events = self.parser.finish();
        let mut out = Vec::new();
        for ev in events {
            self.handle(&ev, &mut out);
        }
        self.close(&mut out);
        out
    }
}
