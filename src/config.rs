//! Configuration loading and validation.
//!
//! The whole configuration is a single TOML file. Anything that is not set
//! falls back to a default that is sane on a 1 GB single-board computer.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Top level configuration document.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub balance: BalanceConfig,
    #[serde(default)]
    pub health: HealthConfig,
    /// Client-facing model name -> upstream model name.
    #[serde(default)]
    pub alias: BTreeMap<String, String>,
    #[serde(default, rename = "upstream")]
    pub upstreams: Vec<UpstreamConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address to bind the HTTP listener to.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Tokio worker threads. 0 means "one per core", which is rarely what you
    /// want on a 4-core SBC that is also running the model.
    #[serde(default = "default_worker_threads")]
    pub worker_threads: usize,
    /// Largest request body accepted from a client, in bytes.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// How long to wait for an upstream to return response *headers*. Body
    /// streaming is not bounded by this: a long generation is not a timeout.
    #[serde(default = "default_upstream_header_timeout")]
    pub upstream_header_timeout_secs: u64,
    /// How long a request may wait for a free slot when every upstream that
    /// serves the model is already at its concurrency limit.
    #[serde(default = "default_queue_timeout")]
    pub queue_timeout_secs: u64,
    /// Idle keep-alive timeout for pooled upstream connections.
    #[serde(default = "default_pool_idle_timeout")]
    pub pool_idle_timeout_secs: u64,
    /// Log level: error, warn, info, debug, trace.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub auth: AuthConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            worker_threads: default_worker_threads(),
            max_body_bytes: default_max_body_bytes(),
            upstream_header_timeout_secs: default_upstream_header_timeout(),
            queue_timeout_secs: default_queue_timeout(),
            pool_idle_timeout_secs: default_pool_idle_timeout(),
            log_level: default_log_level(),
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Keys clients may present as `Authorization: Bearer <key>`.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Environment variables to read additional client keys from.
    #[serde(default)]
    pub api_key_envs: Vec<String>,
    /// Require a valid key on `/v1/*`. Admin endpoints follow this too.
    #[serde(default)]
    pub require_auth: bool,
}

/// How a request is assigned to one of the upstreams that can serve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Plain rotation over the healthy candidates.
    RoundRobin,
    /// Fewest requests in flight, ties broken by rotation.
    LeastConn,
    /// Rotation biased by the `weight` of each upstream.
    Weighted,
    /// Power of two choices, compared on time-to-first-byte. A good default:
    /// it tracks real capacity without the herd behaviour of pure least-conn.
    #[default]
    P2cLatency,
    /// Always the first healthy upstream in declaration order. Useful when one
    /// box is "the" box and the rest are spillover.
    FirstAvailable,
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Strategy::RoundRobin => "round-robin",
            Strategy::LeastConn => "least-conn",
            Strategy::Weighted => "weighted",
            Strategy::P2cLatency => "p2c-latency",
            Strategy::FirstAvailable => "first-available",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BalanceConfig {
    #[serde(default)]
    pub strategy: Strategy,
    /// Extra upstreams to try after the first one fails. 2 means at most three
    /// attempts in total.
    #[serde(default = "default_retries")]
    pub retries: usize,
    /// Upstream status codes that make us try the next upstream instead of
    /// passing the response through.
    #[serde(default = "default_retry_statuses")]
    pub retry_on_status: Vec<u16>,
}

impl Default for BalanceConfig {
    fn default() -> Self {
        Self {
            strategy: Strategy::default(),
            retries: default_retries(),
            retry_on_status: default_retry_statuses(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthConfig {
    /// Seconds between probes. 0 disables active probing; upstreams are then
    /// only judged by live traffic.
    #[serde(default = "default_health_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_health_timeout")]
    pub timeout_secs: u64,
    /// Probe path, appended to the upstream base URL.
    #[serde(default = "default_health_path")]
    pub path: String,
    /// Consecutive failures before an upstream is taken out of rotation.
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    /// Consecutive successes before a recovering upstream is used again.
    #[serde(default = "default_success_threshold")]
    pub success_threshold: u32,
    /// How long an upstream stays out of rotation once it trips.
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// Stable identifier, used in logs, metrics and the admin API.
    pub name: String,
    /// Base URL including any version prefix, e.g. `http://127.0.0.1:11434/v1`.
    pub url: String,
    /// Bearer token sent to this upstream.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Environment variable holding the bearer token. Takes precedence over
    /// `api_key`, so secrets need not live in the config file.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Relative share of traffic under the `weighted` strategy.
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Requests this upstream may process at once. The single most important
    /// knob on an SBC: two concurrent 7B generations will swap a 1 GB board to
    /// death, so the queue in front is a feature.
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
    /// Models this upstream serves. Empty means "discover from
    /// `GET {url}/models` and refresh on every health probe".
    #[serde(default)]
    pub models: Vec<String>,
    /// Never route to this upstream unless the client asked for a model that
    /// only it serves. Handy for a paid cloud fallback behind local boards.
    #[serde(default)]
    pub fallback_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Parse a configuration document and validate it.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(text).map_err(|e| ConfigError(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError(format!("{}: {e}", path.display())))?;
        Self::from_toml(&text)
    }

    /// Reject configurations that would fail confusingly at runtime.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.upstreams.is_empty() {
            return Err(ConfigError(
                "no [[upstream]] configured: mini-router has nothing to route to".into(),
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
                    "upstream {:?}: https requires the `tls` feature, which this build does not have",
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
        for (from, to) in &self.alias {
            if from == to {
                return Err(ConfigError(format!(
                    "alias {from:?} points at itself; remove it"
                )));
            }
        }
        if self.server.auth.require_auth && self.client_keys().is_empty() {
            return Err(ConfigError(
                "server.auth.require_auth is set but no api_keys are configured:                  every request would be rejected"
                    .into(),
            ));
        }
        if self.server.max_body_bytes == 0 {
            return Err(ConfigError("server.max_body_bytes must be non-zero".into()));
        }
        Ok(())
    }

    /// Resolve a client-facing model name to the upstream model name.
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
    /// The bearer token for this upstream, if any.
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
fn default_retries() -> usize {
    2
}
fn default_retry_statuses() -> Vec<u16> {
    vec![408, 429, 500, 502, 503, 504]
}
fn default_health_interval() -> u64 {
    15
}
fn default_health_timeout() -> u64 {
    5
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
fn default_weight() -> u32 {
    1
}
fn default_max_concurrency() -> usize {
    1
}
