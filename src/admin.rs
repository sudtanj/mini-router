//! Operational endpoints: liveness, readiness, metrics and introspection.

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::auth;
use crate::state::SharedState;
use crate::upstream::UpstreamStatus;

/// `GET /healthz` -- is the process alive. Deliberately unauthenticated and
/// deliberately not about upstreams: this is what a supervisor restarts on.
pub async fn healthz() -> Response {
    (StatusCode::OK, "ok\n").into_response()
}

/// `GET /readyz` -- can we actually serve a request right now.
pub async fn readyz(State(state): State<SharedState>) -> Response {
    if state.any_available() {
        (StatusCode::OK, "ready\n").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "no upstream in rotation\n").into_response()
    }
}

/// `GET /metrics` -- Prometheus exposition.
pub async fn metrics(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Err(e) = auth::authorize(&state, &headers) {
        return e.into_response();
    }
    let snapshots = state.snapshots().await;
    let body = state.metrics.render(&snapshots);
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

#[derive(Debug, Serialize)]
pub struct AdminView {
    pub strategy: String,
    pub uptime_seconds: u64,
    pub retries: usize,
    pub upstreams: Vec<UpstreamStatus>,
}

/// `GET /admin/upstreams` -- everything the router knows about the shelf.
pub async fn upstreams(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Err(e) = auth::authorize(&state, &headers) {
        return e.into_response();
    }
    Json(AdminView {
        strategy: state.balancer.strategy().to_string(),
        uptime_seconds: state.metrics.uptime_secs() as u64,
        retries: state.cfg.balance.retries,
        upstreams: state.snapshots().await,
    })
    .into_response()
}
