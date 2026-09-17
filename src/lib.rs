//! mini-router: a small OpenAI-compatible LLM aggregator and load balancer.
//!
//! The design target is a single-board computer -- an Orange Pi Zero 3 with
//! 1 GB of RAM -- sitting in front of one or more small model servers
//! (llama.cpp, Ollama, vLLM, or a cloud endpoint as a fallback). Every choice
//! in here follows from that: no buffered response bodies, admission control
//! instead of unbounded queues, atomics instead of locks on the hot path, and
//! a dependency tree small enough to cross-compile in a couple of minutes.
//!
//! ```no_run
//! use mini_router::{config::Config, state::AppState, router};
//! use std::sync::Arc;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let cfg = Config::from_toml(r#"
//!     [[upstream]]
//!     name = "local"
//!     url = "http://127.0.0.1:11434/v1"
//! "#)?;
//! let state = Arc::new(AppState::new(cfg));
//! let app = router(state.clone());
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
//! axum::serve(listener, app).await?;
//! # Ok(())
//! # }
//! ```

pub mod admin;
pub mod auth;
pub mod balance;
pub mod config;
pub mod error;
pub mod health;
pub mod metrics;
pub mod models;
pub mod proxy;
pub mod state;
pub mod upstream;
pub mod util;

use axum::routing::{any, get};
use axum::Router;

pub use config::Config;
pub use state::{AppState, SharedState};

/// Build the HTTP router.
///
/// `/v1/{*rest}` catches every OpenAI endpoint we do not handle ourselves --
/// completions, embeddings, rerank, audio -- and forwards it, so mini-router
/// keeps working when an upstream grows an endpoint we have never heard of.
pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/v1/models", get(models::list_models))
        .route("/v1/models/{id}", get(models::get_model))
        .route("/v1/{*rest}", any(proxy::proxy))
        .route("/healthz", get(admin::healthz))
        .route("/readyz", get(admin::readyz))
        .route("/metrics", get(admin::metrics))
        .route("/admin/upstreams", get(admin::upstreams))
        .fallback(not_found)
        .with_state(state)
}

async fn not_found() -> axum::response::Response {
    use axum::response::IntoResponse;
    error::ApiError::new(
        error::ErrorKind::NotFound,
        "unknown endpoint; mini-router proxies /v1/* and serves /healthz, /readyz, /metrics and /admin/upstreams",
    )
    .into_response()
}

/// The banner printed at startup, also used by `--version`.
pub fn version_line() -> String {
    format!(
        "{} {} ({} build{})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        if cfg!(feature = "tls") {
            ", tls"
        } else {
            ", no tls"
        },
    )
}
