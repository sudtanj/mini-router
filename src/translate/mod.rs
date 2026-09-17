//! Translation between the OpenAI Chat Completions and Anthropic Messages
//! dialects.
//!
//! This is what lets one front door reach every provider you have. A request
//! that arrived as `POST /v1/chat/completions` can be served by Anthropic, and
//! a request that arrived as `POST /v1/messages` can be served by OpenAI,
//! Groq, or anything else speaking the OpenAI shape.
//!
//! Everything is `serde_json::Value` rather than a wall of structs. Providers
//! add fields constantly; a Value-based translation passes the ones it does not
//! recognise through untouched instead of dropping them, and the mapping rules
//! stay readable next to each other.

pub mod sse;

use serde_json::{json, Map, Value};

use crate::util::now_millis;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslateError(pub String);

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TranslateError {}

fn err(msg: impl Into<String>) -> TranslateError {
    TranslateError(msg.into())
}

// ---------------------------------------------------------------------------
// Request: OpenAI -> Anthropic
// ---------------------------------------------------------------------------

/// Rewrite an OpenAI chat request as an Anthropic Messages request.
///
/// `target_model` is the provider-side model id chosen by the router, which is
/// rarely the name the client asked for.
pub fn request_openai_to_anthropic(
    body: &Value,
    target_model: &str,
    default_max_tokens: u64,
) -> Result<Value, TranslateError> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| err("request has no `messages` array"))?;

    let mut system_parts: Vec<String> = Vec::new();
    let mut out_messages: Vec<Value> = Vec::new();

    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            "system" | "developer" => {
                // Anthropic carries the system prompt beside the turns, not
                // inside them.
                if let Some(t) = content_to_text(m.get("content")) {
                    system_parts.push(t);
                }
            }
            "tool" => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                    "content": content_to_text(m.get("content")).unwrap_or_default(),
                });
                push_block(&mut out_messages, "user", block);
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(t) = content_to_text(m.get("content")) {
                    if !t.is_empty() {
                        blocks.push(json!({"type": "text", "text": t}));
                    }
                }
                for call in m
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let f = call.get("function");
                    let name = f
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    // OpenAI ships arguments as a JSON *string*; Anthropic
                    // wants the parsed object.
                    let args = f
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                        "name": name,
                        "input": args,
                    }));
                }
                if !blocks.is_empty() {
                    push_blocks(&mut out_messages, "assistant", blocks);
                }
            }
            _ => {
                let blocks = openai_content_to_anthropic_blocks(m.get("content"));
                if !blocks.is_empty() {
                    push_blocks(&mut out_messages, "user", blocks);
                }
            }
        }
    }

    if out_messages.is_empty() {
        return Err(err("request has no user or assistant messages to send"));
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(target_model));
    out.insert("messages".into(), Value::Array(out_messages));

    // Anthropic requires max_tokens; OpenAI treats it as optional.
    let max_tokens = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(default_max_tokens);
    out.insert("max_tokens".into(), json!(max_tokens));

    if !system_parts.is_empty() {
        out.insert("system".into(), json!(system_parts.join("\n\n")));
    }
    copy_if_present(body, &mut out, "temperature", "temperature");
    copy_if_present(body, &mut out, "top_p", "top_p");
    copy_if_present(body, &mut out, "stream", "stream");
    copy_if_present(body, &mut out, "metadata", "metadata");

    if let Some(stop) = body.get("stop") {
        let seqs = match stop {
            Value::String(s) => vec![json!(s)],
            Value::Array(a) => a.clone(),
            _ => vec![],
        };
        if !seqs.is_empty() {
            out.insert("stop_sequences".into(), Value::Array(seqs));
        }
    }

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let f = t.get("function")?;
                Some(json!({
                    "name": f.get("name")?,
                    "description": f.get("description").cloned().unwrap_or(Value::Null),
                    "input_schema": f
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                }))
            })
            .collect();
        if !mapped.is_empty() {
            out.insert("tools".into(), Value::Array(mapped));
        }
    }

    if let Some(choice) = body.get("tool_choice") {
        if let Some(mapped) = tool_choice_openai_to_anthropic(choice) {
            out.insert("tool_choice".into(), mapped);
        }
    }

    Ok(Value::Object(out))
}

fn tool_choice_openai_to_anthropic(choice: &Value) -> Option<Value> {
    match choice {
        Value::String(s) => match s.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "required" => Some(json!({"type": "any"})),
            // "none" has no Anthropic equivalent; omitting tool_choice with
            // tools present is the closest thing, so drop it.
            _ => None,
        },
        Value::Object(_) => {
            let name = choice
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)?;
            Some(json!({"type": "tool", "name": name}))
        }
        _ => None,
    }
}

/// OpenAI message content -> Anthropic content blocks.
fn openai_content_to_anthropic_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => {
            if s.is_empty() {
                vec![]
            } else {
                vec![json!({"type": "text", "text": s})]
            }
        }
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => Some(json!({
                    "type": "text",
                    "text": p.get("text").and_then(Value::as_str).unwrap_or(""),
                })),
                Some("image_url") => {
                    let url = p.get("image_url")?.get("url")?.as_str()?;
                    Some(image_block_from_url(url))
                }
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

/// `data:` URLs become base64 sources; everything else becomes a url source.
fn image_block_from_url(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(",") {
            let media_type = meta.split(';').next().unwrap_or("image/png");
            return json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data},
            });
        }
    }
    json!({"type": "image", "source": {"type": "url", "url": url}})
}

/// Append a block, merging into the previous message when the role matches.
/// Anthropic expects turns to alternate, so a run of OpenAI `tool` results
/// has to collapse into a single user turn.
fn push_block(messages: &mut Vec<Value>, role: &str, block: Value) {
    push_blocks(messages, role, vec![block]);
}

fn push_blocks(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
                arr.extend(blocks);
                return;
            }
        }
    }
    messages.push(json!({"role": role, "content": blocks}));
}

// ---------------------------------------------------------------------------
// Request: Anthropic -> OpenAI
// ---------------------------------------------------------------------------

/// Rewrite an Anthropic Messages request as an OpenAI chat request.
pub fn request_anthropic_to_openai(
    body: &Value,
    target_model: &str,
) -> Result<Value, TranslateError> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| err("request has no `messages` array"))?;

    let mut out_messages: Vec<Value> = Vec::new();

    if let Some(system) = body.get("system") {
        if let Some(text) = content_to_text(Some(system)) {
            if !text.is_empty() {
                out_messages.push(json!({"role": "system", "content": text}));
            }
        }
    }

    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = m.get("content");

        match content {
            Some(Value::Array(blocks)) => {
                let mut text_parts: Vec<Value> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                // tool_result blocks become their own `role: tool` messages,
                // which must follow the assistant turn that called them.
                let mut tool_results: Vec<Value> = Vec::new();

                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => text_parts.push(json!({
                            "type": "text",
                            "text": b.get("text").and_then(Value::as_str).unwrap_or(""),
                        })),
                        Some("image") => {
                            if let Some(url) = anthropic_image_to_url(b) {
                                text_parts
                                    .push(json!({"type": "image_url", "image_url": {"url": url}}));
                            }
                        }
                        Some("tool_use") => {
                            tool_calls.push(json!({
                                "id": b.get("id").and_then(Value::as_str).unwrap_or(""),
                                "type": "function",
                                "function": {
                                    "name": b.get("name").and_then(Value::as_str).unwrap_or(""),
                                    // OpenAI wants the arguments as a string.
                                    "arguments": b
                                        .get("input")
                                        .map(|i| i.to_string())
                                        .unwrap_or_else(|| "{}".into()),
                                },
                            }));
                        }
                        Some("tool_result") => {
                            tool_results.push(json!({
                                "role": "tool",
                                "tool_call_id": b
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or(""),
                                "content": content_to_text(b.get("content")).unwrap_or_default(),
                            }));
                        }
                        _ => {}
                    }
                }

                let has_text = !text_parts.is_empty();
                if has_text || !tool_calls.is_empty() {
                    let mut msg = Map::new();
                    msg.insert("role".into(), json!(role));
                    // A single text part is nicer as a plain string: some
                    // OpenAI-compatible servers only accept that form.
                    msg.insert("content".into(), simplify_parts(text_parts));
                    if !tool_calls.is_empty() {
                        msg.insert("tool_calls".into(), Value::Array(tool_calls));
                    }
                    out_messages.push(Value::Object(msg));
                }
                out_messages.extend(tool_results);
            }
            Some(other) => {
                if let Some(text) = content_to_text(Some(other)) {
                    out_messages.push(json!({"role": role, "content": text}));
                }
            }
            None => {}
        }
    }

    if out_messages.iter().all(|m| m["role"] == "system") {
        return Err(err("request has no user or assistant messages to send"));
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(target_model));
    out.insert("messages".into(), Value::Array(out_messages));

    copy_if_present(body, &mut out, "max_tokens", "max_tokens");
    copy_if_present(body, &mut out, "temperature", "temperature");
    copy_if_present(body, &mut out, "top_p", "top_p");
    copy_if_present(body, &mut out, "stream", "stream");
    // `top_k` has no OpenAI equivalent and is dropped on purpose.

    if let Some(stop) = body.get("stop_sequences").and_then(Value::as_array) {
        if !stop.is_empty() {
            out.insert("stop".into(), Value::Array(stop.clone()));
        }
    }

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter(|t| t.get("name").is_some())
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name"),
                        "description": t.get("description").cloned().unwrap_or(Value::Null),
                        "parameters": t
                            .get("input_schema")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    },
                })
            })
            .collect();
        if !mapped.is_empty() {
            out.insert("tools".into(), Value::Array(mapped));
        }
    }

    if let Some(choice) = body.get("tool_choice") {
        match choice.get("type").and_then(Value::as_str) {
            Some("auto") => {
                out.insert("tool_choice".into(), json!("auto"));
            }
            Some("any") => {
                out.insert("tool_choice".into(), json!("required"));
            }
            Some("tool") => {
                if let Some(name) = choice.get("name").and_then(Value::as_str) {
                    out.insert(
                        "tool_choice".into(),
                        json!({"type": "function", "function": {"name": name}}),
                    );
                }
            }
            _ => {}
        }
    }

    Ok(Value::Object(out))
}

fn anthropic_image_to_url(block: &Value) -> Option<String> {
    let source = block.get("source")?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media = source.get("media_type").and_then(Value::as_str)?;
            let data = source.get("data").and_then(Value::as_str)?;
            Some(format!("data:{media};base64,{data}"))
        }
        Some("url") => source.get("url").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn simplify_parts(parts: Vec<Value>) -> Value {
    if parts.len() == 1 && parts[0].get("type").and_then(Value::as_str) == Some("text") {
        return json!(parts[0].get("text").and_then(Value::as_str).unwrap_or(""));
    }
    if parts.is_empty() {
        return json!("");
    }
    Value::Array(parts)
}

// ---------------------------------------------------------------------------
// Response translation
// ---------------------------------------------------------------------------

/// Anthropic Messages response -> OpenAI chat completion.
pub fn response_anthropic_to_openai(body: &Value, requested_model: &str) -> Value {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in body
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => tool_calls.push(json!({
                "id": block.get("id").and_then(Value::as_str).unwrap_or(""),
                "type": "function",
                "function": {
                    "name": block.get("name").and_then(Value::as_str).unwrap_or(""),
                    "arguments": block.get("input").map(|i| i.to_string()).unwrap_or_else(|| "{}".into()),
                },
            })),
            _ => {}
        }
    }

    let stop_reason = body.get("stop_reason").and_then(Value::as_str);
    let finish_reason = stop_reason_to_finish_reason(stop_reason, !tool_calls.is_empty());

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert(
        "content".into(),
        if text.is_empty() && !tool_calls.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
    );
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }

    let usage = body.get("usage");
    let prompt_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64);
    let completion_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64);

    let mut out = json!({
        "id": body.get("id").and_then(Value::as_str).unwrap_or("chatcmpl-translated"),
        "object": "chat.completion",
        "created": now_millis() / 1000,
        "model": requested_model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason,
        }],
    });

    if prompt_tokens.is_some() || completion_tokens.is_some() {
        let p = prompt_tokens.unwrap_or(0);
        let c = completion_tokens.unwrap_or(0);
        out["usage"] = json!({
            "prompt_tokens": p,
            "completion_tokens": c,
            "total_tokens": p + c,
        });
    }
    out
}

/// OpenAI chat completion -> Anthropic Messages response.
pub fn response_openai_to_anthropic(body: &Value, requested_model: &str) -> Value {
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());
    let message = choice.and_then(|c| c.get("message"));

    let mut content: Vec<Value> = Vec::new();
    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
    {
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }
    let mut had_tool_calls = false;
    for call in message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        had_tool_calls = true;
        let f = call.get("function");
        content.push(json!({
            "type": "tool_use",
            "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
            "name": f.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or(""),
            "input": f
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| json!({})),
        }));
    }

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str);

    let usage = body.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    json!({
        "id": body.get("id").and_then(Value::as_str).unwrap_or("msg_translated"),
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": content,
        "stop_reason": finish_reason_to_stop_reason(finish_reason, had_tool_calls),
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
    })
}

pub fn stop_reason_to_finish_reason(
    stop_reason: Option<&str>,
    had_tool_calls: bool,
) -> &'static str {
    match stop_reason {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("end_turn") | Some("stop_sequence") => "stop",
        _ if had_tool_calls => "tool_calls",
        _ => "stop",
    }
}

pub fn finish_reason_to_stop_reason(finish: Option<&str>, had_tool_calls: bool) -> &'static str {
    match finish {
        Some("length") => "max_tokens",
        Some("tool_calls") | Some("function_call") => "tool_use",
        Some("stop") if had_tool_calls => "tool_use",
        _ => "end_turn",
    }
}

// ---------------------------------------------------------------------------
// Error bodies
// ---------------------------------------------------------------------------

/// Re-dress an error body in the dialect the client is speaking, so its SDK
/// can parse the failure instead of choking on it.
pub fn error_to_openai(body: &Value, fallback_message: &str) -> Value {
    if body.get("error").and_then(|e| e.get("message")).is_some()
        && body.get("type").and_then(Value::as_str) != Some("error")
    {
        return body.clone();
    }
    let (message, kind) = anthropic_error_parts(body, fallback_message);
    json!({"error": {"message": message, "type": kind, "code": kind}})
}

pub fn error_to_anthropic(body: &Value, fallback_message: &str) -> Value {
    if body.get("type").and_then(Value::as_str) == Some("error") {
        return body.clone();
    }
    let message = body
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(fallback_message)
        .to_owned();
    let kind = body
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("api_error")
        .to_owned();
    json!({"type": "error", "error": {"type": kind, "message": message}})
}

fn anthropic_error_parts(body: &Value, fallback: &str) -> (String, String) {
    let inner = body.get("error");
    let message = inner
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_owned();
    let kind = inner
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("api_error")
        .to_owned();
    (message, kind)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Flatten any of the content shapes either API accepts into plain text.
pub fn content_to_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                match p {
                    Value::String(s) => out.push_str(s),
                    Value::Object(_) => {
                        if let Some(t) = p.get("text").and_then(Value::as_str) {
                            out.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
            Some(out)
        }
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn copy_if_present(from: &Value, to: &mut Map<String, Value>, src: &str, dst: &str) {
    if let Some(v) = from.get(src) {
        if !v.is_null() {
            to.insert(dst.into(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests;
