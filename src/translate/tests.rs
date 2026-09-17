use super::sse::{AnthropicToOpenai, OpenaiToAnthropic, SseParser, StreamTranslator};
use super::*;

const MAX_TOKENS: u64 = 4096;

fn v(s: &str) -> Value {
    serde_json::from_str(s).expect("test fixture should be valid json")
}

// ---------------------------------------------------------------------------
// Requests: OpenAI -> Anthropic
// ---------------------------------------------------------------------------

#[test]
fn system_messages_move_beside_the_turns() {
    let out = request_openai_to_anthropic(
        &v(r#"{"model":"whatever","messages":[
            {"role":"system","content":"Be brief."},
            {"role":"system","content":"Be kind."},
            {"role":"user","content":"hi"}]}"#),
        "claude-haiku-4-5",
        MAX_TOKENS,
    )
    .unwrap();

    assert_eq!(out["model"], "claude-haiku-4-5");
    assert_eq!(out["system"], "Be brief.\n\nBe kind.");
    assert_eq!(out["messages"].as_array().unwrap().len(), 1);
    assert_eq!(out["messages"][0]["role"], "user");
    assert_eq!(out["messages"][0]["content"][0]["text"], "hi");
}

#[test]
fn anthropic_requires_max_tokens_so_we_supply_one() {
    let without = request_openai_to_anthropic(
        &v(r#"{"messages":[{"role":"user","content":"hi"}]}"#),
        "m",
        MAX_TOKENS,
    )
    .unwrap();
    assert_eq!(without["max_tokens"], MAX_TOKENS);

    let with = request_openai_to_anthropic(
        &v(r#"{"messages":[{"role":"user","content":"hi"}],"max_tokens":128}"#),
        "m",
        MAX_TOKENS,
    )
    .unwrap();
    assert_eq!(with["max_tokens"], 128);

    // The newer OpenAI field wins over the deprecated one.
    let newer = request_openai_to_anthropic(
        &v(r#"{"messages":[{"role":"user","content":"hi"}],"max_tokens":128,"max_completion_tokens":256}"#),
        "m",
        MAX_TOKENS,
    )
    .unwrap();
    assert_eq!(newer["max_tokens"], 256);
}

#[test]
fn sampling_params_and_stop_sequences_carry_over() {
    let out = request_openai_to_anthropic(
        &v(r#"{"messages":[{"role":"user","content":"hi"}],
             "temperature":0.2,"top_p":0.9,"stream":true,"stop":"END"}"#),
        "m",
        MAX_TOKENS,
    )
    .unwrap();
    assert_eq!(out["temperature"], 0.2);
    assert_eq!(out["top_p"], 0.9);
    assert_eq!(out["stream"], true);
    assert_eq!(out["stop_sequences"], v(r#"["END"]"#));

    let list = request_openai_to_anthropic(
        &v(r#"{"messages":[{"role":"user","content":"hi"}],"stop":["A","B"]}"#),
        "m",
        MAX_TOKENS,
    )
    .unwrap();
    assert_eq!(list["stop_sequences"], v(r#"["A","B"]"#));
}

#[test]
fn tools_become_input_schemas() {
    let out = request_openai_to_anthropic(
        &v(r#"{"messages":[{"role":"user","content":"hi"}],
             "tools":[{"type":"function","function":{
                "name":"get_weather","description":"Look up weather",
                "parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}],
             "tool_choice":{"type":"function","function":{"name":"get_weather"}}}"#),
        "m",
        MAX_TOKENS,
    )
    .unwrap();

    assert_eq!(out["tools"][0]["name"], "get_weather");
    assert_eq!(out["tools"][0]["description"], "Look up weather");
    assert_eq!(
        out["tools"][0]["input_schema"]["properties"]["city"]["type"],
        "string"
    );
    assert_eq!(
        out["tool_choice"],
        v(r#"{"type":"tool","name":"get_weather"}"#)
    );
}

#[test]
fn tool_choice_keywords_map_across() {
    let mk = |choice: &str| {
        request_openai_to_anthropic(
            &v(&format!(
                r#"{{"messages":[{{"role":"user","content":"hi"}}],"tool_choice":{choice}}}"#
            )),
            "m",
            MAX_TOKENS,
        )
        .unwrap()
    };
    assert_eq!(mk(r#""auto""#)["tool_choice"], v(r#"{"type":"auto"}"#));
    assert_eq!(mk(r#""required""#)["tool_choice"], v(r#"{"type":"any"}"#));
    // "none" has no equivalent and is dropped rather than mistranslated.
    assert!(mk(r#""none""#).get("tool_choice").is_none());
}

#[test]
fn a_tool_call_round_trip_survives_translation() {
    let out = request_openai_to_anthropic(
        &v(r#"{"messages":[
            {"role":"user","content":"weather in Paris?"},
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"18C"}]}"#),
        "m",
        MAX_TOKENS,
    )
    .unwrap();

    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
    assert_eq!(msgs[1]["content"][0]["id"], "call_1");
    // Arguments arrive as a JSON string and must become a real object.
    assert_eq!(msgs[1]["content"][0]["input"]["city"], "Paris");
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
}

#[test]
fn consecutive_tool_results_merge_into_one_turn() {
    // Anthropic wants turns to alternate, so two OpenAI `tool` messages have
    // to collapse into a single user turn with two blocks.
    let out = request_openai_to_anthropic(
        &v(r#"{"messages":[
            {"role":"user","content":"go"},
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"a","type":"function","function":{"name":"f","arguments":"{}"}},
                {"id":"b","type":"function","function":{"name":"g","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"a","content":"1"},
            {"role":"tool","tool_call_id":"b","content":"2"}]}"#),
        "m",
        MAX_TOKENS,
    )
    .unwrap();

    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3, "results should merge: {msgs:#?}");
    assert_eq!(msgs[2]["content"].as_array().unwrap().len(), 2);
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
}

#[test]
fn images_translate_both_ways() {
    let out = request_openai_to_anthropic(
        &v(r#"{"messages":[{"role":"user","content":[
            {"type":"text","text":"what is this?"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}},
            {"type":"image_url","image_url":{"url":"https://example.com/a.png"}}]}]}"#),
        "m",
        MAX_TOKENS,
    )
    .unwrap();

    let blocks = out["messages"][0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["source"]["type"], "base64");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    assert_eq!(blocks[1]["source"]["data"], "AAAA");
    assert_eq!(blocks[2]["source"]["type"], "url");

    // And back the other way.
    let back = request_anthropic_to_openai(
        &json!({"messages": [{"role": "user", "content": blocks}]}),
        "gpt-4o-mini",
    )
    .unwrap();
    let parts = back["messages"][0]["content"].as_array().unwrap();
    assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    assert_eq!(parts[2]["image_url"]["url"], "https://example.com/a.png");
}

#[test]
fn a_request_without_messages_is_rejected() {
    assert!(request_openai_to_anthropic(&v(r#"{"model":"m"}"#), "m", MAX_TOKENS).is_err());
    // System-only is not a conversation either.
    assert!(request_openai_to_anthropic(
        &v(r#"{"messages":[{"role":"system","content":"hi"}]}"#),
        "m",
        MAX_TOKENS
    )
    .is_err());
}

// ---------------------------------------------------------------------------
// Requests: Anthropic -> OpenAI
// ---------------------------------------------------------------------------

#[test]
fn system_prompt_becomes_a_system_message() {
    let out = request_anthropic_to_openai(
        &v(
            r#"{"model":"whatever","max_tokens":100,"system":"Be brief.",
             "messages":[{"role":"user","content":"hi"}]}"#,
        ),
        "gpt-4o-mini",
    )
    .unwrap();

    assert_eq!(out["model"], "gpt-4o-mini");
    assert_eq!(out["messages"][0]["role"], "system");
    assert_eq!(out["messages"][0]["content"], "Be brief.");
    assert_eq!(out["messages"][1]["content"], "hi");
    assert_eq!(out["max_tokens"], 100);
}

#[test]
fn a_block_system_prompt_is_flattened() {
    let out = request_anthropic_to_openai(
        &v(
            r#"{"max_tokens":10,"system":[{"type":"text","text":"A"},{"type":"text","text":"B"}],
             "messages":[{"role":"user","content":"hi"}]}"#,
        ),
        "m",
    )
    .unwrap();
    assert_eq!(out["messages"][0]["content"], "AB");
}

#[test]
fn anthropic_tool_use_becomes_openai_tool_calls() {
    let out = request_anthropic_to_openai(
        &v(r#"{"max_tokens":10,"messages":[
            {"role":"user","content":"weather?"},
            {"role":"assistant","content":[
                {"type":"text","text":"checking"},
                {"type":"tool_use","id":"toolu_1","name":"get_weather","input":{"city":"Paris"}}]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_1","content":"18C"}]}]}"#),
        "m",
    )
    .unwrap();

    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"], "checking");
    assert_eq!(msgs[1]["tool_calls"][0]["id"], "toolu_1");
    assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "get_weather");
    // OpenAI wants arguments as a JSON string, not an object.
    let args = msgs[1]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert_eq!(v(args)["city"], "Paris");
    assert_eq!(msgs[2]["role"], "tool");
    assert_eq!(msgs[2]["tool_call_id"], "toolu_1");
    assert_eq!(msgs[2]["content"], "18C");
}

#[test]
fn anthropic_tools_and_choice_map_to_functions() {
    let out = request_anthropic_to_openai(
        &v(
            r#"{"max_tokens":10,"messages":[{"role":"user","content":"hi"}],
             "tools":[{"name":"f","description":"d","input_schema":{"type":"object"}}],
             "tool_choice":{"type":"any"},
             "stop_sequences":["X"],"top_k":40}"#,
        ),
        "m",
    )
    .unwrap();

    assert_eq!(out["tools"][0]["type"], "function");
    assert_eq!(out["tools"][0]["function"]["name"], "f");
    assert_eq!(out["tools"][0]["function"]["parameters"]["type"], "object");
    assert_eq!(out["tool_choice"], "required");
    assert_eq!(out["stop"], v(r#"["X"]"#));
    // top_k has no OpenAI equivalent; it must not be forwarded as junk.
    assert!(out.get("top_k").is_none());
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

#[test]
fn anthropic_response_becomes_a_chat_completion() {
    let out = response_anthropic_to_openai(
        &v(
            r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-haiku-4-5",
             "content":[{"type":"text","text":"Hello there"}],
             "stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":3}}"#,
        ),
        "my-pool",
    );

    assert_eq!(out["id"], "msg_1");
    assert_eq!(out["object"], "chat.completion");
    // The client gets back the name it asked for, not the provider's.
    assert_eq!(out["model"], "my-pool");
    assert_eq!(out["choices"][0]["message"]["content"], "Hello there");
    assert_eq!(out["choices"][0]["message"]["role"], "assistant");
    assert_eq!(out["choices"][0]["finish_reason"], "stop");
    assert_eq!(out["usage"]["prompt_tokens"], 10);
    assert_eq!(out["usage"]["completion_tokens"], 3);
    assert_eq!(out["usage"]["total_tokens"], 13);
}

#[test]
fn anthropic_tool_use_response_becomes_tool_calls() {
    let out = response_anthropic_to_openai(
        &v(r#"{"id":"msg_1","content":[
             {"type":"tool_use","id":"toolu_1","name":"f","input":{"a":1}}],
             "stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}}"#),
        "m",
    );
    assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        out["choices"][0]["message"]["tool_calls"][0]["id"],
        "toolu_1"
    );
    let args = out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert_eq!(v(args)["a"], 1);
    assert!(out["choices"][0]["message"]["content"].is_null());
}

#[test]
fn openai_response_becomes_an_anthropic_message() {
    let out = response_openai_to_anthropic(
        &v(
            r#"{"id":"chatcmpl-1","object":"chat.completion","model":"gpt-4o-mini",
             "choices":[{"index":0,"message":{"role":"assistant","content":"Hi"},
                         "finish_reason":"length"}],
             "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
        ),
        "my-pool",
    );

    assert_eq!(out["type"], "message");
    assert_eq!(out["role"], "assistant");
    assert_eq!(out["model"], "my-pool");
    assert_eq!(out["content"][0]["type"], "text");
    assert_eq!(out["content"][0]["text"], "Hi");
    assert_eq!(out["stop_reason"], "max_tokens");
    assert_eq!(out["usage"]["input_tokens"], 5);
    assert_eq!(out["usage"]["output_tokens"], 2);
}

#[test]
fn stop_reasons_map_in_both_directions() {
    assert_eq!(
        stop_reason_to_finish_reason(Some("end_turn"), false),
        "stop"
    );
    assert_eq!(
        stop_reason_to_finish_reason(Some("max_tokens"), false),
        "length"
    );
    assert_eq!(
        stop_reason_to_finish_reason(Some("tool_use"), false),
        "tool_calls"
    );
    assert_eq!(
        stop_reason_to_finish_reason(Some("stop_sequence"), false),
        "stop"
    );

    assert_eq!(
        finish_reason_to_stop_reason(Some("stop"), false),
        "end_turn"
    );
    assert_eq!(
        finish_reason_to_stop_reason(Some("length"), false),
        "max_tokens"
    );
    assert_eq!(
        finish_reason_to_stop_reason(Some("tool_calls"), true),
        "tool_use"
    );
}

#[test]
fn error_bodies_are_re_dressed_for_the_client() {
    let anthropic_err =
        v(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#);
    let as_openai = error_to_openai(&anthropic_err, "fallback");
    assert_eq!(as_openai["error"]["message"], "Overloaded");
    assert_eq!(as_openai["error"]["type"], "overloaded_error");

    let openai_err = v(r#"{"error":{"message":"Rate limited","type":"rate_limit_error"}}"#);
    let as_anthropic = error_to_anthropic(&openai_err, "fallback");
    assert_eq!(as_anthropic["type"], "error");
    assert_eq!(as_anthropic["error"]["message"], "Rate limited");

    // Already in the right dialect: passed through untouched.
    assert_eq!(error_to_openai(&openai_err, "f"), openai_err);
    assert_eq!(error_to_anthropic(&anthropic_err, "f"), anthropic_err);
}

// ---------------------------------------------------------------------------
// SSE parsing
// ---------------------------------------------------------------------------

#[test]
fn sse_parser_handles_split_chunks() {
    let mut p = SseParser::new();
    // A frame split across three pushes, mid-field and mid-value.
    assert!(p.push(b"event: content_block_de").is_empty());
    assert!(p.push(b"lta\ndata: {\"a\":").is_empty());
    let events = p.push(b"1}\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.as_deref(), Some("content_block_delta"));
    assert_eq!(events[0].data, r#"{"a":1}"#);
}

#[test]
fn sse_parser_handles_crlf_comments_and_multiple_frames() {
    let mut p = SseParser::new();
    let events = p.push(b": keep-alive\r\n\r\ndata: one\r\n\r\ndata: two\n\n");
    let datas: Vec<&str> = events.iter().map(|e| e.data.as_str()).collect();
    assert_eq!(datas, ["one", "two"]);
}

#[test]
fn sse_parser_joins_multi_line_data() {
    let mut p = SseParser::new();
    let events = p.push(b"data: line1\ndata: line2\n\n");
    assert_eq!(events[0].data, "line1\nline2");
}

// ---------------------------------------------------------------------------
// Streaming translation
// ---------------------------------------------------------------------------

/// Collect the `data:` payloads of an OpenAI-shaped stream.
fn openai_stream_chunks(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .split("\n\n")
        .filter_map(|f| f.trim().strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(v)
        .collect()
}

/// Collect (event name, payload) pairs of an Anthropic-shaped stream.
fn anthropic_stream_events(bytes: &[u8]) -> Vec<(String, Value)> {
    String::from_utf8_lossy(bytes)
        .split("\n\n")
        .filter(|f| !f.trim().is_empty())
        .map(|frame| {
            let mut name = String::new();
            let mut data = String::new();
            for line in frame.trim().split('\n') {
                if let Some(rest) = line.strip_prefix("event: ") {
                    name = rest.to_string();
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    data = rest.to_string();
                }
            }
            (name, v(&data))
        })
        .collect()
}

#[test]
fn anthropic_stream_becomes_openai_chunks() {
    let mut t = AnthropicToOpenai::new("my-pool");
    let mut out = Vec::new();
    for frame in [
        r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":9}}}

"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" there"}}

"#,
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}

"#,
        r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}

"#,
        r#"event: message_stop
data: {"type":"message_stop"}

"#,
    ] {
        out.extend(t.push(frame.as_bytes()));
    }
    out.extend(t.finish());

    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(
        text.ends_with("data: [DONE]\n\n"),
        "stream must terminate: {text}"
    );
    assert_eq!(text.matches("[DONE]").count(), 1, "exactly one terminator");

    let chunks = openai_stream_chunks(&out);
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[0]["id"], "msg_1");
    assert_eq!(chunks[0]["model"], "my-pool");

    let content: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "Hello there");

    let last = chunks.last().unwrap();
    assert_eq!(last["choices"][0]["finish_reason"], "stop");
    assert_eq!(last["usage"]["prompt_tokens"], 9);
    assert_eq!(last["usage"]["completion_tokens"], 2);
}

#[test]
fn anthropic_tool_stream_becomes_openai_tool_call_deltas() {
    let mut t = AnthropicToOpenai::new("m");
    let mut out = Vec::new();
    out.extend(t.push(br#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1"}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"Paris\"}"}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}

event: message_stop
data: {"type":"message_stop"}

"#));
    out.extend(t.finish());

    let chunks = openai_stream_chunks(&out);
    let opener = chunks
        .iter()
        .find(|c| c["choices"][0]["delta"]["tool_calls"][0]["id"] == "toolu_1")
        .expect("tool call should be opened");
    assert_eq!(opener["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
    assert_eq!(
        opener["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );

    // The argument fragments must reassemble into the original JSON.
    let args: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str())
        .collect();
    assert_eq!(v(&args)["city"], "Paris");
    assert_eq!(
        chunks.last().unwrap()["choices"][0]["finish_reason"],
        "tool_calls"
    );
}

#[test]
fn openai_stream_becomes_anthropic_events() {
    let mut t = OpenaiToAnthropic::new("my-pool");
    let mut out = Vec::new();
    out.extend(t.push(
        br#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"Hello"}}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":" there"}}]}

data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":2}}

data: [DONE]

"#,
    ));
    out.extend(t.finish());

    let events = anthropic_stream_events(&out);
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ],
        "got {names:?}"
    );

    assert_eq!(events[0].1["message"]["id"], "chatcmpl-1");
    assert_eq!(events[0].1["message"]["model"], "my-pool");
    assert_eq!(events[0].1["message"]["role"], "assistant");

    let text: String = events
        .iter()
        .filter(|(n, _)| n == "content_block_delta")
        .filter_map(|(_, d)| d["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "Hello there");

    let (_, delta) = events.iter().find(|(n, _)| n == "message_delta").unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "end_turn");
    assert_eq!(delta["usage"]["output_tokens"], 2);
}

#[test]
fn openai_tool_stream_becomes_anthropic_tool_use_blocks() {
    let mut t = OpenaiToAnthropic::new("m");
    let mut out = Vec::new();
    out.extend(t.push(
        br#"data: {"id":"c1","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"f","arguments":""}}]}}]}

data: {"id":"c1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\":"}}]}}]}

data: {"id":"c1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}

data: {"id":"c1","choices":[{"delta":{},"finish_reason":"tool_calls"}]}

data: [DONE]

"#,
    ));
    out.extend(t.finish());

    let events = anthropic_stream_events(&out);
    let (_, start) = events
        .iter()
        .find(|(n, _)| n == "content_block_start")
        .expect("a tool_use block should open");
    assert_eq!(start["content_block"]["type"], "tool_use");
    assert_eq!(start["content_block"]["id"], "call_1");
    assert_eq!(start["content_block"]["name"], "f");

    let args: String = events
        .iter()
        .filter(|(n, _)| n == "content_block_delta")
        .filter_map(|(_, d)| d["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(v(&args)["a"], 1);

    let (_, delta) = events.iter().find(|(n, _)| n == "message_delta").unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
}

#[test]
fn two_openai_tool_calls_become_two_blocks() {
    let mut t = OpenaiToAnthropic::new("m");
    let mut out = Vec::new();
    out.extend(t.push(
        br#"data: {"id":"c1","choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"f","arguments":"{}"}}]}}]}

data: {"id":"c1","choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"g","arguments":"{}"}}]}}]}

data: [DONE]

"#,
    ));
    out.extend(t.finish());

    let events = anthropic_stream_events(&out);
    let starts: Vec<&Value> = events
        .iter()
        .filter(|(n, _)| n == "content_block_start")
        .map(|(_, d)| d)
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0]["index"], 0);
    assert_eq!(starts[1]["index"], 1);
    // Every opened block must also be closed.
    let stops = events
        .iter()
        .filter(|(n, _)| n == "content_block_stop")
        .count();
    assert_eq!(stops, 2, "each block must be closed: {events:#?}");
}

#[test]
fn a_truncated_upstream_stream_is_still_closed_properly() {
    // The provider hangs up after one delta and never sends message_stop.
    let mut t = AnthropicToOpenai::new("m");
    let mut out = Vec::new();
    out.extend(t.push(
        br#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}

"#,
    ));
    out.extend(t.finish());
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("partial"));
    assert!(
        text.ends_with("data: [DONE]\n\n"),
        "client must see a terminator: {text}"
    );

    // Same in the other direction: the Anthropic client needs message_stop.
    let mut t2 = OpenaiToAnthropic::new("m");
    let mut out2 = Vec::new();
    out2.extend(t2.push(
        br#"data: {"id":"c","choices":[{"delta":{"content":"partial"}}]}

"#,
    ));
    out2.extend(t2.finish());
    let events = anthropic_stream_events(&out2);
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"content_block_stop"), "{names:?}");
    assert_eq!(names.last(), Some(&"message_stop"), "{names:?}");
}

#[test]
fn a_mid_stream_error_reaches_the_client_in_its_own_dialect() {
    let mut t = AnthropicToOpenai::new("m");
    let mut out = Vec::new();
    out.extend(t.push(
        br#"event: error
data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}

"#,
    ));
    out.extend(t.finish());
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("Overloaded"), "{text}");
    assert!(text.contains("[DONE]"));

    let mut t2 = OpenaiToAnthropic::new("m");
    let mut out2 = Vec::new();
    out2.extend(t2.push(
        br#"data: {"error":{"message":"Rate limited","type":"rate_limit_error"}}

"#,
    ));
    out2.extend(t2.finish());
    let text2 = String::from_utf8_lossy(&out2);
    assert!(text2.contains("event: error"), "{text2}");
    assert!(text2.contains("Rate limited"));
}

#[test]
fn stream_translation_survives_arbitrary_chunk_boundaries() {
    // The same input fed one byte at a time must produce the same output as
    // one big push: chunk boundaries are wherever TCP puts them.
    let input = br#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: message_stop
data: {"type":"message_stop"}

"#;

    let mut whole = AnthropicToOpenai::new("m");
    let mut a = whole.push(input);
    a.extend(whole.finish());

    let mut drip = AnthropicToOpenai::new("m");
    let mut b = Vec::new();
    for byte in input.iter() {
        b.extend(drip.push(&[*byte]));
    }
    b.extend(drip.finish());

    // `created` is a wall-clock second and may differ; compare the payloads.
    let ca = openai_stream_chunks(&a);
    let cb = openai_stream_chunks(&b);
    assert_eq!(ca.len(), cb.len());
    for (x, y) in ca.iter().zip(cb.iter()) {
        assert_eq!(x["choices"], y["choices"]);
    }
}
