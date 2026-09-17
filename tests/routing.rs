//! End-to-end tests: real HTTP, real sockets, mock providers of both dialects.

mod support;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use mini_router::protocol::Protocol::{Anthropic, Openai};
use serde_json::{json, Value};
use support::*;

// ---------------------------------------------------------------------------
// The four-way matrix: either front door reaching either kind of provider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openai_client_to_openai_provider_is_passthrough() {
    let p = start_mock("oai", Openai, &["gpt-4o-mini"]).await;
    let addr = Env::new().provider(&p).start().await;

    let res = openai_chat(addr, "gpt-4o-mini").send().await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.upstream(), Some("oai"));
    assert_eq!(res.model(), Some("gpt-4o-mini"));
    assert_eq!(
        res.translated(),
        None,
        "same dialect must not be translated at all"
    );
    assert_eq!(
        res.json()["choices"][0]["message"]["content"],
        "hello from oai"
    );
}

#[tokio::test]
async fn openai_client_reaches_an_anthropic_provider() {
    let p = start_mock("ant", Anthropic, &["claude-haiku-4-5"]).await;
    let addr = Env::new().provider(&p).start().await;

    let res = Req::post(
        format!("http://{addr}/v1/chat/completions"),
        json!({
            "model": "claude-haiku-4-5",
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "hi"}
            ],
            "temperature": 0.5
        }),
    )
    .send()
    .await;

    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.translated(), Some("anthropic->openai"));

    // The provider received an Anthropic-shaped request.
    let sent = p.last_body();
    assert_eq!(
        sent["system"], "Be brief.",
        "system must move beside turns: {sent}"
    );
    assert!(
        sent["max_tokens"].is_number(),
        "Anthropic requires max_tokens: {sent}"
    );
    assert_eq!(sent["temperature"], 0.5);
    assert_eq!(sent["messages"][0]["content"][0]["text"], "hi");
    assert!(sent.get("messages").unwrap()[0].get("role").unwrap() == "user");

    // The client got an OpenAI-shaped response back.
    let got = res.json();
    assert_eq!(got["object"], "chat.completion");
    assert_eq!(got["choices"][0]["message"]["content"], "hello from ant");
    assert_eq!(got["choices"][0]["finish_reason"], "stop");
    assert_eq!(got["usage"]["prompt_tokens"], 5);
    assert_eq!(got["usage"]["total_tokens"], 8);
}

#[tokio::test]
async fn anthropic_client_to_anthropic_provider_is_passthrough() {
    let p = start_mock("ant", Anthropic, &["claude-haiku-4-5"]).await;
    let addr = Env::new().provider(&p).start().await;

    let res = anthropic_chat(addr, "claude-haiku-4-5").send().await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.upstream(), Some("ant"));
    assert_eq!(res.translated(), None);
    assert_eq!(res.json()["content"][0]["text"], "hello from ant");
    assert_eq!(res.json()["type"], "message");
}

#[tokio::test]
async fn anthropic_client_reaches_an_openai_provider() {
    let p = start_mock("oai", Openai, &["gpt-4o-mini"]).await;
    let addr = Env::new().provider(&p).start().await;

    let res = Req::post(
        format!("http://{addr}/v1/messages"),
        json!({
            "model": "gpt-4o-mini",
            "max_tokens": 128,
            "system": "Be brief.",
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .anthropic()
    .send()
    .await;

    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.translated(), Some("openai->anthropic"));

    // The provider received an OpenAI-shaped request.
    let sent = p.last_body();
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["messages"][0]["content"], "Be brief.");
    assert_eq!(sent["messages"][1]["content"], "hi");
    assert_eq!(sent["max_tokens"], 128);

    // The client got an Anthropic-shaped response back.
    let got = res.json();
    assert_eq!(got["type"], "message");
    assert_eq!(got["role"], "assistant");
    assert_eq!(got["content"][0]["text"], "hello from oai");
    assert_eq!(got["stop_reason"], "end_turn");
    assert_eq!(got["usage"]["input_tokens"], 5);
    assert_eq!(got["usage"]["output_tokens"], 3);
}

// ---------------------------------------------------------------------------
// Streaming, translated and not
// ---------------------------------------------------------------------------

struct Stream {
    body: String,
    first_chunk: Duration,
    total: Duration,
    frames: usize,
}

/// Issue a streaming request and record how the bytes actually arrived.
async fn stream(req: axum::http::Request<axum::body::Body>) -> Stream {
    let started = Instant::now();
    let resp = client().request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream",
        "a streamed answer must keep its content type"
    );
    let mut body = resp.into_body();
    let mut first_chunk = None;
    let mut out = String::new();
    let mut frames = 0;
    while let Some(frame) = body.frame().await {
        if let Some(data) = frame.unwrap().data_ref() {
            first_chunk.get_or_insert_with(|| started.elapsed());
            frames += 1;
            out.push_str(&String::from_utf8_lossy(data));
        }
    }
    Stream {
        body: out,
        first_chunk: first_chunk.expect("at least one chunk"),
        total: started.elapsed(),
        frames,
    }
}

fn streaming_request(
    url: String,
    body: Value,
    anthropic: bool,
) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder()
        .method("POST")
        .uri(url)
        .header("content-type", "application/json");
    if anthropic {
        b = b.header("anthropic-version", "2023-06-01");
    }
    b.body(axum::body::Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn an_anthropic_stream_is_translated_into_openai_chunks_incrementally() {
    let p = start_mock("ant", Anthropic, &["claude-haiku-4-5"]).await;
    let addr = Env::new().provider(&p).start().await;

    let s = stream(streaming_request(
        format!("http://{addr}/v1/chat/completions"),
        json!({"model": "claude-haiku-4-5", "messages": [{"role":"user","content":"hi"}], "stream": true}),
        false,
    ))
    .await;

    // OpenAI-shaped frames, terminated the way an OpenAI SDK expects.
    assert!(s.body.contains("chat.completion.chunk"), "{}", s.body);
    assert!(s.body.trim_end().ends_with("data: [DONE]"), "{}", s.body);
    assert!(
        !s.body.contains("event: "),
        "no Anthropic frames should leak: {}",
        s.body
    );

    let text: String = s
        .body
        .split("\n\n")
        .filter_map(|f| f.trim().strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|c| {
            c["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(text, "hello from ant");

    // Translation must not turn the stream into a buffer.
    assert!(s.frames > 1, "expected several frames, got {}", s.frames);
    assert!(
        s.first_chunk < s.total / 2,
        "first chunk at {:?} but stream ended at {:?}: looks buffered",
        s.first_chunk,
        s.total
    );
}

#[tokio::test]
async fn an_openai_stream_is_translated_into_anthropic_events_incrementally() {
    let p = start_mock("oai", Openai, &["gpt-4o-mini"]).await;
    let addr = Env::new().provider(&p).start().await;

    let s = stream(streaming_request(
        format!("http://{addr}/v1/messages"),
        json!({"model": "gpt-4o-mini", "max_tokens": 64,
               "messages": [{"role":"user","content":"hi"}], "stream": true}),
        true,
    ))
    .await;

    for expected in [
        "event: message_start",
        "event: content_block_start",
        "event: content_block_delta",
        "event: content_block_stop",
        "event: message_delta",
        "event: message_stop",
    ] {
        assert!(
            s.body.contains(expected),
            "missing {expected} in:\n{}",
            s.body
        );
    }
    assert!(
        !s.body.contains("[DONE]"),
        "OpenAI terminator must not leak: {}",
        s.body
    );

    let text: String = s
        .body
        .split("\n\n")
        .filter(|f| f.contains("content_block_delta"))
        .filter_map(|f| f.lines().find_map(|l| l.strip_prefix("data: ")))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|e| e["delta"]["text"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(text, "hello from oai");

    assert!(s.frames > 1, "expected several frames, got {}", s.frames);
    assert!(
        s.first_chunk < s.total / 2,
        "first chunk at {:?} but stream ended at {:?}: looks buffered",
        s.first_chunk,
        s.total
    );
}

#[tokio::test]
async fn a_same_dialect_stream_passes_straight_through() {
    let p = start_mock("oai", Openai, &["gpt-4o-mini"]).await;
    let addr = Env::new().provider(&p).start().await;

    let s = stream(streaming_request(
        format!("http://{addr}/v1/chat/completions"),
        json!({"model": "gpt-4o-mini", "messages": [{"role":"user","content":"hi"}], "stream": true}),
        false,
    ))
    .await;

    assert!(s.body.trim_end().ends_with("data: [DONE]"));
    assert!(s.frames > 1);
    assert!(s.first_chunk < s.total / 2);
}

// ---------------------------------------------------------------------------
// Tool calls across dialects
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_calls_translate_from_anthropic_to_openai() {
    let p = start_mock("ant", Anthropic, &["claude-haiku-4-5"]).await;
    let addr = Env::new().provider(&p).start().await;

    let res = Req::post(
        format!("http://{addr}/v1/chat/completions"),
        json!({
            "model": "claude-haiku-4-5",
            "messages": [{"role": "user", "content": "weather?"}],
            "tools": [{"type": "function", "function": {
                "name": "get_weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}]
        }),
    )
    .send()
    .await;

    // The provider saw an Anthropic tool definition.
    let sent = p.last_body();
    assert_eq!(sent["tools"][0]["name"], "get_weather");
    assert_eq!(
        sent["tools"][0]["input_schema"]["properties"]["city"]["type"],
        "string"
    );

    // The client saw OpenAI tool_calls.
    let got = res.json();
    assert_eq!(got["choices"][0]["finish_reason"], "tool_calls");
    let call = &got["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "get_weather");
    let args: Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "Paris");
}

#[tokio::test]
async fn streamed_tool_calls_translate_from_openai_to_anthropic() {
    let p = start_mock("oai", Openai, &["gpt-4o-mini"]).await;
    let addr = Env::new().provider(&p).start().await;

    let s = stream(streaming_request(
        format!("http://{addr}/v1/messages"),
        json!({"model": "gpt-4o-mini", "max_tokens": 64, "stream": true,
               "messages": [{"role":"user","content":"weather?"}],
               "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}]}),
        true,
    ))
    .await;

    assert!(s.body.contains("\"type\":\"tool_use\""), "{}", s.body);
    assert!(s.body.contains("get_weather"), "{}", s.body);

    let args: String = s
        .body
        .split("\n\n")
        .filter(|f| f.contains("input_json_delta"))
        .filter_map(|f| f.lines().find_map(|l| l.strip_prefix("data: ")))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|e| e["delta"]["partial_json"].as_str().map(str::to_owned))
        .collect();
    let parsed: Value = serde_json::from_str(&args).expect("fragments should reassemble");
    assert_eq!(parsed["city"], "Paris");
    assert!(
        s.body.contains("\"stop_reason\":\"tool_use\""),
        "{}",
        s.body
    );
}

// ---------------------------------------------------------------------------
// Pools
// ---------------------------------------------------------------------------

/// A pool spanning both kinds of provider, in priority order.
async fn pool_setup() -> (Mock, Mock, SocketAddr) {
    let a = start_mock("openai", Openai, &["gpt-4o-mini"]).await;
    let b = start_mock("anthropic", Anthropic, &["claude-haiku-4-5"]).await;
    let addr = Env::new()
        .provider(&a)
        .provider(&b)
        .pool("fast", &[(&a, "gpt-4o-mini"), (&b, "claude-haiku-4-5")])
        .start()
        .await;
    (a, b, addr)
}

#[tokio::test]
async fn a_pool_serves_one_name_from_the_first_member() {
    let (a, b, addr) = pool_setup().await;

    for _ in 0..3 {
        let res = openai_chat(addr, "fast").send().await;
        assert_eq!(res.status, 200, "body: {}", res.body);
        assert_eq!(
            res.upstream(),
            Some("openai"),
            "priority means first member"
        );
        assert_eq!(
            res.model(),
            Some("gpt-4o-mini"),
            "the pool picks the provider's model id"
        );
    }
    assert_eq!(a.hits(), 3);
    assert_eq!(
        b.hits(),
        0,
        "the second member is the spillover path, not a peer"
    );
}

#[tokio::test]
async fn a_pool_spills_to_a_provider_of_the_other_dialect() {
    let (a, b, addr) = pool_setup().await;
    a.set_status(500);

    let res = openai_chat(addr, "fast").send().await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.upstream(), Some("anthropic"));
    assert_eq!(res.model(), Some("claude-haiku-4-5"));
    // Reaching the Anthropic member from an OpenAI client means translating.
    assert_eq!(res.translated(), Some("anthropic->openai"));
    assert_eq!(
        res.json()["choices"][0]["message"]["content"],
        "hello from anthropic"
    );
    assert_eq!(a.hits(), 1, "the failing member was tried first");
    assert_eq!(b.hits(), 1);
}

#[tokio::test]
async fn an_anthropic_client_can_use_the_same_pool() {
    let (a, _b, addr) = pool_setup().await;

    let res = anthropic_chat(addr, "fast").send().await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.upstream(), Some("openai"));
    assert_eq!(res.translated(), Some("openai->anthropic"));
    assert_eq!(res.json()["content"][0]["text"], "hello from openai");
    // The OpenAI provider was asked in its own dialect.
    assert!(a.last_body()["messages"].is_array());
    assert_eq!(a.last_body()["model"], "gpt-4o-mini");
}

#[tokio::test]
async fn a_pool_can_override_the_balancing_strategy() {
    let a = start_mock("a", Openai, &["m"]).await;
    let b = start_mock("b", Openai, &["m"]).await;
    let addr = Env::new()
        .provider(&a)
        .provider(&b)
        .pool("spread", &[(&a, "m"), (&b, "m")])
        .set("MINI_ROUTER_POOL_SPREAD_STRATEGY", "round-robin")
        .start()
        .await;

    for _ in 0..10 {
        assert_eq!(openai_chat(addr, "spread").send().await.status, 200);
    }
    assert_eq!(a.hits(), 5);
    assert_eq!(b.hits(), 5);
}

// ---------------------------------------------------------------------------
// Spillover on *any* error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_kind_of_failure_spills_over() {
    // Not just rate limits and 5xx: a bad key, a missing model and a refused
    // request all mean "this provider is not going to serve it, try the next".
    for status in [400u16, 401, 403, 404, 409, 422, 429, 500, 502, 503, 529] {
        let a = start_mock("first", Openai, &["m"]).await;
        let b = start_mock("second", Openai, &["m"]).await;
        a.set_status(status);
        let addr = Env::new()
            .provider(&a)
            .provider(&b)
            .pool("p", &[(&a, "m"), (&b, "m")])
            .start()
            .await;

        let res = openai_chat(addr, "p").send().await;
        assert_eq!(
            res.status, 200,
            "status {status} should have spilled over: {}",
            res.body
        );
        assert_eq!(res.upstream(), Some("second"), "status {status}");
        assert_eq!(a.hits(), 1, "status {status}");
        assert_eq!(b.hits(), 1, "status {status}");
    }
}

#[tokio::test]
async fn a_refused_connection_spills_over() {
    let good = start_mock("good", Openai, &["m"]).await;
    // Port 1 on loopback refuses immediately.
    let addr = Env::new()
        .set("MINI_ROUTER_PROVIDER_DEAD_URL", "http://127.0.0.1:1/v1")
        .provider(&good)
        .set("MINI_ROUTER_POOL_P", &format!("dead:m,{}:m", good.name))
        .start()
        .await;

    let res = openai_chat(addr, "p").send().await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.upstream(), Some("good"));
}

#[tokio::test]
async fn status_list_spillover_can_be_narrowed() {
    let a = start_mock("first", Openai, &["m"]).await;
    let b = start_mock("second", Openai, &["m"]).await;
    a.set_status(400);
    let addr = Env::new()
        .set("MINI_ROUTER_SPILLOVER", "status-list")
        .set("MINI_ROUTER_RETRY_ON_STATUS", "503")
        .provider(&a)
        .provider(&b)
        .pool("p", &[(&a, "m"), (&b, "m")])
        .start()
        .await;

    // 400 is not in the list, so it is the client's answer.
    let res = openai_chat(addr, "p").send().await;
    assert_eq!(res.status, 400);
    assert_eq!(
        b.hits(),
        0,
        "narrowed spillover must not try the next member"
    );
}

#[tokio::test]
async fn the_last_providers_own_error_reaches_the_client() {
    let a = start_mock("first", Openai, &["m"]).await;
    let b = start_mock("second", Anthropic, &["m"]).await;
    a.set_status(500);
    b.set_status(429);
    let addr = Env::new()
        .provider(&a)
        .provider(&b)
        .pool("p", &[(&a, "m"), (&b, "m")])
        .start()
        .await;

    let res = openai_chat(addr, "p").send().await;
    // The status and the message come from the provider, not from us.
    assert_eq!(res.status, 429);
    let body = res.json();
    assert_eq!(
        body["error"]["message"], "second says no",
        "the provider's own explanation is more useful than ours: {}",
        res.body
    );
    // ...but shaped for the dialect the client is speaking.
    assert!(
        body.get("type").is_none(),
        "must not be Anthropic-shaped: {}",
        res.body
    );
}

#[tokio::test]
async fn an_anthropic_client_gets_errors_in_its_own_dialect() {
    let p = start_mock("oai", Openai, &["m"]).await;
    p.set_status(500);
    let addr = Env::new().provider(&p).start().await;

    let res = anthropic_chat(addr, "m").send().await;
    assert_eq!(res.status, 500);
    let body = res.json();
    assert_eq!(body["type"], "error", "body: {}", res.body);
    assert_eq!(body["error"]["message"], "oai says no");

    // And a router-generated error, too.
    let missing = anthropic_chat(addr, "no-such-model").send().await;
    assert_eq!(missing.status, 503);
    assert_eq!(missing.json()["type"], "error");
    assert_eq!(missing.json()["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn a_rate_limited_provider_is_parked_for_the_time_it_asked_for() {
    let a = start_mock("limited", Openai, &["m"]).await;
    let b = start_mock("spare", Openai, &["m"]).await;
    a.set_status(429);
    a.set_retry_after(60);
    let addr = Env::new()
        .provider(&a)
        .provider(&b)
        .pool("p", &[(&a, "m"), (&b, "m")])
        .start()
        .await;

    assert_eq!(openai_chat(addr, "p").send().await.status, 200);
    assert_eq!(a.hits(), 1);

    // It said 60 seconds, so it should not be asked again in this test.
    for _ in 0..3 {
        let res = openai_chat(addr, "p").send().await;
        assert_eq!(res.status, 200);
        assert_eq!(res.upstream(), Some("spare"));
    }
    assert_eq!(a.hits(), 1, "a parked provider must not be retried");

    let admin = get(&format!("http://{addr}/admin/upstreams")).await;
    let limited = admin.json()["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["name"] == "limited")
        .unwrap()
        .clone();
    assert_eq!(limited["health"], "down");
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

#[tokio::test]
async fn each_provider_gets_its_own_credential_in_its_own_header() {
    let a = start_mock("openai", Openai, &["m"]).await;
    let b = start_mock("anthropic", Anthropic, &["m"]).await;
    let addr = Env::new()
        .set("MINI_ROUTER_REQUIRE_AUTH", "true")
        .set("MINI_ROUTER_API_KEYS", "sk-client")
        .provider(&a)
        .provider(&b)
        .pool("p", &[(&a, "m"), (&b, "m")])
        .start()
        .await;

    assert_eq!(
        openai_chat(addr, "p")
            .bearer("sk-client")
            .send()
            .await
            .status,
        200
    );
    // OpenAI gets a bearer token, and never the client's.
    assert_eq!(
        a.last_header("authorization").as_deref(),
        Some("Bearer sk-openai-secret")
    );
    assert_eq!(a.last_header("x-api-key"), None);

    a.set_status(500);
    assert_eq!(
        openai_chat(addr, "p")
            .bearer("sk-client")
            .send()
            .await
            .status,
        200
    );
    // Anthropic gets x-api-key plus a version, and no bearer token.
    assert_eq!(
        b.last_header("x-api-key").as_deref(),
        Some("sk-anthropic-secret")
    );
    assert_eq!(
        b.last_header("anthropic-version").as_deref(),
        Some("2023-06-01")
    );
    assert_eq!(b.last_header("authorization"), None);
}

#[tokio::test]
async fn client_auth_accepts_either_sdks_header() {
    let p = start_mock("oai", Openai, &["m"]).await;
    let addr = Env::new()
        .set("MINI_ROUTER_REQUIRE_AUTH", "true")
        .set("MINI_ROUTER_API_KEYS", "sk-client")
        .provider(&p)
        .start()
        .await;

    // An OpenAI SDK sends a bearer token.
    assert_eq!(
        openai_chat(addr, "m")
            .bearer("sk-client")
            .send()
            .await
            .status,
        200
    );
    // An Anthropic SDK sends x-api-key.
    assert_eq!(
        anthropic_chat(addr, "m")
            .header("x-api-key", "sk-client")
            .send()
            .await
            .status,
        200
    );

    assert_eq!(openai_chat(addr, "m").send().await.status, 401);
    assert_eq!(
        openai_chat(addr, "m")
            .bearer("sk-wrong")
            .send()
            .await
            .status,
        401
    );
    assert_eq!(
        p.hits(),
        2,
        "unauthorized requests must not reach a provider"
    );
}

// ---------------------------------------------------------------------------
// Catalogue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_catalogue_is_served_in_both_dialects() {
    let (_a, _b, addr) = pool_setup().await;

    let oai = get(&format!("http://{addr}/v1/models")).await;
    assert_eq!(oai.status, 200);
    let body = oai.json();
    assert_eq!(body["object"], "list");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["claude-haiku-4-5", "fast", "gpt-4o-mini"]);

    let pool_entry = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "fast")
        .unwrap();
    assert_eq!(pool_entry["owned_by"], "mini-router-pool");
    assert_eq!(pool_entry["pool_members"][0], "openai:gpt-4o-mini");
    assert_eq!(pool_entry["pool_members"][1], "anthropic:claude-haiku-4-5");

    // The same catalogue, in Anthropic's shape.
    let ant = Req::get(format!("http://{addr}/v1/models"))
        .anthropic()
        .send()
        .await;
    let body = ant.json();
    assert!(body.get("object").is_none());
    assert_eq!(body["has_more"], false);
    assert_eq!(body["data"][0]["type"], "model");
    assert!(body["data"][0]["created_at"]
        .as_str()
        .unwrap()
        .ends_with('Z'));
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["claude-haiku-4-5", "fast", "gpt-4o-mini"]);

    // Explicit prefixes force a dialect regardless of headers.
    let forced = Req::get(format!("http://{addr}/anthropic/v1/models"))
        .send()
        .await;
    assert_eq!(forced.json()["data"][0]["type"], "model");

    // A single model, and a 404 for one nobody has.
    let one = get(&format!("http://{addr}/v1/models/fast")).await;
    assert_eq!(one.status, 200);
    assert_eq!(one.json()["id"], "fast");
    assert_eq!(
        get(&format!("http://{addr}/v1/models/nope")).await.status,
        404
    );
}

#[tokio::test]
async fn aliases_resolve_onto_pools_and_models() {
    let (a, _b, addr_unused) = pool_setup().await;
    let _ = addr_unused;
    let b = start_mock("anthropic2", Anthropic, &["claude-haiku-4-5"]).await;
    let addr = Env::new()
        .provider(&a)
        .provider(&b)
        .pool("fast", &[(&a, "gpt-4o-mini")])
        .set(
            "MINI_ROUTER_ALIASES",
            "gpt-3.5-turbo=fast,dangling=nothing-serves-this",
        )
        .start()
        .await;

    // An app hard-coded to gpt-3.5-turbo lands on the pool.
    let res = openai_chat(addr, "gpt-3.5-turbo").send().await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.upstream(), Some("openai"));
    assert_eq!(res.model(), Some("gpt-4o-mini"));

    let ids: Vec<String> = get(&format!("http://{addr}/v1/models")).await.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(ids.contains(&"gpt-3.5-turbo".to_string()));
    assert!(
        !ids.contains(&"dangling".to_string()),
        "an alias pointing at nothing must not be advertised: {ids:?}"
    );
}

// ---------------------------------------------------------------------------
// Everything else
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_untranslatable_endpoint_only_reaches_its_own_dialect() {
    let oai = start_mock("oai", Openai, &["m"]).await;
    let ant = start_mock("ant", Anthropic, &["m"]).await;
    let addr = Env::new().provider(&ant).provider(&oai).start().await;

    // Embeddings have no Anthropic equivalent, so the Anthropic provider must
    // be skipped rather than sent something it cannot answer.
    let res = Req::post(
        format!("http://{addr}/v1/embeddings"),
        json!({"model": "m", "input": "hello"}),
    )
    .send()
    .await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.upstream(), Some("oai"));
    assert_eq!(ant.hits(), 0);
}

#[tokio::test]
async fn max_concurrency_is_enforced_per_provider() {
    let p = start_mock("slow", Openai, &["m"]).await;
    p.set_delay(Duration::from_millis(120));
    let addr = Env::new()
        .provider(&p)
        .set("MINI_ROUTER_PROVIDER_SLOW_MAX_CONCURRENCY", "1")
        .start()
        .await;

    let tasks: Vec<_> = (0..4)
        .map(|_| tokio::spawn(async move { openai_chat(addr, "m").send().await }))
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap().status, 200);
    }
    assert_eq!(p.hits(), 4);
    assert_eq!(
        p.peak_inflight(),
        1,
        "max_concurrency = 1 must serialise requests"
    );
}

#[tokio::test]
async fn health_probing_discovers_models_and_drops_dead_providers() {
    let p = start_mock("ant", Anthropic, &["discovered-claude"]).await;
    let addr = Env::new()
        .provider(&p)
        // This is the one test that wants live probing.
        .set("MINI_ROUTER_HEALTH_INTERVAL_SECS", "1")
        .set("MINI_ROUTER_HEALTH_TIMEOUT_SECS", "1")
        .set("MINI_ROUTER_FAILURE_THRESHOLD", "1")
        .set("MINI_ROUTER_SUCCESS_THRESHOLD", "1")
        .start()
        .await;

    let models_url = format!("http://{addr}/v1/models");
    wait_until("model discovery", Duration::from_secs(10), || {
        let url = models_url.clone();
        async move { get(&url).await.body.contains("discovered-claude") }
    })
    .await;

    p.set_health_status(500);
    let ready_url = format!("http://{addr}/readyz");
    wait_until("provider to drop out", Duration::from_secs(10), || {
        let url = ready_url.clone();
        async move { get(&url).await.status == 503 }
    })
    .await;

    let admin = get(&format!("http://{addr}/admin/upstreams")).await;
    assert_eq!(admin.json()["upstreams"][0]["health"], "down");
    assert_eq!(admin.json()["upstreams"][0]["protocol"], "anthropic");
}

#[tokio::test]
async fn admin_and_metrics_report_traffic_and_translation() {
    let (_a, b, addr) = pool_setup().await;

    assert_eq!(get(&format!("http://{addr}/healthz")).await.status, 200);
    assert_eq!(get(&format!("http://{addr}/readyz")).await.status, 200);

    // Two passthrough, one translated.
    assert_eq!(openai_chat(addr, "fast").send().await.status, 200);
    assert_eq!(openai_chat(addr, "gpt-4o-mini").send().await.status, 200);
    assert_eq!(
        openai_chat(addr, "claude-haiku-4-5").send().await.status,
        200
    );
    assert_eq!(b.hits(), 1);

    let m = get(&format!("http://{addr}/metrics")).await;
    assert_eq!(m.status, 200);
    assert!(
        m.body.contains("mini_router_requests_total 3"),
        "{}",
        m.body
    );
    assert!(
        m.body.contains("mini_router_translated_total 1"),
        "{}",
        m.body
    );
    assert!(m
        .body
        .contains(r#"mini_router_responses_total{class="2xx"} 3"#));
    assert!(m
        .body
        .contains(r#"mini_router_upstream_info{upstream="anthropic",protocol="anthropic"} 1"#));
    assert!(m
        .body
        .contains(r#"mini_router_upstream_info{upstream="openai",protocol="openai"} 1"#));

    let admin = get(&format!("http://{addr}/admin/upstreams")).await;
    let body = admin.json();
    assert_eq!(body["strategy"], "priority");
    assert_eq!(body["spillover"], "any-error");
    assert_eq!(body["pools"][0]["name"], "fast");
    assert_eq!(body["pools"][0]["members"][0]["upstream"], "openai");
    assert_eq!(body["pools"][0]["members"][0]["available"], true);
}

#[tokio::test]
async fn unknown_paths_and_oversized_bodies_are_rejected() {
    let p = start_mock("oai", Openai, &["m"]).await;
    let addr = Env::new()
        .provider(&p)
        .set("MINI_ROUTER_MAX_BODY_BYTES", "1024")
        .start()
        .await;

    // Not an API path: must 404 rather than be forwarded to a paid provider.
    let unknown = get(&format!("http://{addr}/not-a-thing")).await;
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.json()["error"]["type"], "not_found_error");

    let huge = Req::post(
        format!("http://{addr}/v1/chat/completions"),
        json!({"model": "m", "messages": [{"role": "user", "content": "x".repeat(4096)}]}),
    )
    .send()
    .await;
    assert_eq!(huge.status, 413);
    assert_eq!(p.hits(), 0);
}

// ---------------------------------------------------------------------------
// Configured entirely from the environment (the docker-compose path)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_router_configured_only_by_environment_variables_works() {
    let a = start_mock("openai", Openai, &["gpt-4o-mini"]).await;
    let b = start_mock("anthropic", Anthropic, &["claude-haiku-4-5"]).await;

    // Spelled out the way a compose file would, rather than through the
    // builder, so the exact variable names stay covered.
    let addr = Env::new()
        .set("MINI_ROUTER_PROVIDER_OPENAI_URL", &a.base_url())
        .set("MINI_ROUTER_PROVIDER_OPENAI_API_KEY", "sk-openai-secret")
        .set("MINI_ROUTER_PROVIDER_ANTHROPIC_URL", &b.base_url())
        .set("MINI_ROUTER_PROVIDER_ANTHROPIC_PROTOCOL", "anthropic")
        .set(
            "MINI_ROUTER_PROVIDER_ANTHROPIC_API_KEY",
            "sk-anthropic-secret",
        )
        .set(
            "MINI_ROUTER_POOL_FAST",
            "openai:gpt-4o-mini,anthropic:claude-haiku-4-5",
        )
        .set("MINI_ROUTER_ALIASES", "gpt-3.5-turbo=fast")
        .set("MINI_ROUTER_REQUIRE_AUTH", "true")
        .set("MINI_ROUTER_API_KEYS", "sk-client")
        .start()
        .await;

    // The pool works, from the first member.
    let res = openai_chat(addr, "fast").bearer("sk-client").send().await;
    assert_eq!(res.status, 200, "body: {}", res.body);
    assert_eq!(res.upstream(), Some("openai"));
    assert_eq!(res.model(), Some("gpt-4o-mini"));

    // The alias works.
    let aliased = openai_chat(addr, "gpt-3.5-turbo")
        .bearer("sk-client")
        .send()
        .await;
    assert_eq!(aliased.status, 200);
    assert_eq!(aliased.upstream(), Some("openai"));

    // Auth from the environment is enforced.
    assert_eq!(openai_chat(addr, "fast").send().await.status, 401);

    // Spillover reaches the other dialect, translating on the way.
    a.set_status(500);
    let spilled = anthropic_chat(addr, "fast")
        .header("x-api-key", "sk-client")
        .send()
        .await;
    assert_eq!(spilled.status, 200, "body: {}", spilled.body);
    assert_eq!(spilled.upstream(), Some("anthropic"));
    assert_eq!(spilled.json()["content"][0]["text"], "hello from anthropic");
    // Each provider still got its own credential.
    assert_eq!(
        b.last_header("x-api-key").as_deref(),
        Some("sk-anthropic-secret")
    );
}

#[tokio::test]
async fn admin_and_metrics_can_be_switched_off_from_the_environment() {
    let p = start_mock("oai", Openai, &["m"]).await;
    let addr = Env::new()
        .provider(&p)
        .set("MINI_ROUTER_ADMIN", "false")
        .set("MINI_ROUTER_METRICS", "false")
        .start()
        .await;

    // Gone, not merely unauthenticated.
    assert_eq!(get(&format!("http://{addr}/metrics")).await.status, 404);
    assert_eq!(
        get(&format!("http://{addr}/admin/upstreams")).await.status,
        404
    );

    // What a container runtime probes stays, and so does the actual job.
    assert_eq!(get(&format!("http://{addr}/healthz")).await.status, 200);
    assert_eq!(get(&format!("http://{addr}/readyz")).await.status, 200);
    assert_eq!(openai_chat(addr, "m").send().await.status, 200);
}
