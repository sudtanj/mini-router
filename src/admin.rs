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
    pub spillover: String,
    pub uptime_seconds: u64,
    /// 0 means "try every candidate".
    pub max_attempts: usize,
    pub pools: Vec<PoolView>,
    pub upstreams: Vec<UpstreamStatus>,
}

/// A pool and the state of the providers behind it, in priority order.
#[derive(Debug, Serialize)]
pub struct PoolView {
    pub name: String,
    pub strategy: String,
    pub members: Vec<PoolMemberView>,
}

#[derive(Debug, Serialize)]
pub struct PoolMemberView {
    pub upstream: String,
    pub model: String,
    pub weight: u32,
    /// False when this member's provider is out of rotation right now.
    pub available: bool,
}

/// `GET /admin/upstreams` -- everything the router knows about the shelf.
pub async fn upstreams(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Err(e) = auth::authorize(&state, &headers) {
        return e.into_response();
    }
    let pools = state
        .cfg
        .pools
        .iter()
        .map(|(name, pool)| PoolView {
            name: name.clone(),
            strategy: pool
                .strategy
                .unwrap_or(state.cfg.balance.strategy)
                .to_string(),
            members: pool
                .members
                .iter()
                .map(|m| PoolMemberView {
                    upstream: m.upstream.clone(),
                    model: m.model.clone(),
                    weight: m.weight,
                    available: state.get(&m.upstream).is_some_and(|u| u.is_available()),
                })
                .collect(),
        })
        .collect();

    Json(AdminView {
        strategy: state.balancer.strategy().to_string(),
        spillover: match state.cfg.balance.spillover {
            crate::config::Spillover::AnyError => "any-error".into(),
            crate::config::Spillover::StatusList => "status-list".into(),
        },
        uptime_seconds: state.metrics.uptime_secs() as u64,
        max_attempts: state.cfg.balance.max_attempts,
        pools,
        upstreams: state.snapshots().await,
    })
    .into_response()
}
