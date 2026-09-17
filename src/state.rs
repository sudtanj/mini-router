//! Shared application state: the provider pool, the HTTP client, the balancer.

use std::sync::Arc;
use std::time::Duration;

use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use crate::balance::{Balancer, Target};
use crate::config::{Config, Strategy};
use crate::metrics::Metrics;
use crate::protocol::{Endpoint, Protocol};
use crate::upstream::{Upstream, UpstreamStatus};

#[cfg(feature = "tls")]
pub type Connector = hyper_rustls::HttpsConnector<HttpConnector>;
#[cfg(not(feature = "tls"))]
pub type Connector = HttpConnector;

/// The pooled client used for every provider call.
pub type HttpClient = Client<Connector, axum::body::Body>;

pub type SharedState = Arc<AppState>;

/// The candidates for one request, in the order they should be tried.
#[derive(Debug)]
pub struct Plan {
    pub targets: Vec<Target>,
    /// True when the client asked for a pool name rather than a model id.
    pub from_pool: bool,
}

#[derive(Debug)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub upstreams: Vec<Arc<Upstream>>,
    pub balancer: Balancer,
    pub metrics: Metrics,
    pub client: HttpClient,
    /// Keys accepted from clients. Empty means auth is effectively off.
    pub client_keys: Vec<String>,
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
        let balancer = Balancer::new(cfg.balance.strategy);
        let client = build_client(&cfg);
        Self {
            upstreams,
            balancer,
            metrics: Metrics::new(),
            client,
            client_keys,
            cfg: Arc::new(cfg),
        }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<Upstream>> {
        self.upstreams.iter().find(|u| u.name == name)
    }

    /// Work out where a request could go, in the order to try.
    ///
    /// Three cases, in order of precedence:
    ///
    /// 1. The name is a pool -> its members, which each carry their own
    ///    provider-side model id. This is the case that load-balances across
    ///    *different* models on *different* providers.
    /// 2. The name is a model some provider reports -> those providers, asked
    ///    for that same model id.
    /// 3. No model at all (or an endpoint that does not take one) -> every
    ///    provider that can speak the dialect.
    ///
    /// `endpoint` matters because only chat can be translated between
    /// dialects: an embeddings request has to reach a provider that already
    /// speaks the protocol it was written in.
    pub async fn plan(&self, model: Option<&str>, ingress: Protocol, endpoint: &Endpoint) -> Plan {
        let translatable = matches!(endpoint, Endpoint::Chat);

        if let Some(name) = model {
            if let Some(pool) = self.cfg.pools.get(name) {
                let mut targets = Vec::new();
                for m in &pool.members {
                    let Some(up) = self.get(&m.upstream) else {
                        continue;
                    };
                    if !up.is_available() {
                        continue;
                    }
                    if !translatable && up.cfg.protocol != ingress {
                        continue;
                    }
                    targets.push(Target::new(up.clone(), m.model.clone(), m.weight));
                }
                return Plan {
                    targets: self.balancer.order(pool.strategy, targets),
                    from_pool: true,
                };
            }
        }

        let mut primary = Vec::new();
        let mut fallback = Vec::new();
        for up in &self.upstreams {
            if !up.is_available() {
                continue;
            }
            if !translatable && up.cfg.protocol != ingress {
                continue;
            }
            if let Some(m) = model {
                if !up.serves(m).await {
                    continue;
                }
            }
            let target = Target::new(
                up.clone(),
                model.unwrap_or_default().to_owned(),
                up.cfg.weight,
            );
            if up.cfg.fallback_only {
                fallback.push(target);
            } else {
                primary.push(target);
            }
        }
        // Held-back providers are only considered when nothing else can serve
        // the request at all.
        let targets = if primary.is_empty() {
            fallback
        } else {
            primary
        };
        Plan {
            targets: self.balancer.order(None, targets),
            from_pool: false,
        }
    }

    /// The strategy actually in force for a given model name.
    pub fn strategy_for(&self, model: Option<&str>) -> Strategy {
        model
            .and_then(|m| self.cfg.pools.get(m))
            .and_then(|p| p.strategy)
            .unwrap_or(self.cfg.balance.strategy)
    }

    /// Whether any provider is currently in rotation.
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
        // Remote providers are reached over TLS across the internet; a warm
        // pool saves a handshake on every request.
        .pool_max_idle_per_host(4)
        .build(connector)
}

/// Install the rustls crypto provider. Safe to call more than once.
#[cfg(feature = "tls")]
pub fn init_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(not(feature = "tls"))]
pub fn init_crypto() {}
