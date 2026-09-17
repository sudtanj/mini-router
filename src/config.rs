//! The configuration model and its validation.
//!
//! There is no configuration file. Every setting comes from the environment
//! (see [`crate::env`]), which is what makes a `docker compose` file with an
//! `environment:` block a complete deployment. This module holds the shape
//! those variables are parsed into, the defaults for everything left unset,
//! and the rules that reject a configuration that would fail confusingly at
//! runtime.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use crate::protocol::{Protocol, DEFAULT_ANTHROPIC_VERSION};

/// Top level configuration document.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub server: ServerConfig,
    pub balance: BalanceConfig,
    pub health: HealthConfig,
    pub translate: TranslateConfig,
    /// Client-facing model name -> upstream model name.
    pub alias: BTreeMap<String, String>,
    /// Named groups of provider models that serve one client-facing name.
    pub pools: BTreeMap<String, PoolConfig>,
    pub upstreams: Vec<UpstreamConfig>,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Address to bind the HTTP listener to.
    pub listen: SocketAddr,
    /// Tokio worker threads. 0 means "one per core", which is rarely what you
    /// want on a small board whose job is mostly waiting on sockets.
    pub worker_threads: usize,
    /// Largest request body accepted from a client, in bytes.
    pub max_body_bytes: usize,
    /// Largest non-streamed response body the router will translate. Only
    /// applies when the client and the provider speak different dialects;
    /// same-dialect responses are streamed through untouched at any size.
    pub max_translate_bytes: usize,
    /// How long to wait for a provider to return response *headers*. Body
    /// streaming is not bounded by this: a long generation is not a timeout.
    pub upstream_header_timeout_secs: u64,
    /// How long a request may wait for a free slot when every candidate is
    /// already at its concurrency limit.
    pub queue_timeout_secs: u64,
    /// Idle keep-alive timeout for pooled provider connections.
    pub pool_idle_timeout_secs: u64,
    /// Log level: error, warn, info, debug, trace.
    pub log_level: String,
    /// Serve `GET /admin/upstreams`. It is a JSON endpoint, not a dashboard --
    /// there is no UI anywhere in mini-router -- but turning it off removes one
    /// more thing listening.
    pub admin: bool,
    /// Serve `GET /metrics` (Prometheus text).
    pub metrics: bool,
    pub auth: AuthConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            worker_threads: default_worker_threads(),
            max_body_bytes: default_max_body_bytes(),
            max_translate_bytes: default_max_translate_bytes(),
            upstream_header_timeout_secs: default_upstream_header_timeout(),
            queue_timeout_secs: default_queue_timeout(),
            pool_idle_timeout_secs: default_pool_idle_timeout(),
            log_level: default_log_level(),
            admin: true,
            metrics: true,
            auth: AuthConfig::default(),
        }
    }
}

impl ServerConfig {
    pub fn upstream_header_timeout(&self) -> Duration {
        Duration::from_secs(self.upstream_header_timeout_secs)
    }
    pub fn queue_timeout(&self) -> Duration {
        Duration::from_secs(self.queue_timeout_secs)
    }
}

#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// Keys clients may present, as `Authorization: Bearer <key>` or
    /// `x-api-key: <key>` -- whichever dialect their SDK uses.
    pub api_keys: Vec<String>,
    /// Environment variables to read additional client keys from.
    pub api_key_envs: Vec<String>,
    /// Require a valid key on the API endpoints.
    pub require_auth: bool,
}

/// Knobs for cross-protocol translation.
#[derive(Debug, Clone)]
pub struct TranslateConfig {
    /// `max_tokens` is optional for OpenAI and mandatory for Anthropic, so an
    /// OpenAI-shaped request that omits it needs a value invented here.
    pub default_max_tokens: u64,
    /// Value sent as `anthropic-version` when the client did not pick one.
    pub anthropic_version: String,
}

impl Default for TranslateConfig {
    fn default() -> Self {
        Self {
            default_max_tokens: default_max_tokens(),
            anthropic_version: default_anthropic_version(),
        }
    }
}

/// How the candidates for a request are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Declaration order, every time. The first healthy candidate serves the
    /// request and anything that fails spills to the next one. This is the
    /// default because it is what people mean by "use my cheap provider, fall
    /// back to the good one".
    #[default]
    Priority,
    /// Plain rotation over the candidates.
    RoundRobin,
    /// Fewest requests in flight first.
    LeastConn,
    /// Rotation biased by the `weight` of each candidate.
    Weighted,
    /// Power of two choices on time-to-first-byte, then the rest by cost.
    P2cLatency,
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Strategy::Priority => "priority",
            Strategy::RoundRobin => "round-robin",
            Strategy::LeastConn => "least-conn",
            Strategy::Weighted => "weighted",
            Strategy::P2cLatency => "p2c-latency",
        })
    }
}

/// What counts as a reason to try the next candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Spillover {
    /// Anything that is not a success: a refused connection, a timeout, a
    /// rate limit, a 5xx, an expired key, a model the provider does not have.
    #[default]
    AnyError,
    /// Only the statuses listed in `retry_on_status`. Transport failures and
    /// timeouts still spill over -- there is no response to pass through.
    StatusList,
}

#[derive(Debug, Clone)]
pub struct BalanceConfig {
    pub strategy: Strategy,
    /// Cap on attempts per request. 0 means "try every candidate", which is
    /// what makes spillover actually exhaust the pool.
    pub max_attempts: usize,
    pub spillover: Spillover,
    /// Consulted only when spillover is `status-list`.
    pub retry_on_status: Vec<u16>,
}

impl Default for BalanceConfig {
    fn default() -> Self {
        Self {
            strategy: Strategy::default(),
            max_attempts: 0,
            spillover: Spillover::default(),
            retry_on_status: default_retry_statuses(),
        }
    }
}

impl BalanceConfig {
    /// Whether a provider response with this status should spill over.
    pub fn should_spill(&self, status: u16) -> bool {
        match self.spillover {
            Spillover::AnyError => !(200..300).contains(&status),
            Spillover::StatusList => self.retry_on_status.contains(&status),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HealthConfig {
    /// Seconds between probes. 0 disables active probing; providers are then
    /// only judged by live traffic.
    pub interval_secs: u64,
    pub timeout_secs: u64,
    /// Probe path, appended to the provider's base URL.
    pub path: String,
    /// Consecutive failures before a provider is taken out of rotation.
    pub failure_threshold: u32,
    /// Consecutive successes before a recovering provider is used again.
    pub success_threshold: u32,
    /// How long a provider stays out of rotation once it trips. A
    /// `Retry-After` header from the provider overrides this when it is
    /// longer, because the provider knows better than we do.
    pub cooldown_secs: u64,
    /// Upper bound on a `Retry-After` the router will honour, so a provider
    /// cannot park itself for an hour.
    pub max_cooldown_secs: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_health_interval(),
            timeout_secs: default_health_timeout(),
            path: default_health_path(),
            failure_threshold: default_failure_threshold(),
            success_threshold: default_success_threshold(),
            cooldown_secs: default_cooldown(),
            max_cooldown_secs: default_max_cooldown(),
        }
    }
}

impl HealthConfig {
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
    pub fn cooldown(&self) -> Duration {
        Duration::from_secs(self.cooldown_secs)
    }
    pub fn max_cooldown(&self) -> Duration {
        Duration::from_secs(self.max_cooldown_secs)
    }
}

/// A named group of provider models that one client-facing name resolves to.
/// Built from `MINI_ROUTER_POOL_<NAME>`.
#[derive(Debug, Clone, Default)]
pub struct PoolConfig {
    /// Members in priority order.
    pub members: Vec<PoolMember>,
    /// Overrides the global strategy for this pool.
    pub strategy: Option<Strategy>,
    /// Shown in the model catalogue.
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PoolMember {
    /// Name of the provider this member lives on.
    pub upstream: String,
    /// The provider-side model id.
    pub model: String,
    /// Relative share under the `weighted` strategy.
    pub weight: u32,
}

#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    /// Stable identifier, used in logs, metrics and the admin API.
    pub name: String,
    /// Base URL including any version prefix, e.g. `https://api.openai.com/v1`.
    pub url: String,
    /// Which dialect this provider speaks.
    pub protocol: Protocol,
    /// Bearer token or API key sent to this provider.
    pub api_key: Option<String>,
    /// Environment variable holding the key. Takes precedence over `api_key`,
    /// so secrets need not live in the config file.
    pub api_key_env: Option<String>,
    /// Relative share of traffic under the `weighted` strategy.
    pub weight: u32,
    /// Requests this provider may process at once. For a remote API this is
    /// about staying inside a rate limit, not about memory.
    pub max_concurrency: usize,
    /// Models this provider serves. Empty means "discover from
    /// `GET {url}/models` and refresh on every health probe".
    pub models: Vec<String>,
    /// Never route here unless the client asked for a model only this provider
    /// serves. Pools express priority through member order instead.
    pub fallback_only: bool,
    /// Extra headers sent with every request, for providers that want one
    /// (`HTTP-Referer` and `X-Title` for OpenRouter, say).
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(String);

impl ConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Build and validate the configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        crate::env::load(&crate::env::vars()).map(|r| r.config)
    }

    /// Reject configurations that would fail confusingly at runtime.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.upstreams.is_empty() {
            return Err(ConfigError(
                "no providers configured: mini-router has nothing to route to.\n\
                 Set a well-known key (OPENAI_API_KEY, ANTHROPIC_API_KEY, GROQ_API_KEY, ...),\n\
                 or define one with MINI_ROUTER_PROVIDER_<NAME>_URL.\n\
                 Run `mini-router --help` for the full list."
                    .into(),
            ));
        }
        let mut seen = BTreeMap::new();
        for up in &self.upstreams {
            if up.name.trim().is_empty() {
                return Err(ConfigError("upstream name must not be empty".into()));
            }
            if seen.insert(up.name.clone(), ()).is_some() {
                return Err(ConfigError(format!(
                    "duplicate upstream name {:?}",
                    up.name
                )));
            }
            if !up.url.starts_with("http://") && !up.url.starts_with("https://") {
                return Err(ConfigError(format!(
                    "upstream {:?}: url must start with http:// or https://, got {:?}",
                    up.name, up.url
                )));
            }
            if cfg!(not(feature = "tls")) && up.url.starts_with("https://") {
                return Err(ConfigError(format!(
                    "upstream {:?}: https requires the `tls` feature, which this build does not \
                     have. Remote provider APIs are https, so build without --no-default-features",
                    up.name
                )));
            }
            if up.max_concurrency == 0 {
                return Err(ConfigError(format!(
                    "upstream {:?}: max_concurrency must be at least 1",
                    up.name
                )));
            }
            if up.weight == 0 {
                return Err(ConfigError(format!(
                    "upstream {:?}: weight must be at least 1",
                    up.name
                )));
            }
        }
        if self.upstreams.iter().all(|u| u.fallback_only) {
            return Err(ConfigError(
                "every upstream is fallback_only: no upstream would ever be picked".into(),
            ));
        }

        for (name, pool) in &self.pools {
            if pool.members.is_empty() {
                return Err(ConfigError(format!("pool {name:?} has no members")));
            }
            if self.upstreams.iter().any(|u| u.name == *name) {
                return Err(ConfigError(format!(
                    "pool {name:?} shares its name with an upstream; pick a different one"
                )));
            }
            for m in &pool.members {
                if !seen.contains_key(&m.upstream) {
                    return Err(ConfigError(format!(
                        "pool {name:?} refers to unknown upstream {:?}",
                        m.upstream
                    )));
                }
                if m.model.trim().is_empty() {
                    return Err(ConfigError(format!(
                        "pool {name:?}: member on upstream {:?} has an empty model",
                        m.upstream
                    )));
                }
                if m.weight == 0 {
                    return Err(ConfigError(format!(
                        "pool {name:?}: member {:?} has weight 0",
                        m.model
                    )));
                }
            }
        }

        for (from, to) in &self.alias {
            if from == to {
                return Err(ConfigError(format!(
                    "alias {from:?} points at itself; remove it"
                )));
            }
        }
        if self.server.auth.require_auth && self.client_keys().is_empty() {
            return Err(ConfigError(
                "server.auth.require_auth is set but no api_keys are configured: every request \
                 would be rejected"
                    .into(),
            ));
        }
        if self.server.max_body_bytes == 0 {
            return Err(ConfigError("server.max_body_bytes must be non-zero".into()));
        }
        if self.translate.default_max_tokens == 0 {
            return Err(ConfigError(
                "translate.default_max_tokens must be non-zero: Anthropic requires max_tokens"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Resolve a client-facing model name through the alias table.
    /// Aliases are followed transitively, with a hard stop on cycles.
    pub fn resolve_alias<'a>(&'a self, model: &'a str) -> &'a str {
        let mut current = model;
        for _ in 0..8 {
            match self.alias.get(current) {
                Some(next) => current = next.as_str(),
                None => return current,
            }
        }
        current
    }

    /// Every client key, from the config file and from the environment.
    pub fn client_keys(&self) -> Vec<String> {
        let mut keys = self.server.auth.api_keys.clone();
        for var in &self.server.auth.api_key_envs {
            if let Ok(v) = std::env::var(var) {
                if !v.is_empty() {
                    keys.push(v);
                }
            }
        }
        keys
    }
}

impl UpstreamConfig {
    /// A provider with defaults everywhere except its name and URL. Used when
    /// building configuration from the environment.
    pub fn stub(name: &str, url: &str) -> Self {
        Self {
            name: name.to_owned(),
            url: url.to_owned(),
            protocol: Protocol::default(),
            api_key: None,
            api_key_env: None,
            weight: default_weight(),
            max_concurrency: default_max_concurrency(),
            models: Vec::new(),
            fallback_only: false,
            headers: BTreeMap::new(),
        }
    }

    /// The API key for this provider, if any.
    pub fn resolve_key(&self) -> Option<String> {
        if let Some(var) = &self.api_key_env {
            if let Ok(v) = std::env::var(var) {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        self.api_key.clone().filter(|k| !k.is_empty())
    }

    /// Join the base URL with a path such as `/chat/completions`.
    pub fn join(&self, path: &str) -> String {
        let base = self.url.trim_end_matches('/');
        if path.starts_with('/') {
            format!("{base}{path}")
        } else {
            format!("{base}/{path}")
        }
    }
}

fn default_listen() -> SocketAddr {
    "0.0.0.0:8080"
        .parse()
        .expect("valid default listen address")
}
fn default_worker_threads() -> usize {
    2
}
fn default_max_body_bytes() -> usize {
    8 * 1024 * 1024
}
fn default_max_translate_bytes() -> usize {
    8 * 1024 * 1024
}
fn default_upstream_header_timeout() -> u64 {
    120
}
fn default_queue_timeout() -> u64 {
    60
}
fn default_pool_idle_timeout() -> u64 {
    90
}
fn default_log_level() -> String {
    "info".into()
}
fn default_max_tokens() -> u64 {
    4096
}
fn default_anthropic_version() -> String {
    DEFAULT_ANTHROPIC_VERSION.into()
}
fn default_retry_statuses() -> Vec<u16> {
    vec![408, 409, 429, 500, 502, 503, 504, 529]
}
fn default_health_interval() -> u64 {
    60
}
fn default_health_timeout() -> u64 {
    10
}
fn default_health_path() -> String {
    "/models".into()
}
fn default_failure_threshold() -> u32 {
    3
}
fn default_success_threshold() -> u32 {
    2
}
fn default_cooldown() -> u64 {
    30
}
fn default_max_cooldown() -> u64 {
    300
}
fn default_weight() -> u32 {
    1
}
fn default_max_concurrency() -> usize {
    8
}
