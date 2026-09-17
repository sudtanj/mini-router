//! The forwarding path.
//!
//! One request comes in, in one of two dialects. The router works out which
//! providers could serve it, tries them in order, and gives the client back an
//! answer in the dialect it asked in -- translating on the way out and on the
//! way back when the provider speaks the other one.
//!
//! Two properties are load-bearing:
//!
//! 1. **Spillover is total.** Anything that is not a success -- a refused
//!    connection, a timeout, a rate limit, an expired key, a model the
//!    provider has never heard of -- moves the request to the next candidate.
//!    Only when the list is exhausted does the client see a failure, and then
//!    it sees the provider's own last error rather than a synthetic one.
//! 2. **Streams stay streams.** A same-dialect response is passed through
//!    frame by frame without being looked at. A cross-dialect response is run
//!    through an incremental translator, which is still frame by frame. Either
//!    way a long generation costs no more memory than a short one.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body::{Body as HttpBody, Frame, SizeHint};
use http_body_util::{BodyExt, Limited};
use serde_json::Value;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::timeout;

use crate::auth;
use crate::balance::Target;
use crate::error::{invalid_request, unavailable, ApiError, ErrorKind};
use crate::metrics::Metrics;
use crate::protocol::{apply_auth, Endpoint, Ingress, Protocol};
use crate::state::SharedState;
use crate::translate::{self, sse::StreamTranslator};
use crate::upstream::Upstream;

/// Headers that belong to a single hop and must not be forwarded.
/// The credential headers are in here because the router replaces them with
/// the provider's own.
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

/// Response headers that stop being true once a body has been translated.
const REWRITTEN_ON_TRANSLATE: &[&str] = &["content-length", "content-encoding", "content-type"];

/// Names the provider that answered.
pub const UPSTREAM_HEADER: &str = "x-mini-router-upstream";
/// Names the provider-side model it was asked for.
pub const MODEL_HEADER: &str = "x-mini-router-model";
/// Present when the response was translated, e.g. `anthropic->openai`.
pub const TRANSLATED_HEADER: &str = "x-mini-router-translated";

/// Entry point for every API request.
pub async fn gateway(State(state): State<SharedState>, req: Request<Body>) -> Response {
    let (parts, body) = req.into_parts();

    let Some(ingress) = Ingress::classify(parts.uri.path(), &parts.headers) else {
        return ApiError::new(
            ErrorKind::NotFound,
            "unknown endpoint; mini-router serves /v1/* (OpenAI), /v1/messages (Anthropic), \
             /healthz, /readyz, /metrics and /admin/upstreams",
        )
        .into_response();
    };

    if let Err(e) = auth::authorize(&state, &parts.headers) {
        state.metrics.record_status(e.kind.status().as_u16());
        return e.into_dialect(ingress.protocol);
    }

    // The catalogue is answered by the router itself, not forwarded.
    if ingress.endpoint == Endpoint::Models {
        return crate::models::list_models(&state, ingress.protocol).await;
    }
    if let Some(id) = ingress.model_id_in_path() {
        return crate::models::get_model(&state, ingress.protocol, id).await;
    }

    let limit = state.cfg.server.max_body_bytes;
    let body_bytes = match axum::body::to_bytes(body, limit).await {
        Ok(b) => b,
        Err(_) => {
            state.metrics.record_status(413);
            let e = invalid_request(format!("request body exceeds max_body_bytes ({limit})"));
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                e.dialect_body(ingress.protocol),
            )
                .into_response();
        }
    };

    Metrics::incr(&state.metrics.requests_total);

    let parsed: Option<Value> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice(&body_bytes).ok()
    };

    let requested_model = parsed
        .as_ref()
        .and_then(|v| v.get("model"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let streaming = parsed
        .as_ref()
        .is_some_and(|v| v.get("stream").and_then(Value::as_bool).unwrap_or(false));
    if streaming {
        Metrics::incr(&state.metrics.streaming_total);
    }

    // Aliases are resolved before routing, so an alias can point at a pool.
    let routing_name = requested_model
        .as_deref()
        .map(|m| state.cfg.resolve_alias(m).to_owned());

    let ctx = Ctx {
        ingress,
        parsed,
        raw_body: body_bytes,
        streaming,
        // What the client called it, echoed back in translated responses.
        client_model: requested_model.unwrap_or_default(),
        method: parts.method,
        query: parts.uri.query().map(str::to_owned),
        headers: parts.headers,
    };

    match dispatch(&state, &ctx, routing_name.as_deref()).await {
        Ok(resp) => {
            state.metrics.record_status(resp.status().as_u16());
            resp
        }
        Err(e) => {
            state.metrics.record_status(e.kind.status().as_u16());
            e.into_dialect(ctx.ingress.protocol)
        }
    }
}

/// Everything about the incoming request that an attempt needs.
struct Ctx {
    ingress: Ingress,
    parsed: Option<Value>,
    raw_body: Bytes,
    streaming: bool,
    client_model: String,
    method: Method,
    query: Option<String>,
    headers: HeaderMap,
}

/// A provider response we decided to spill over, kept in case it turns out to
/// be the best explanation we can give the client.
struct LastFailure {
    status: StatusCode,
    body: Bytes,
    protocol: Protocol,
    target: String,
}

async fn dispatch(
    state: &SharedState,
    ctx: &Ctx,
    routing_name: Option<&str>,
) -> Result<Response, ApiError> {
    let plan = state
        .plan(routing_name, ctx.ingress.protocol, &ctx.ingress.endpoint)
        .await;

    if plan.targets.is_empty() {
        Metrics::incr(&state.metrics.no_upstream_total);
        return Err(no_candidate_error(state, routing_name, ctx));
    }

    let configured = state.cfg.balance.max_attempts;
    let max_attempts = if configured == 0 {
        plan.targets.len()
    } else {
        configured.min(plan.targets.len())
    };

    let mut last_transport: Option<ApiError> = None;
    let mut last_failure: Option<LastFailure> = None;

    for (attempt, target) in plan.targets.iter().take(max_attempts).enumerate() {
        if attempt > 0 {
            Metrics::incr(&state.metrics.retries_total);
        }

        let permit = match acquire(state, target, attempt, max_attempts).await {
            Ok(p) => p,
            Err(e) => {
                // Saturated: that is a failure like any other, so spill.
                last_transport = Some(e);
                continue;
            }
        };

        match attempt_once(state, ctx, target, permit).await {
            Ok(Outcome::Response(resp)) => return Ok(resp),
            Ok(Outcome::Spill(failure)) => {
                tracing::warn!(
                    target = %target.label(),
                    status = %failure.status,
                    attempt,
                    "provider failed, spilling over"
                );
                last_failure = Some(failure);
            }
            Err(e) => {
                tracing::warn!(target = %target.label(), attempt, error = %e.message, "attempt failed");
                target
                    .upstream
                    .set_last_error(Some(e.message.clone()))
                    .await;
                last_transport = Some(e);
            }
        }
    }

    // Prefer the provider's own explanation over one we invented.
    if let Some(f) = last_failure {
        Metrics::incr(&state.metrics.spilled_out_total);
        return Ok(provider_error_response(ctx, f));
    }
    Metrics::incr(&state.metrics.spilled_out_total);
    Err(last_transport.unwrap_or_else(|| unavailable("no provider could serve this request")))
}

fn no_candidate_error(state: &SharedState, routing_name: Option<&str>, ctx: &Ctx) -> ApiError {
    match routing_name {
        Some(m) if state.cfg.pools.contains_key(m) => unavailable(format!(
            "every member of pool {m:?} is out of rotation; check /admin/upstreams"
        )),
        Some(m) => unavailable(format!(
            "no provider currently serves model {m:?}; check /v1/models for what is available"
        )),
        None => match &ctx.ingress.endpoint {
            Endpoint::Passthrough(p) => unavailable(format!(
                "no {} provider is available to serve {p}",
                ctx.ingress.protocol
            )),
            _ => unavailable("no provider is currently available"),
        },
    }
}

/// Take a concurrency slot, queueing only when this is the last chance.
async fn acquire(
    state: &SharedState,
    target: &Target,
    attempt: usize,
    max_attempts: usize,
) -> Result<OwnedSemaphorePermit, ApiError> {
    if let Ok(p) = Arc::clone(&target.upstream.permits).try_acquire_owned() {
        return Ok(p);
    }
    // Another candidate is probably free; only wait when there is nobody left
    // to spill to.
    if attempt + 1 < max_attempts {
        return Err(unavailable(format!(
            "{} is at its concurrency limit",
            target.upstream.name
        ))
        .with_upstream(&target.upstream.name));
    }
    match timeout(
        state.cfg.server.queue_timeout(),
        Arc::clone(&target.upstream.permits).acquire_owned(),
    )
    .await
    {
        Ok(Ok(p)) => Ok(p),
        Ok(Err(_)) => Err(unavailable("router is shutting down")),
        Err(_) => {
            Metrics::incr(&state.metrics.queue_timeouts_total);
            Err(ApiError::new(
                ErrorKind::RateLimit,
                format!(
                    "every provider for this request was busy for {}s",
                    state.cfg.server.queue_timeout_secs
                ),
            )
            .with_upstream(&target.upstream.name))
        }
    }
}

enum Outcome {
    Response(Response),
    Spill(LastFailure),
}

async fn attempt_once(
    state: &SharedState,
    ctx: &Ctx,
    target: &Target,
    permit: OwnedSemaphorePermit,
) -> Result<Outcome, ApiError> {
    let egress = target.protocol();
    let translating = egress != ctx.ingress.protocol;

    let path = ctx.ingress.endpoint.path_for(egress);
    let path = match &ctx.query {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    let url = target.upstream.cfg.join(&path);

    let body = build_request_body(state, ctx, target, translating)?;

    let mut builder = Request::builder().method(ctx.method.clone()).uri(&url);
    {
        let Some(hs) = builder.headers_mut() else {
            return Err(unavailable(format!("bad provider url {url:?}"))
                .with_upstream(&target.upstream.name));
        };
        for (name, value) in ctx.headers.iter() {
            if HOP_BY_HOP.contains(&name.as_str()) {
                continue;
            }
            // A translated body is a different body; its length changed.
            if translating && name == header::CONTENT_LENGTH {
                continue;
            }
            hs.insert(name.clone(), value.clone());
        }
        for (name, value) in &target.upstream.cfg.headers {
            if let (Ok(n), Ok(v)) = (
                HeaderName::try_from(name.as_str()),
                HeaderValue::from_str(value),
            ) {
                hs.insert(n, v);
            }
        }
        hs.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        apply_auth(
            hs,
            egress,
            target.upstream.api_key.as_deref(),
            &state.cfg.translate.anthropic_version,
        );
    }

    let req = builder.body(Body::from(body)).map_err(|e| {
        unavailable(format!("could not build provider request: {e}"))
            .with_upstream(&target.upstream.name)
    })?;

    let guard = InflightGuard::new(target.upstream.clone(), permit);
    let started = Instant::now();
    let result = timeout(
        state.cfg.server.upstream_header_timeout(),
        state.client.request(req),
    )
    .await;

    let health = &state.cfg.health;
    let response = match result {
        Err(_) => {
            target
                .upstream
                .record_failure(health.failure_threshold, health.cooldown());
            return Err(unavailable(format!(
                "timed out after {}s waiting for response headers",
                state.cfg.server.upstream_header_timeout_secs
            ))
            .with_upstream(&target.upstream.name));
        }
        Ok(Err(e)) => {
            target
                .upstream
                .record_failure(health.failure_threshold, health.cooldown());
            return Err(unavailable(format!("{e}")).with_upstream(&target.upstream.name));
        }
        Ok(Ok(r)) => r,
    };

    let status = response.status();

    if state.cfg.balance.should_spill(status.as_u16()) {
        // A provider that names its own backoff knows better than our guess.
        if let Some(delay) = retry_after(response.headers(), health.max_cooldown()) {
            target.upstream.park_until(delay);
        }
        target
            .upstream
            .record_failure(health.failure_threshold, health.cooldown());
        let body = Limited::new(response.into_body(), state.cfg.server.max_translate_bytes)
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        let message =
            summarize_error(&body).unwrap_or_else(|| format!("provider returned {status}"));
        target.upstream.set_last_error(Some(message)).await;
        return Ok(Outcome::Spill(LastFailure {
            status,
            body,
            protocol: egress,
            target: target.label(),
        }));
    }

    // Time to first byte, measured at the headers: for a streamed completion
    // this is the prompt-processing time, which is the signal the
    // latency-aware strategies want.
    target.upstream.record_latency(started.elapsed());
    if status.is_server_error() {
        target
            .upstream
            .record_failure(health.failure_threshold, health.cooldown());
    } else {
        target.upstream.record_success(health.success_threshold);
        target.upstream.set_last_error(None).await;
    }

    let (parts, incoming) = response.into_parts();
    let mut out = Response::builder().status(parts.status);
    if let Some(hs) = out.headers_mut() {
        for (name, value) in parts.headers.iter() {
            if HOP_BY_HOP.contains(&name.as_str()) {
                continue;
            }
            if translating && REWRITTEN_ON_TRANSLATE.contains(&name.as_str()) {
                continue;
            }
            hs.insert(name.clone(), value.clone());
        }
        if let Ok(v) = HeaderValue::from_str(&target.upstream.name) {
            hs.insert(HeaderName::from_static(UPSTREAM_HEADER), v);
        }
        if let Ok(v) = HeaderValue::from_str(&target.model) {
            hs.insert(HeaderName::from_static(MODEL_HEADER), v);
        }
        if translating {
            if let Ok(v) = HeaderValue::from_str(&format!("{egress}->{}", ctx.ingress.protocol)) {
                hs.insert(HeaderName::from_static(TRANSLATED_HEADER), v);
            }
            hs.insert(
                header::CONTENT_TYPE,
                if ctx.streaming {
                    HeaderValue::from_static("text/event-stream")
                } else {
                    HeaderValue::from_static("application/json")
                },
            );
        }
    }

    if !translating {
        // Untouched: the provider's bytes go straight to the client.
        return out
            .body(Body::new(TrackedBody {
                inner: incoming,
                _guard: guard,
            }))
            .map(Outcome::Response)
            .map_err(|e| {
                unavailable(format!("could not build response: {e}"))
                    .with_upstream(&target.upstream.name)
            });
    }

    if ctx.streaming {
        let translator = stream_translator(egress, ctx.ingress.protocol, &ctx.client_model);
        Metrics::incr(&state.metrics.translated_total);
        return out
            .body(Body::new(TranslatedBody {
                inner: incoming,
                translator,
                finished: false,
                _guard: guard,
            }))
            .map(Outcome::Response)
            .map_err(|e| {
                unavailable(format!("could not build response: {e}"))
                    .with_upstream(&target.upstream.name)
            });
    }

    // A non-streamed body has to be read before it can be reshaped. It is one
    // completion, and it is bounded.
    let collected = Limited::new(incoming, state.cfg.server.max_translate_bytes)
        .collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|_| {
            unavailable(format!(
                "provider response exceeded max_translate_bytes ({})",
                state.cfg.server.max_translate_bytes
            ))
            .with_upstream(&target.upstream.name)
        })?;
    drop(guard);

    let parsed: Value = serde_json::from_slice(&collected).map_err(|e| {
        unavailable(format!("provider sent a body we could not parse: {e}"))
            .with_upstream(&target.upstream.name)
    })?;
    let translated = match (egress, ctx.ingress.protocol) {
        (Protocol::Anthropic, Protocol::Openai) => {
            translate::response_anthropic_to_openai(&parsed, &ctx.client_model)
        }
        (Protocol::Openai, Protocol::Anthropic) => {
            translate::response_openai_to_anthropic(&parsed, &ctx.client_model)
        }
        _ => parsed,
    };
    Metrics::incr(&state.metrics.translated_total);

    out.body(Body::from(translated.to_string()))
        .map(Outcome::Response)
        .map_err(|e| {
            unavailable(format!("could not build response: {e}"))
                .with_upstream(&target.upstream.name)
        })
}

/// Produce the body to send to this provider: the original bytes when the
/// dialects match, a translation when they do not. Either way the model field
/// becomes the provider-side id.
fn build_request_body(
    state: &SharedState,
    ctx: &Ctx,
    target: &Target,
    translating: bool,
) -> Result<Bytes, ApiError> {
    let Some(parsed) = &ctx.parsed else {
        // Not JSON: nothing to rewrite, and nothing we could translate.
        if translating {
            return Err(invalid_request(
                "this request cannot be translated between protocols because its body is not JSON",
            ));
        }
        return Ok(ctx.raw_body.clone());
    };

    if !translating {
        if target.model.is_empty()
            || parsed.get("model").and_then(Value::as_str) == Some(&target.model)
        {
            return Ok(ctx.raw_body.clone());
        }
        let mut v = parsed.clone();
        v["model"] = Value::String(target.model.clone());
        return serde_json::to_vec(&v)
            .map(Bytes::from)
            .map_err(|e| unavailable(format!("could not rewrite request: {e}")));
    }

    let translated = match (ctx.ingress.protocol, target.protocol()) {
        (Protocol::Openai, Protocol::Anthropic) => translate::request_openai_to_anthropic(
            parsed,
            &target.model,
            state.cfg.translate.default_max_tokens,
        ),
        (Protocol::Anthropic, Protocol::Openai) => {
            translate::request_anthropic_to_openai(parsed, &target.model)
        }
        _ => Ok(parsed.clone()),
    }
    .map_err(|e| invalid_request(format!("could not translate request: {e}")))?;

    serde_json::to_vec(&translated)
        .map(Bytes::from)
        .map_err(|e| unavailable(format!("could not serialise translated request: {e}")))
}

fn stream_translator(
    from: Protocol,
    to: Protocol,
    client_model: &str,
) -> Box<dyn StreamTranslator> {
    match (from, to) {
        (Protocol::Anthropic, Protocol::Openai) => {
            Box::new(translate::sse::AnthropicToOpenai::new(client_model))
        }
        _ => Box::new(translate::sse::OpenaiToAnthropic::new(client_model)),
    }
}

/// Hand the client the provider's own final error, in the client's dialect.
fn provider_error_response(ctx: &Ctx, failure: LastFailure) -> Response {
    let parsed: Value = serde_json::from_slice(&failure.body).unwrap_or(Value::Null);
    let fallback = format!("provider returned {}", failure.status);
    let body = if failure.protocol == ctx.ingress.protocol {
        parsed
    } else {
        match ctx.ingress.protocol {
            Protocol::Openai => translate::error_to_openai(&parsed, &fallback),
            Protocol::Anthropic => translate::error_to_anthropic(&parsed, &fallback),
        }
    };
    let mut resp = (
        failure.status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(&failure.target) {
        resp.headers_mut()
            .insert(HeaderName::from_static(UPSTREAM_HEADER), v);
    }
    resp
}

/// Pull a one-line explanation out of a provider error body, for the logs and
/// the admin view.
fn summarize_error(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let msg = v
        .get("error")
        .and_then(|e| e.get("message"))
        .or_else(|| v.get("message"))
        .and_then(Value::as_str)?;
    Some(msg.chars().take(300).collect())
}

/// `Retry-After` as seconds or an HTTP date, clamped to something sane.
fn retry_after(headers: &HeaderMap, max: Duration) -> Option<Duration> {
    let raw = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    let secs = raw.parse::<u64>().ok()?;
    Some(Duration::from_secs(secs).min(max))
}

/// Holds a provider's concurrency permit and its in-flight count for the
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

/// A provider response body that releases its slot when the last frame is read
/// -- or when the client hangs up mid-stream.
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

/// The same, but running every frame through a dialect translator on the way.
struct TranslatedBody {
    inner: hyper::body::Incoming,
    translator: Box<dyn StreamTranslator>,
    finished: bool,
    _guard: InflightGuard,
}

impl HttpBody for TranslatedBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        loop {
            if this.finished {
                return Poll::Ready(None);
            }
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    let Ok(data) = frame.into_data() else {
                        // Trailers carry nothing we can translate.
                        continue;
                    };
                    let out = this.translator.push(&data);
                    if out.is_empty() {
                        // A partial event: nothing to send until it completes.
                        continue;
                    }
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    this.finished = true;
                    let out = this.translator.finish();
                    if out.is_empty() {
                        return Poll::Ready(None);
                    }
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.finished
    }

    fn size_hint(&self) -> SizeHint {
        // The translated length is not known ahead of time.
        SizeHint::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_covers_the_credential_headers() {
        for h in ["authorization", "x-api-key", "host", "content-length"] {
            assert!(HOP_BY_HOP.contains(&h), "{h} must not be forwarded as-is");
        }
    }

    #[test]
    fn retry_after_is_parsed_and_clamped() {
        let mut h = HeaderMap::new();
        h.insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
        assert_eq!(
            retry_after(&h, Duration::from_secs(300)),
            Some(Duration::from_secs(30))
        );

        // A provider cannot park itself for longer than we allow.
        h.insert(header::RETRY_AFTER, HeaderValue::from_static("99999"));
        assert_eq!(
            retry_after(&h, Duration::from_secs(300)),
            Some(Duration::from_secs(300))
        );

        // HTTP-date form is not parsed; better to use our own cooldown than
        // to guess wrong.
        h.insert(
            header::RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&h, Duration::from_secs(300)), None);
        assert_eq!(
            retry_after(&HeaderMap::new(), Duration::from_secs(300)),
            None
        );
    }

    #[test]
    fn error_summaries_come_from_either_dialect() {
        assert_eq!(
            summarize_error(br#"{"error":{"message":"Insufficient credit"}}"#).as_deref(),
            Some("Insufficient credit")
        );
        assert_eq!(
            summarize_error(br#"{"type":"error","error":{"message":"Overloaded"}}"#).as_deref(),
            Some("Overloaded")
        );
        assert_eq!(summarize_error(b"not json"), None);
    }
}
