//! Mock provider APIs that speak either dialect, plus helpers to drive the
//! router over real HTTP.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{json, Value};

use mini_router::config::Config;
use mini_router::protocol::Protocol;
use mini_router::state::AppState;

/// Knobs a test can turn while a mock provider is running.
#[derive(Debug, Default)]
pub struct MockControl {
    /// Status returned by the completion endpoint.
    pub status: AtomicUsize,
    /// Status returned by the models endpoint.
    pub health_status: AtomicUsize,
    /// Artificial think time before answering.
    pub delay_ms: AtomicU64,
    /// Seconds to advertise in a `Retry-After` header, 0 for none.
    pub retry_after: AtomicU64,
    pub hits: AtomicUsize,
    pub inflight: AtomicUsize,
    pub peak_inflight: AtomicUsize,
    /// The last request body this provider received, verbatim.
    pub last_body: Mutex<Option<Value>>,
    pub last_headers: Mutex<Option<HeaderMap>>,
}

#[derive(Clone)]
pub struct Mock {
    pub name: &'static str,
    pub protocol: Protocol,
    pub addr: SocketAddr,
    pub models: Vec<String>,
    pub ctl: Arc<MockControl>,
}

impl Mock {
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }
    pub fn hits(&self) -> usize {
        self.ctl.hits.load(Ordering::Relaxed)
    }
    pub fn peak_inflight(&self) -> usize {
        self.ctl.peak_inflight.load(Ordering::Relaxed)
    }
    pub fn set_status(&self, status: u16) {
        self.ctl.status.store(status as usize, Ordering::Relaxed);
    }
    pub fn set_health_status(&self, status: u16) {
        self.ctl
            .health_status
            .store(status as usize, Ordering::Relaxed);
    }
    pub fn set_delay(&self, d: Duration) {
        self.ctl
            .delay_ms
            .store(d.as_millis() as u64, Ordering::Relaxed);
    }
    pub fn set_retry_after(&self, secs: u64) {
        self.ctl.retry_after.store(secs, Ordering::Relaxed);
    }
    /// The body this provider was last sent. Panics if it was never called.
    pub fn last_body(&self) -> Value {
        self.ctl
            .last_body
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| panic!("{} was never called", self.name))
    }
    pub fn last_model(&self) -> Option<String> {
        self.ctl
            .last_body
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|b| b.get("model").and_then(Value::as_str).map(str::to_owned))
    }
    pub fn last_header(&self, name: &str) -> Option<String> {
        self.ctl
            .last_headers
            .lock()
            .unwrap()
            .as_ref()?
            .get(name)?
            .to_str()
            .ok()
            .map(str::to_owned)
    }
    /// The config block that points the router at this provider.
    pub fn upstream_toml(&self) -> String {
        format!(
            r#"
            [[upstream]]
            name = "{}"
            url = "{}"
            protocol = "{}"
            api_key = "sk-{}-secret"
            models = [{}]
            "#,
            self.name,
            self.base_url(),
            self.protocol,
            self.name,
            self.models
                .iter()
                .map(|m| format!("{m:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[derive(Clone)]
struct MockState {
    name: &'static str,
    protocol: Protocol,
    models: Vec<String>,
    ctl: Arc<MockControl>,
}

/// Start a mock provider on an ephemeral port.
pub async fn start_mock(name: &'static str, protocol: Protocol, models: &[&str]) -> Mock {
    let ctl = Arc::new(MockControl::default());
    ctl.status.store(200, Ordering::Relaxed);
    ctl.health_status.store(200, Ordering::Relaxed);
    let models: Vec<String> = models.iter().map(|m| m.to_string()).collect();

    let state = MockState {
        name,
        protocol,
        models: models.clone(),
        ctl: ctl.clone(),
    };

    let chat_path = match protocol {
        Protocol::Openai => "/v1/chat/completions",
        Protocol::Anthropic => "/v1/messages",
    };
    let app = Router::new()
        .route("/v1/models", axum::routing::get(mock_models))
        .route(chat_path, axum::routing::post(mock_chat))
        .route("/v1/embeddings", axum::routing::post(mock_chat))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Mock {
        name,
        protocol,
        addr,
        models,
        ctl,
    }
}

async fn mock_models(State(s): State<MockState>) -> Response {
    let status = StatusCode::from_u16(s.ctl.health_status.load(Ordering::Relaxed) as u16)
        .unwrap_or(StatusCode::OK);
    if !status.is_success() {
        return (status, "unhealthy").into_response();
    }
    let body = match s.protocol {
        Protocol::Openai => json!({
            "object": "list",
            "data": s.models.iter()
                .map(|m| json!({"id": m, "object": "model", "owned_by": s.name}))
                .collect::<Vec<_>>(),
        }),
        Protocol::Anthropic => json!({
            "data": s.models.iter()
                .map(|m| json!({"type": "model", "id": m, "display_name": m}))
                .collect::<Vec<_>>(),
            "has_more": false,
        }),
    };
    axum::Json(body).into_response()
}

async fn mock_chat(State(s): State<MockState>, headers: HeaderMap, body: Body) -> Response {
    s.ctl.hits.fetch_add(1, Ordering::Relaxed);
    let now = s.ctl.inflight.fetch_add(1, Ordering::Relaxed) + 1;
    s.ctl.peak_inflight.fetch_max(now, Ordering::Relaxed);
    let _guard = InflightGuard(s.ctl.clone());

    *s.ctl.last_headers.lock().unwrap() = Some(headers);
    let bytes = body
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    *s.ctl.last_body.lock().unwrap() = Some(parsed.clone());

    let delay = s.ctl.delay_ms.load(Ordering::Relaxed);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }

    let status =
        StatusCode::from_u16(s.ctl.status.load(Ordering::Relaxed) as u16).unwrap_or(StatusCode::OK);
    if !status.is_success() {
        let body = match s.protocol {
            Protocol::Openai => json!({
                "error": {"message": format!("{} says no", s.name), "type": "server_error"}
            }),
            Protocol::Anthropic => json!({
                "type": "error",
                "error": {"type": "overloaded_error", "message": format!("{} says no", s.name)}
            }),
        };
        let retry = s.ctl.retry_after.load(Ordering::Relaxed);
        let mut resp = (status, axum::Json(body)).into_response();
        if retry > 0 {
            resp.headers_mut()
                .insert("retry-after", retry.to_string().parse().unwrap());
        }
        return resp;
    }

    let streaming = parsed
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let wants_tool = parsed.get("tools").is_some();
    let model = parsed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();

    if streaming {
        return stream_response(s.protocol, s.name, wants_tool);
    }

    let body = match s.protocol {
        Protocol::Openai => {
            let mut message =
                json!({"role": "assistant", "content": format!("hello from {}", s.name)});
            if wants_tool {
                message["content"] = Value::Null;
                message["tool_calls"] = json!([{
                    "id": "call_mock",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                }]);
            }
            json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "model": model,
                "choices": [{
                    "index": 0,
                    "message": message,
                    "finish_reason": if wants_tool { "tool_calls" } else { "stop" },
                }],
                "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
            })
        }
        Protocol::Anthropic => {
            let content = if wants_tool {
                json!([{
                    "type": "tool_use",
                    "id": "toolu_mock",
                    "name": "get_weather",
                    "input": {"city": "Paris"},
                }])
            } else {
                json!([{"type": "text", "text": format!("hello from {}", s.name)}])
            };
            json!({
                "id": "msg_mock",
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": content,
                "stop_reason": if wants_tool { "tool_use" } else { "end_turn" },
                "usage": {"input_tokens": 5, "output_tokens": 3},
            })
        }
    };
    axum::Json(body).into_response()
}

struct InflightGuard(Arc<MockControl>);
impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

pub const SSE_GAP: Duration = Duration::from_millis(60);

/// A streamed answer in the provider's own dialect, spaced out over time so a
/// test can tell streaming from buffering.
fn stream_response(protocol: Protocol, name: &'static str, wants_tool: bool) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(1);
    tokio::spawn(async move {
        let frames = match (protocol, wants_tool) {
            (Protocol::Openai, false) => openai_text_frames(name),
            (Protocol::Openai, true) => openai_tool_frames(),
            (Protocol::Anthropic, false) => anthropic_text_frames(name),
            (Protocol::Anthropic, true) => anthropic_tool_frames(),
        };
        for frame in frames {
            if tx.send(Ok(axum::body::Bytes::from(frame))).await.is_err() {
                return;
            }
            tokio::time::sleep(SSE_GAP).await;
        }
    });
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn openai_text_frames(name: &'static str) -> Vec<String> {
    let mut out = vec![format!(
        "data: {}\n\n",
        json!({"id":"chatcmpl-mock","object":"chat.completion.chunk","model":"m",
               "choices":[{"index":0,"delta":{"role":"assistant","content":""}}]})
    )];
    for word in ["hello ", "from ", name] {
        out.push(format!(
            "data: {}\n\n",
            json!({"id":"chatcmpl-mock","object":"chat.completion.chunk","model":"m",
                   "choices":[{"index":0,"delta":{"content":word}}]})
        ));
    }
    out.push(format!(
        "data: {}\n\n",
        json!({"id":"chatcmpl-mock","object":"chat.completion.chunk","model":"m",
               "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
               "usage":{"prompt_tokens":5,"completion_tokens":3}})
    ));
    out.push("data: [DONE]\n\n".to_string());
    out
}

fn openai_tool_frames() -> Vec<String> {
    vec![
        format!(
            "data: {}\n\n",
            json!({"id":"chatcmpl-mock","choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_mock","type":"function",
                 "function":{"name":"get_weather","arguments":""}}]}}]})
        ),
        format!(
            "data: {}\n\n",
            json!({"id":"chatcmpl-mock","choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"{\"city\":"}}]}}]})
        ),
        format!(
            "data: {}\n\n",
            json!({"id":"chatcmpl-mock","choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"\"Paris\"}"}}]}}]})
        ),
        format!(
            "data: {}\n\n",
            json!({"id":"chatcmpl-mock","choices":[{"delta":{},"finish_reason":"tool_calls"}]})
        ),
        "data: [DONE]\n\n".to_string(),
    ]
}

fn anthropic_frame(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn anthropic_text_frames(name: &'static str) -> Vec<String> {
    let mut out = vec![
        anthropic_frame(
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_mock","type":"message",
                   "role":"assistant","model":"m","content":[],
                   "usage":{"input_tokens":5,"output_tokens":0}}}),
        ),
        anthropic_frame(
            "content_block_start",
            json!({"type":"content_block_start","index":0,
                   "content_block":{"type":"text","text":""}}),
        ),
    ];
    for word in ["hello ", "from ", name] {
        out.push(anthropic_frame(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,
                   "delta":{"type":"text_delta","text":word}}),
        ));
    }
    out.push(anthropic_frame(
        "content_block_stop",
        json!({"type":"content_block_stop","index":0}),
    ));
    out.push(anthropic_frame(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
               "usage":{"output_tokens":3}}),
    ));
    out.push(anthropic_frame(
        "message_stop",
        json!({"type":"message_stop"}),
    ));
    out
}

fn anthropic_tool_frames() -> Vec<String> {
    vec![
        anthropic_frame(
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_mock","role":"assistant"}}),
        ),
        anthropic_frame(
            "content_block_start",
            json!({"type":"content_block_start","index":0,
                   "content_block":{"type":"tool_use","id":"toolu_mock","name":"get_weather"}}),
        ),
        anthropic_frame(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,
                   "delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}),
        ),
        anthropic_frame(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,
                   "delta":{"type":"input_json_delta","partial_json":"\"Paris\"}"}}),
        ),
        anthropic_frame(
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        anthropic_frame(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
        ),
        anthropic_frame("message_stop", json!({"type":"message_stop"})),
    ]
}

// ---------------------------------------------------------------------------
// Driving the router
// ---------------------------------------------------------------------------

/// Start the router under test. Returns its address.
pub async fn start_router(config_toml: &str) -> SocketAddr {
    let cfg = Config::from_toml(config_toml).expect("test config should be valid");
    let state = Arc::new(AppState::new(cfg));
    mini_router::health::spawn_probes(state.clone());
    let app = mini_router::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

pub type TestClient = Client<HttpConnector, Body>;

pub fn client() -> TestClient {
    Client::builder(TokioExecutor::new()).build_http()
}

pub struct Res {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Res {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("body is not json ({e}): {}", self.body))
    }
    pub fn upstream(&self) -> Option<&str> {
        self.header(mini_router::proxy::UPSTREAM_HEADER)
    }
    pub fn model(&self) -> Option<&str> {
        self.header(mini_router::proxy::MODEL_HEADER)
    }
    pub fn translated(&self) -> Option<&str> {
        self.header(mini_router::proxy::TRANSLATED_HEADER)
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

pub struct Req {
    method: &'static str,
    url: String,
    body: Option<Value>,
    headers: Vec<(String, String)>,
}

impl Req {
    pub fn post(url: impl Into<String>, body: Value) -> Self {
        Self {
            method: "POST",
            url: url.into(),
            body: Some(body),
            headers: vec![],
        }
    }
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET",
            url: url.into(),
            body: None,
            headers: vec![],
        }
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
    /// Marks the request as coming from an Anthropic SDK.
    pub fn anthropic(self) -> Self {
        self.header("anthropic-version", "2023-06-01")
    }
    pub fn bearer(self, key: &str) -> Self {
        self.header("authorization", &format!("Bearer {key}"))
    }

    pub async fn send(self) -> Res {
        let mut builder = axum::http::Request::builder()
            .method(self.method)
            .uri(&self.url);
        if self.body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        for (k, v) in &self.headers {
            builder = builder.header(k, v);
        }
        let req = builder
            .body(match self.body {
                Some(v) => Body::from(serde_json::to_vec(&v).unwrap()),
                None => Body::empty(),
            })
            .unwrap();
        let resp = client()
            .request(req)
            .await
            .expect("request should complete");
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Res {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }
}

/// An OpenAI-style chat request.
pub fn openai_chat(addr: SocketAddr, model: &str) -> Req {
    Req::post(
        format!("http://{addr}/v1/chat/completions"),
        json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}),
    )
}

/// An Anthropic-style messages request.
pub fn anthropic_chat(addr: SocketAddr, model: &str) -> Req {
    Req::post(
        format!("http://{addr}/v1/messages"),
        json!({
            "model": model,
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .anthropic()
}

pub async fn get(url: &str) -> Res {
    Req::get(url).send().await
}

/// Poll `f` until it returns true or the deadline passes.
pub async fn wait_until<F, Fut>(what: &str, timeout: Duration, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if f().await {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
