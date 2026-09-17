//! A mock OpenAI-compatible upstream, plus helpers to drive the router.

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

use mini_router::config::Config;
use mini_router::state::AppState;

/// Knobs a test can turn while the mock upstream is running.
#[derive(Debug, Default)]
pub struct MockControl {
    /// Status returned by the completion endpoints.
    pub status: AtomicUsize,
    /// Status returned by `GET /v1/models`.
    pub health_status: AtomicUsize,
    /// Artificial think time before answering.
    pub delay_ms: AtomicU64,
    /// Completion requests received.
    pub hits: AtomicUsize,
    /// Requests being served right now, and the high-water mark.
    pub inflight: AtomicUsize,
    pub peak_inflight: AtomicUsize,
    /// Model field of the last request body received.
    pub last_model: Mutex<Option<String>>,
    /// Authorization header of the last request received.
    pub last_auth: Mutex<Option<String>>,
}

#[derive(Clone)]
pub struct Mock {
    pub name: &'static str,
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
    pub fn last_model(&self) -> Option<String> {
        self.ctl.last_model.lock().unwrap().clone()
    }
    pub fn last_auth(&self) -> Option<String> {
        self.ctl.last_auth.lock().unwrap().clone()
    }
}

#[derive(Clone)]
struct MockState {
    name: &'static str,
    models: Vec<String>,
    ctl: Arc<MockControl>,
}

/// Start a mock upstream on an ephemeral port.
pub async fn start_mock(name: &'static str, models: &[&str]) -> Mock {
    let ctl = Arc::new(MockControl::default());
    ctl.status.store(200, Ordering::Relaxed);
    ctl.health_status.store(200, Ordering::Relaxed);
    let models: Vec<String> = models.iter().map(|m| m.to_string()).collect();

    let state = MockState {
        name,
        models: models.clone(),
        ctl: ctl.clone(),
    };

    let app = Router::new()
        .route("/v1/models", axum::routing::get(mock_models))
        .route("/v1/chat/completions", axum::routing::post(mock_completion))
        .route("/v1/embeddings", axum::routing::post(mock_completion))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Mock {
        name,
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
    let data: Vec<serde_json::Value> = s
        .models
        .iter()
        .map(|m| serde_json::json!({"id": m, "object": "model", "owned_by": s.name}))
        .collect();
    axum::Json(serde_json::json!({"object": "list", "data": data})).into_response()
}

async fn mock_completion(State(s): State<MockState>, headers: HeaderMap, body: Body) -> Response {
    s.ctl.hits.fetch_add(1, Ordering::Relaxed);
    let now = s.ctl.inflight.fetch_add(1, Ordering::Relaxed) + 1;
    s.ctl.peak_inflight.fetch_max(now, Ordering::Relaxed);
    let _guard = InflightGuard(s.ctl.clone());

    *s.ctl.last_auth.lock().unwrap() = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let bytes = body
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    let parsed: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    *s.ctl.last_model.lock().unwrap() = parsed
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::to_owned);
    let streaming = parsed
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let delay = s.ctl.delay_ms.load(Ordering::Relaxed);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }

    let status =
        StatusCode::from_u16(s.ctl.status.load(Ordering::Relaxed) as u16).unwrap_or(StatusCode::OK);
    if !status.is_success() {
        return (
            status,
            axum::Json(serde_json::json!({"error": {"message": "mock failure"}})),
        )
            .into_response();
    }

    if streaming {
        return sse_response(s.name);
    }

    axum::Json(serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "model": parsed.get("model").cloned().unwrap_or(serde_json::Value::Null),
        "served_by": s.name,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": format!("hello from {}", s.name)},
            "finish_reason": "stop"
        }]
    }))
    .into_response()
}

struct InflightGuard(Arc<MockControl>);
impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Five SSE chunks, spaced out, so a test can tell streaming from buffering.
pub const SSE_CHUNKS: usize = 5;
pub const SSE_GAP: Duration = Duration::from_millis(80);

fn sse_response(name: &'static str) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(1);
    tokio::spawn(async move {
        for i in 0..SSE_CHUNKS {
            let chunk = format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "chatcmpl-mock",
                    "object": "chat.completion.chunk",
                    "served_by": name,
                    "choices": [{"index": 0, "delta": {"content": format!("tok{i} ")}}]
                })
            );
            if tx.send(Ok(axum::body::Bytes::from(chunk))).await.is_err() {
                return;
            }
            tokio::time::sleep(SSE_GAP).await;
        }
        let _ = tx
            .send(Ok(axum::body::Bytes::from("data: [DONE]\n\n")))
            .await;
    });
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}

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
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("body is not json ({e}): {}", self.body))
    }
    pub fn upstream(&self) -> Option<&str> {
        self.headers
            .get(mini_router::proxy::UPSTREAM_HEADER)
            .and_then(|v| v.to_str().ok())
    }
}

pub async fn request(
    method: &str,
    url: &str,
    body: Option<serde_json::Value>,
    key: Option<&str>,
) -> Res {
    let mut builder = axum::http::Request::builder().method(method).uri(url);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    if let Some(k) = key {
        builder = builder.header("authorization", format!("Bearer {k}"));
    }
    let req = builder
        .body(match body {
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

pub async fn get(url: &str) -> Res {
    request("GET", url, None, None).await
}

pub async fn chat(addr: SocketAddr, model: &str) -> Res {
    request(
        "POST",
        &format!("http://{addr}/v1/chat/completions"),
        Some(serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}]
        })),
        None,
    )
    .await
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
