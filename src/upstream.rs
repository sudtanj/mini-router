//! Per-upstream runtime state: health, load and latency.
//!
//! Everything here is lock-free apart from the model list, which changes only
//! when a health probe discovers something new. The hot path (pick an upstream)
//! touches atomics only.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{RwLock, Semaphore};

use crate::config::UpstreamConfig;
use crate::protocol::Protocol;
use crate::util::now_millis;

/// Smoothing factor for the latency EWMA, in percent of the new sample.
const EWMA_ALPHA_PCT: u64 = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    /// Taking traffic.
    Up,
    /// Out of rotation; retried once the cooldown expires.
    Down,
    /// Coming back: probing succeeded at least once but not enough times yet.
    Probing,
}

impl Health {
    fn from_u8(v: u8) -> Health {
        match v {
            0 => Health::Up,
            1 => Health::Down,
            _ => Health::Probing,
        }
    }
    fn as_u8(self) -> u8 {
        match self {
            Health::Up => 0,
            Health::Down => 1,
            Health::Probing => 2,
        }
    }
}

/// A single upstream OpenAI-compatible server.
#[derive(Debug)]
pub struct Upstream {
    pub name: String,
    pub cfg: UpstreamConfig,
    /// Bearer token, resolved once at startup.
    pub api_key: Option<String>,
    /// Permits gate how many requests this box handles at once.
    pub permits: Arc<Semaphore>,
    /// Models served, either configured or discovered.
    models: RwLock<Vec<String>>,
    health: AtomicU32,
    consecutive_failures: AtomicU32,
    consecutive_successes: AtomicU32,
    /// Wall-clock millis before which a downed upstream is not retried.
    retry_after_ms: AtomicU64,
    inflight: AtomicUsize,
    /// EWMA of time-to-first-byte, in microseconds.
    ewma_us: AtomicU64,
    pub total_requests: AtomicU64,
    pub total_failures: AtomicU64,
    pub last_error: RwLock<Option<String>>,
}

impl Upstream {
    pub fn new(cfg: UpstreamConfig) -> Self {
        let api_key = cfg.resolve_key();
        let permits = Arc::new(Semaphore::new(cfg.max_concurrency));
        let models = RwLock::new(cfg.models.clone());
        Self {
            name: cfg.name.clone(),
            api_key,
            permits,
            models,
            // Start optimistic: a router that refuses traffic until the first
            // probe lands is useless on a box that boots everything at once.
            health: AtomicU32::new(Health::Up.as_u8() as u32),
            consecutive_failures: AtomicU32::new(0),
            consecutive_successes: AtomicU32::new(0),
            retry_after_ms: AtomicU64::new(0),
            inflight: AtomicUsize::new(0),
            ewma_us: AtomicU64::new(0),
            total_requests: AtomicU64::new(0),
            total_failures: AtomicU64::new(0),
            last_error: RwLock::new(None),
            cfg,
        }
    }

    pub fn health(&self) -> Health {
        Health::from_u8(self.health.load(Ordering::Relaxed) as u8)
    }

    fn set_health(&self, h: Health) {
        self.health.store(h.as_u8() as u32, Ordering::Relaxed);
    }

    /// Whether this upstream may be picked right now. A downed upstream becomes
    /// eligible again once its cooldown expires, which is what lets a box that
    /// rebooted rejoin without waiting for a probe.
    pub fn is_available(&self) -> bool {
        match self.health() {
            Health::Up | Health::Probing => true,
            Health::Down => now_millis() >= self.retry_after_ms.load(Ordering::Relaxed),
        }
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
    }

    pub fn free_slots(&self) -> usize {
        self.permits.available_permits()
    }

    /// Time-to-first-byte EWMA in microseconds. Zero until the first sample,
    /// which callers treat as "unknown, try it".
    pub fn ewma_us(&self) -> u64 {
        self.ewma_us.load(Ordering::Relaxed)
    }

    pub fn ewma_ms(&self) -> f64 {
        self.ewma_us() as f64 / 1000.0
    }

    pub fn incr_inflight(&self) {
        self.inflight.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decr_inflight(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    /// Fold a time-to-first-byte sample into the EWMA.
    pub fn record_latency(&self, sample: Duration) {
        let sample_us = sample.as_micros().min(u64::MAX as u128) as u64;
        let prev = self.ewma_us.load(Ordering::Relaxed);
        let next = if prev == 0 {
            sample_us
        } else {
            (prev * (100 - EWMA_ALPHA_PCT) + sample_us * EWMA_ALPHA_PCT) / 100
        };
        self.ewma_us.store(next, Ordering::Relaxed);
    }

    /// A request or probe succeeded.
    pub fn record_success(&self, success_threshold: u32) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        let n = self.consecutive_successes.fetch_add(1, Ordering::Relaxed) + 1;
        if self.health() != Health::Up && n >= success_threshold.max(1) {
            self.set_health(Health::Up);
            self.consecutive_successes.store(0, Ordering::Relaxed);
            tracing::info!(upstream = %self.name, "upstream is healthy again");
        } else if self.health() == Health::Down {
            self.set_health(Health::Probing);
        }
    }

    /// Park this provider for at least `until`, regardless of the breaker.
    /// Used for a `Retry-After`: the provider has told us when to come back,
    /// and guessing differently only wastes the next request.
    pub fn park_until(&self, delay: Duration) {
        let target = now_millis() + delay.as_millis() as u64;
        self.retry_after_ms.fetch_max(target, Ordering::Relaxed);
        self.set_health(Health::Down);
        tracing::warn!(
            upstream = %self.name,
            seconds = delay.as_secs(),
            "provider asked us to back off"
        );
    }

    /// A request or probe failed. Trips the breaker once the threshold is hit.
    pub fn record_failure(&self, failure_threshold: u32, cooldown: Duration) {
        self.consecutive_successes.store(0, Ordering::Relaxed);
        self.total_failures.fetch_add(1, Ordering::Relaxed);
        let n = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= failure_threshold.max(1) && self.health() != Health::Down {
            self.set_health(Health::Down);
            self.retry_after_ms.store(
                now_millis() + cooldown.as_millis() as u64,
                Ordering::Relaxed,
            );
            tracing::warn!(
                upstream = %self.name,
                failures = n,
                cooldown_secs = cooldown.as_secs(),
                "upstream taken out of rotation"
            );
        }
    }

    pub async fn set_last_error(&self, err: Option<String>) {
        *self.last_error.write().await = err;
    }

    /// Replace the served-model list. Ignored when models were configured
    /// explicitly, so discovery can never shrink a hand-written list.
    pub async fn set_discovered_models(&self, discovered: Vec<String>) {
        if !self.cfg.models.is_empty() {
            return;
        }
        let mut guard = self.models.write().await;
        if *guard != discovered {
            tracing::debug!(upstream = %self.name, count = discovered.len(), "model list updated");
            *guard = discovered;
        }
    }

    pub async fn models(&self) -> Vec<String> {
        self.models.read().await.clone()
    }

    /// Whether this upstream can serve `model`. An upstream with no known
    /// models is treated as a wildcard: it has not told us otherwise, and
    /// refusing traffic on that basis would break llama.cpp servers that do
    /// not implement `/models`.
    pub async fn serves(&self, model: &str) -> bool {
        let models = self.models.read().await;
        models.is_empty() || models.iter().any(|m| m == model)
    }

    /// Point-in-time snapshot for the admin API.
    pub async fn snapshot(&self) -> UpstreamStatus {
        UpstreamStatus {
            name: self.name.clone(),
            url: self.cfg.url.clone(),
            protocol: self.cfg.protocol,
            health: self.health(),
            inflight: self.inflight(),
            max_concurrency: self.cfg.max_concurrency,
            weight: self.cfg.weight,
            fallback_only: self.cfg.fallback_only,
            ttfb_ewma_ms: (self.ewma_ms() * 10.0).round() / 10.0,
            total_requests: self.total_requests.load(Ordering::Relaxed),
            total_failures: self.total_failures.load(Ordering::Relaxed),
            models: self.models().await,
            last_error: self.last_error.read().await.clone(),
        }
    }
}

/// Serializable view of an upstream, returned by `GET /admin/upstreams`.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamStatus {
    pub name: String,
    pub url: String,
    pub protocol: Protocol,
    pub health: Health,
    pub inflight: usize,
    pub max_concurrency: usize,
    pub weight: u32,
    pub fallback_only: bool,
    pub ttfb_ewma_ms: f64,
    pub total_requests: u64,
    pub total_failures: u64,
    pub models: Vec<String>,
    pub last_error: Option<String>,
}
