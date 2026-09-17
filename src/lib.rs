//! mini-router: a small LLM API aggregator and load balancer.
//!
//! One endpoint in front of every model provider you already pay for. Clients
//! speak either the OpenAI Chat Completions dialect or the Anthropic Messages
//! dialect, and either one can reach either kind of provider -- mini-router
//! translates requests, responses and streams in both directions.
//!
//! The models are remote. The router is not: it is meant to run on a small
//! always-on box on your own network -- an Orange Pi Zero 3 is the design
//! target -- so it is a couple of megabytes of binary and stays under 5 MB of
//! RSS regardless of how many completions are streaming through it.
//!
//! The pieces:
//!
//! - [`protocol`] recognises the dialect a request arrived in and knows how
//!   each provider wants to be authenticated.
//! - [`translate`] converts between the dialects, including incremental
//!   translation of server-sent event streams.
//! - [`config`] defines pools: one client-facing model name over several
//!   provider models, tried in order.
//! - [`balance`] orders the candidates for a request.
//! - [`proxy`] tries them until one answers, spilling over on any failure.
//!
//! ```no_run
//! use mini_router::{config::Config, state::AppState, router};
//! use std::sync::Arc;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let cfg = Config::from_toml(r#"
//!     [[upstream]]
//!     name = "anthropic"
//!     url = "https://api.anthropic.com/v1"
//!     protocol = "anthropic"
//!     api_key_env = "ANTHROPIC_API_KEY"
//!
//!     [[upstream]]
//!     name = "openai"
//!     url = "https://api.openai.com/v1"
//!     api_key_env = "OPENAI_API_KEY"
//!
//!     [pool.fast]
//!     members = [
//!       { upstream = "openai", model = "gpt-4o-mini" },
//!       { upstream = "anthropic", model = "claude-haiku-4-5" },
//!     ]
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
pub mod protocol;
pub mod proxy;
pub mod state;
pub mod translate;
pub mod upstream;
pub mod util;

use axum::routing::{any, get};
use axum::Router;

pub use config::Config;
pub use state::{AppState, SharedState};

/// Build the HTTP router.
///
/// Everything under `/v1`, `/openai` and `/anthropic` goes to one gateway
/// handler, which works out the dialect from the path and headers. Routing
/// there rather than in the route table is what lets both SDKs point at the
/// same port and lets an endpoint we have never heard of still be forwarded.
pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/healthz", get(admin::healthz))
        .route("/readyz", get(admin::readyz))
        .route("/metrics", get(admin::metrics))
        .route("/admin/upstreams", get(admin::upstreams))
        .fallback(any(proxy::gateway))
        .with_state(state)
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
