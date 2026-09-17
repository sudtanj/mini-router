//! Shared application state: the upstream pool, the HTTP client, the balancer.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use crate::balance::Balancer;
use crate::config::Config;
use crate::metrics::Metrics;
use crate::upstream::{Upstream, UpstreamStatus};

#[cfg(feature = "tls")]
pub type Connector = hyper_rustls::HttpsConnector<HttpConnector>;
#[cfg(not(feature = "tls"))]
pub type Connector = HttpConnector;

/// The pooled client used for every upstream call.
pub type HttpClient = Client<Connector, axum::body::Body>;

pub type SharedState = Arc<AppState>;

#[derive(Debug)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub upstreams: Vec<Arc<Upstream>>,
    pub balancer: Balancer,
    pub metrics: Metrics,
    pub client: HttpClient,
    /// Keys accepted from clients. Empty means auth is effectively off.
    pub client_keys: Vec<String>,
    pub retry_statuses: HashSet<u16>,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        let upstreams = cfg
            .upstreams
            .iter()
            .cloned()
            .map(|u| Arc::new(Upstream::new(u)))
            .collect::<Vec<_>>();
        let client_keys = cfg.client_keys();
        let retry_statuses = cfg.balance.retry_on_status.iter().copied().collect();
        let balancer = Balancer::new(cfg.balance.strategy);
        let client = build_client(&cfg);
        Self {
            upstreams,
            balancer,
            metrics: Metrics::new(),
            client,
            client_keys,
            retry_statuses,
            cfg: Arc::new(cfg),
        }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<Upstream>> {
        self.upstreams.iter().find(|u| u.name == name)
    }

    /// Upstreams that could take this request right now.
    ///
    /// `model` is the upstream-side model name (aliases already resolved).
    /// `exclude` holds upstreams already tried for this request.
    ///
    /// `fallback_only` upstreams are held back: they are returned only when no
    /// ordinary upstream can serve the model, which is how a cloud provider
    /// sits behind a shelf of boards without stealing their traffic.
    pub async fn candidates(&self, model: Option<&str>, exclude: &[&str]) -> Vec<Arc<Upstream>> {
        let mut primary = Vec::new();
        let mut fallback = Vec::new();
        for up in &self.upstreams {
            if exclude.contains(&up.name.as_str()) || !up.is_available() {
                continue;
            }
            if let Some(m) = model {
                if !up.serves(m).await {
                    continue;
                }
            }
            if up.cfg.fallback_only {
                fallback.push(up.clone());
            } else {
                primary.push(up.clone());
            }
        }
        if primary.is_empty() {
            fallback
        } else {
            primary
        }
    }

    /// Whether any upstream is currently in rotation.
    pub fn any_available(&self) -> bool {
        self.upstreams.iter().any(|u| u.is_available())
    }

    pub async fn snapshots(&self) -> Vec<UpstreamStatus> {
        let mut out = Vec::with_capacity(self.upstreams.len());
        for u in &self.upstreams {
            out.push(u.snapshot().await);
        }
        out
    }
}

fn build_client(cfg: &Config) -> HttpClient {
    let mut http = HttpConnector::new();
    http.set_nodelay(true);
    http.set_connect_timeout(Some(Duration::from_secs(10)));
    // Keep connections warm: on a local network the TCP handshake is a real
    // slice of the time-to-first-token for short prompts.
    http.set_keepalive(Some(Duration::from_secs(60)));
    http.enforce_http(false);

    #[cfg(feature = "tls")]
    let connector = {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .wrap_connector(http)
    };
    #[cfg(not(feature = "tls"))]
    let connector = http;

    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(cfg.server.pool_idle_timeout_secs))
        .pool_max_idle_per_host(2)
        .build(connector)
}

/// Install the rustls crypto provider. Safe to call more than once.
#[cfg(feature = "tls")]
pub fn init_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(not(feature = "tls"))]
pub fn init_crypto() {}
