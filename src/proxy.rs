//! The forwarding path.
//!
//! Two properties matter more than anything else here, because the target is a
//! board with a gigabyte of RAM:
//!
//! 1. Response bodies are never buffered. A token stream is passed through
//!    frame by frame, so a 20-minute generation costs one socket, not a
//!    growing `Vec<u8>`.
//! 2. Every in-flight request holds a permit from its upstream's semaphore for
//!    exactly as long as its body is still streaming. That permit *is* the
//!    admission control that keeps two 7B generations from meeting on a board
//!    that can only afford one.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body::{Body as HttpBody, Frame, SizeHint};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::timeout;

use crate::auth;
use crate::error::{invalid_request, unavailable, ApiError, ErrorKind};
use crate::metrics::Metrics;
use crate::state::SharedState;
use crate::upstream::Upstream;

/// Headers that belong to a single hop and must not be forwarded.
/// `Authorization` is in here because the router replaces it with the
/// upstream's own credential.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
    "authorization",
    "x-api-key",
];

/// Header naming the upstream that actually answered. Invaluable when three
/// boards disagree about what a model should say.
pub const UPSTREAM_HEADER: &str = "x-mini-router-upstream";

/// Handler for every proxied `/v1/*` endpoint.
pub async fn proxy(State(state): State<SharedState>, req: Request<Body>) -> Response {
    let (parts, body) = req.into_parts();

    if let Err(e) = auth::authorize(&state, &parts.headers) {
        state.metrics.record_status(e.kind.status().as_u16());
        return e.into_response();
    }

    let limit = state.cfg.server.max_body_bytes;
    let body_bytes = match axum::body::to_bytes(body, limit).await {
        Ok(b) => b,
        Err(_) => {
            let e = invalid_request(format!("request body exceeds max_body_bytes ({limit})"));
            state.metrics.record_status(413);
            return (StatusCode::PAYLOAD_TOO_LARGE, e.body()).into_response();
        }
    };

    Metrics::incr(&state.metrics.requests_total);

    // Route on the model field, rewriting it when it is an alias. Bodies that
    // are not JSON (or carry no model) still get routed, just without a model
    // constraint, which is what makes non-chat endpoints work unchanged.
    let parsed: Option<serde_json::Value> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice(&body_bytes).ok()
    };

    let requested_model = parsed
        .as_ref()
        .and_then(|v| v.get("model"))
        .and_then(|m| m.as_str())
        .map(str::to_owned);

    if parsed.as_ref().is_some_and(|v| {
        v.get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }) {
        Metrics::incr(&state.metrics.streaming_total);
    }

    let mut outgoing = body_bytes;
    let target_model = match &requested_model {
        Some(m) => {
            let resolved = state.cfg.resolve_alias(m).to_owned();
            if &resolved != m {
                if let Some(mut v) = parsed.clone() {
                    v["model"] = serde_json::Value::String(resolved.clone());
                    match serde_json::to_vec(&v) {
                        Ok(buf) => outgoing = Bytes::from(buf),
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to rewrite aliased model");
                        }
                    }
                }
                tracing::debug!(from = %m, to = %resolved, "model alias applied");
            }
            Some(resolved)
        }
        None => None,
    };

    let path = upstream_path(parts.uri.path());
    let target = match parts.uri.query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_owned(),
    };

    match dispatch(
        &state,
        &parts.method,
        &target,
        &parts.headers,
        outgoing,
        target_model.as_deref(),
    )
    .await
    {
        Ok(resp) => {
            state.metrics.record_status(resp.status().as_u16());
            resp
        }
        Err(e) => {
            state.metrics.record_status(e.kind.status().as_u16());
            e.into_response()
        }
    }
}

/// Try upstreams until one answers, up to `balance.retries` extra attempts.
async fn dispatch(
    state: &SharedState,
    method: &Method,
    target: &str,
    headers: &HeaderMap,
    body: Bytes,
    model: Option<&str>,
) -> Result<Response, ApiError> {
    let max_attempts = state.cfg.balance.retries + 1;
    let mut tried: Vec<String> = Vec::with_capacity(max_attempts);
    let mut last: Option<ApiError> = None;

    for attempt in 0..max_attempts {
        let excluded: Vec<&str> = tried.iter().map(String::as_str).collect();
        let candidates = state.candidates(model, &excluded).await;
        if candidates.is_empty() {
            break;
        }

        // Prefer an upstream with a free slot; fall back to the whole set and
        // queue only when every one of them is busy.
        let free: Vec<Arc<Upstream>> = candidates
            .iter()
            .filter(|c| c.free_slots() > 0)
            .cloned()
            .collect();
        let pool = if free.is_empty() { &candidates } else { &free };

        let Some(up) = state.balancer.select(pool) else {
            break;
        };

        if attempt > 0 {
            Metrics::incr(&state.metrics.retries_total);
        }

        let permit = match Arc::clone(&up.permits).try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                let waited = timeout(
                    state.cfg.server.queue_timeout(),
                    Arc::clone(&up.permits).acquire_owned(),
                )
                .await;
                match waited {
                    Ok(Ok(p)) => p,
                    Ok(Err(_)) => {
                        // Semaphore closed: we are shutting down.
                        return Err(unavailable("router is shutting down"));
                    }
                    Err(_) => {
                        Metrics::incr(&state.metrics.queue_timeouts_total);
                        return Err(ApiError::new(
                            ErrorKind::RateLimit,
                            format!(
                                "every upstream serving this model was busy for {}s",
                                state.cfg.server.queue_timeout_secs
                            ),
                        )
                        .with_upstream(up.name.clone()));
                    }
                }
            }
        };

        tried.push(up.name.clone());
        match attempt_once(state, &up, permit, method, target, headers, body.clone()).await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                tracing::warn!(upstream = %up.name, attempt, error = %e.message, "attempt failed");
                up.set_last_error(Some(e.message.clone())).await;
                last = Some(e);
            }
        }
    }

    Err(last.unwrap_or_else(|| {
        Metrics::incr(&state.metrics.no_upstream_total);
        match model {
            Some(m) => unavailable(format!(
                "no healthy upstream serves model {m:?}; check /admin/upstreams"
            )),
            None => unavailable("no healthy upstream available"),
        }
    }))
}

/// One request against one upstream.
async fn attempt_once(
    state: &SharedState,
    up: &Arc<Upstream>,
    permit: OwnedSemaphorePermit,
    method: &Method,
    target: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let url = up.cfg.join(target);
    let mut builder = Request::builder().method(method.clone()).uri(&url);

    {
        let Some(hs) = builder.headers_mut() else {
            return Err(unavailable(format!("bad upstream url {url:?}")).with_upstream(&up.name));
        };
        for (name, value) in headers.iter() {
            if HOP_BY_HOP.contains(&name.as_str()) {
                continue;
            }
            hs.insert(name.clone(), value.clone());
        }
        if let Some(key) = &up.api_key {
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {key}")) {
                let mut v = v;
                v.set_sensitive(true);
                hs.insert(header::AUTHORIZATION, v);
            }
        }
    }

    let req = builder.body(Body::from(body)).map_err(|e| {
        unavailable(format!("could not build upstream request: {e}")).with_upstream(&up.name)
    })?;

    let guard = InflightGuard::new(up.clone(), permit);
    let started = Instant::now();
    let result = timeout(
        state.cfg.server.upstream_header_timeout(),
        state.client.request(req),
    )
    .await;

    let health = &state.cfg.health;
    let response = match result {
        Err(_) => {
            up.record_failure(health.failure_threshold, health.cooldown());
            return Err(unavailable(format!(
                "timed out after {}s waiting for response headers",
                state.cfg.server.upstream_header_timeout_secs
            ))
            .with_upstream(&up.name));
        }
        Ok(Err(e)) => {
            up.record_failure(health.failure_threshold, health.cooldown());
            return Err(unavailable(format!("{e}")).with_upstream(&up.name));
        }
        Ok(Ok(r)) => r,
    };

    let status = response.status();
    let retryable = state.retry_statuses.contains(&status.as_u16());
    if retryable {
        up.record_failure(health.failure_threshold, health.cooldown());
        return Err(ApiError::new(
            if status == StatusCode::TOO_MANY_REQUESTS {
                ErrorKind::RateLimit
            } else {
                ErrorKind::UpstreamUnavailable
            },
            format!("upstream returned {status}"),
        )
        .with_upstream(&up.name));
    }

    // Time to first byte, measured at the headers: for a streamed completion
    // this is the prompt-processing time, which is exactly the signal the
    // latency-aware strategies want.
    up.record_latency(started.elapsed());
    if status.is_server_error() {
        up.record_failure(health.failure_threshold, health.cooldown());
    } else {
        up.record_success(health.success_threshold);
        up.set_last_error(None).await;
    }

    let (parts, incoming) = response.into_parts();
    let mut out = Response::builder().status(parts.status);
    if let Some(hs) = out.headers_mut() {
        for (name, value) in parts.headers.iter() {
            if HOP_BY_HOP.contains(&name.as_str()) {
                continue;
            }
            hs.insert(name.clone(), value.clone());
        }
        if let Ok(v) = HeaderValue::from_str(&up.name) {
            hs.insert(HeaderName::from_static(UPSTREAM_HEADER), v);
        }
    }

    out.body(Body::new(TrackedBody {
        inner: incoming,
        _guard: guard,
    }))
    .map_err(|e| unavailable(format!("could not build response: {e}")).with_upstream(&up.name))
}

/// Strip the `/v1` prefix so it can be re-joined onto an upstream base URL that
/// already carries its own version prefix.
fn upstream_path(path: &str) -> &str {
    match path.strip_prefix("/v1") {
        Some("") => "/",
        Some(rest) => rest,
        None => path,
    }
}

/// Holds an upstream's concurrency permit and its in-flight count for the
/// lifetime of a request, including the streamed body.
#[derive(Debug)]
struct InflightGuard {
    up: Arc<Upstream>,
    _permit: OwnedSemaphorePermit,
}

impl InflightGuard {
    fn new(up: Arc<Upstream>, permit: OwnedSemaphorePermit) -> Self {
        up.incr_inflight();
        up.total_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            up,
            _permit: permit,
        }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.up.decr_inflight();
    }
}

/// An upstream response body that releases its slot when the last frame is
/// read -- or when the client hangs up mid-stream, which is why this is a body
/// wrapper and not a line in the handler.
struct TrackedBody {
    inner: hyper::body::Incoming,
    _guard: InflightGuard,
}

impl HttpBody for TrackedBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_prefix_is_stripped() {
        assert_eq!(upstream_path("/v1/chat/completions"), "/chat/completions");
        assert_eq!(upstream_path("/v1/models"), "/models");
        assert_eq!(upstream_path("/v1"), "/");
        // Anything outside /v1 is forwarded untouched.
        assert_eq!(upstream_path("/api/generate"), "/api/generate");
    }

    #[test]
    fn hop_by_hop_covers_the_credential_headers() {
        for h in ["authorization", "x-api-key", "host", "content-length"] {
            assert!(HOP_BY_HOP.contains(&h), "{h} must not be forwarded as-is");
        }
    }
}
