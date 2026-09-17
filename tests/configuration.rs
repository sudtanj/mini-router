//! Configuration, end to end.
//!
//! mini-router has no configuration file: the environment is the whole of it.
//! These tests cover the public surface of that -- the defaults a bare
//! deployment gets, the validation rules, and whether the compose file and
//! `.env.example` this repo ships still say things mini-router understands.

use mini_router::config::{Config, PoolConfig, PoolMember, Spillover, Strategy, UpstreamConfig};
use mini_router::env;
use mini_router::protocol::Protocol;

fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn load(pairs: &[(&str, &str)]) -> Config {
    env::load(&vars(pairs))
        .unwrap_or_else(|e| panic!("should resolve: {e}"))
        .config
}

const A_PROVIDER: (&str, &str) = (
    "MINI_ROUTER_PROVIDER_OPENAI_URL",
    "http://openai.invalid/v1",
);

// ---------------------------------------------------------------------------
// What a bare deployment gets
// ---------------------------------------------------------------------------

#[test]
fn one_provider_variable_is_a_complete_configuration() {
    let cfg = load(&[A_PROVIDER]);

    assert_eq!(cfg.upstreams.len(), 1);
    assert_eq!(cfg.upstreams[0].name, "openai");
    assert_eq!(cfg.server.listen.to_string(), "0.0.0.0:8080");
    assert_eq!(cfg.server.worker_threads, 2);
    // Providers are assumed OpenAI-compatible, which most of them are.
    assert_eq!(cfg.upstreams[0].protocol, Protocol::Openai);
    assert_eq!(cfg.upstreams[0].max_concurrency, 8);
    // Declared order is honoured, and anything that fails spills over.
    assert_eq!(cfg.balance.strategy, Strategy::Priority);
    assert_eq!(cfg.balance.spillover, Spillover::AnyError);
    assert_eq!(cfg.balance.max_attempts, 0, "0 means try every candidate");
    assert_eq!(cfg.translate.default_max_tokens, 4096);
    assert_eq!(cfg.translate.anthropic_version, "2023-06-01");
    // Observability is on unless switched off; there is no UI behind either.
    assert!(cfg.server.admin);
    assert!(cfg.server.metrics);
    assert!(!cfg.server.auth.require_auth);
}

#[test]
fn spillover_defaults_to_treating_every_non_success_as_a_failure() {
    let cfg = load(&[A_PROVIDER]);
    for status in [400, 401, 403, 404, 408, 409, 422, 429, 500, 502, 503, 529] {
        assert!(
            cfg.balance.should_spill(status),
            "{status} should spill over"
        );
    }
    for status in [200, 201, 204] {
        assert!(!cfg.balance.should_spill(status), "{status} is a success");
    }
}

#[test]
fn status_list_spillover_consults_only_its_list() {
    let cfg = load(&[
        A_PROVIDER,
        ("MINI_ROUTER_SPILLOVER", "status-list"),
        ("MINI_ROUTER_RETRY_ON_STATUS", "429,503"),
    ]);
    assert!(cfg.balance.should_spill(429));
    assert!(cfg.balance.should_spill(503));
    assert!(!cfg.balance.should_spill(400));
    assert!(!cfg.balance.should_spill(500));
}

#[test]
fn every_strategy_name_parses_and_renders_back() {
    for (name, expected) in [
        ("priority", Strategy::Priority),
        ("round-robin", Strategy::RoundRobin),
        ("least-conn", Strategy::LeastConn),
        ("weighted", Strategy::Weighted),
        ("p2c-latency", Strategy::P2cLatency),
    ] {
        let cfg = load(&[A_PROVIDER, ("MINI_ROUTER_STRATEGY", name)]);
        assert_eq!(cfg.balance.strategy, expected, "{name}");
        assert_eq!(cfg.balance.strategy.to_string(), name);
    }
}

#[test]
fn a_whole_deployment_in_one_block_of_variables() {
    let cfg = load(&[
        ("MINI_ROUTER_LISTEN", "127.0.0.1:9000"),
        ("MINI_ROUTER_WORKER_THREADS", "1"),
        ("MINI_ROUTER_REQUIRE_AUTH", "true"),
        ("MINI_ROUTER_API_KEYS", "sk-a,sk-b"),
        ("MINI_ROUTER_STRATEGY", "weighted"),
        ("MINI_ROUTER_MAX_ATTEMPTS", "2"),
        ("MINI_ROUTER_DEFAULT_MAX_TOKENS", "1024"),
        ("MINI_ROUTER_ANTHROPIC_VERSION", "2024-10-22"),
        (
            "MINI_ROUTER_PROVIDER_OPENAI_URL",
            "http://openai.invalid/v1",
        ),
        ("MINI_ROUTER_PROVIDER_OPENAI_MAX_CONCURRENCY", "16"),
        (
            "MINI_ROUTER_PROVIDER_ANTHROPIC_URL",
            "http://ant.invalid/v1",
        ),
        ("MINI_ROUTER_PROVIDER_ANTHROPIC_PROTOCOL", "anthropic"),
        ("MINI_ROUTER_PROVIDER_ANTHROPIC_FALLBACK_ONLY", "true"),
        ("MINI_ROUTER_PROVIDER_ANTHROPIC_HEADERS", "x-custom=value"),
        (
            "MINI_ROUTER_POOL_FAST",
            "openai:gpt-4o-mini,anthropic:claude-haiku-4-5",
        ),
        ("MINI_ROUTER_POOL_FAST_STRATEGY", "round-robin"),
        ("MINI_ROUTER_POOL_FAST_WEIGHTS", "3,1"),
        ("MINI_ROUTER_POOL_FAST_DESCRIPTION", "cheap and quick"),
        ("MINI_ROUTER_ALIASES", "gpt-3.5-turbo=fast"),
    ]);

    assert_eq!(cfg.server.listen.to_string(), "127.0.0.1:9000");
    assert_eq!(cfg.server.auth.api_keys, ["sk-a", "sk-b"]);
    assert_eq!(cfg.balance.strategy, Strategy::Weighted);
    assert_eq!(cfg.balance.max_attempts, 2);
    assert_eq!(cfg.translate.anthropic_version, "2024-10-22");

    let ant = cfg
        .upstreams
        .iter()
        .find(|u| u.name == "anthropic")
        .unwrap();
    assert_eq!(ant.protocol, Protocol::Anthropic);
    assert!(ant.fallback_only);
    assert_eq!(ant.headers.get("x-custom").unwrap(), "value");

    let pool = cfg.pools.get("fast").unwrap();
    assert_eq!(pool.members.len(), 2);
    assert_eq!(pool.members[0].weight, 3);
    assert_eq!(pool.strategy, Some(Strategy::RoundRobin));
    assert_eq!(pool.description.as_deref(), Some("cheap and quick"));
    // An alias may point at a pool.
    assert_eq!(cfg.resolve_alias("gpt-3.5-turbo"), "fast");
}

// ---------------------------------------------------------------------------
// Validation, including rules the environment cannot express
// ---------------------------------------------------------------------------

/// A minimal valid configuration to mutate into an invalid one.
fn valid() -> Config {
    let mut cfg = Config::default();
    cfg.upstreams
        .push(UpstreamConfig::stub("openai", "http://openai.invalid/v1"));
    cfg
}

fn rejects(cfg: &Config) -> String {
    cfg.validate()
        .map(|_| String::new())
        .unwrap_err()
        .to_string()
}

#[test]
fn a_configuration_with_no_providers_says_how_to_fix_it() {
    let err = rejects(&Config::default());
    assert!(err.contains("no providers configured"), "{err}");
    assert!(err.contains("OPENAI_API_KEY"), "{err}");
    assert!(err.contains("MINI_ROUTER_PROVIDER_<NAME>_URL"), "{err}");
}

#[test]
fn duplicate_provider_names_are_rejected() {
    // Not reachable through the environment, where names are keys, but the
    // library API allows it and the router could not resolve a pool member.
    let mut cfg = valid();
    cfg.upstreams
        .push(UpstreamConfig::stub("openai", "http://other.invalid/v1"));
    assert!(rejects(&cfg).contains("duplicate upstream name"));
}

#[test]
fn a_url_without_a_scheme_is_rejected() {
    let mut cfg = valid();
    cfg.upstreams[0].url = "openai.invalid/v1".into();
    assert!(rejects(&cfg).contains("http://"));
}

#[test]
fn zero_concurrency_and_zero_weight_are_rejected() {
    let mut cfg = valid();
    cfg.upstreams[0].max_concurrency = 0;
    assert!(rejects(&cfg).contains("must be at least 1"));

    let mut cfg = valid();
    cfg.upstreams[0].weight = 0;
    assert!(rejects(&cfg).contains("must be at least 1"));
}

#[test]
fn a_shelf_that_is_entirely_fallback_is_rejected() {
    let mut cfg = valid();
    cfg.upstreams[0].fallback_only = true;
    assert!(rejects(&cfg).contains("fallback_only"));
}

#[test]
fn a_pool_named_after_a_provider_is_rejected() {
    // Otherwise it is ambiguous which one a client means.
    let mut cfg = valid();
    cfg.pools.insert(
        "openai".into(),
        PoolConfig {
            members: vec![PoolMember {
                upstream: "openai".into(),
                model: "m".into(),
                weight: 1,
            }],
            ..Default::default()
        },
    );
    assert!(rejects(&cfg).contains("shares its name"));
}

#[test]
fn auth_without_keys_is_rejected() {
    let mut cfg = valid();
    cfg.server.auth.require_auth = true;
    let err = rejects(&cfg);
    assert!(err.contains("no api_keys"), "{err}");
}

#[test]
fn zero_default_max_tokens_is_rejected() {
    // Anthropic requires max_tokens, so zero would make every translated
    // request fail at the provider instead of here.
    let mut cfg = valid();
    cfg.translate.default_max_tokens = 0;
    assert!(rejects(&cfg).contains("max_tokens"));
}

#[test]
fn a_self_referencing_alias_is_rejected() {
    let mut cfg = valid();
    cfg.alias.insert("a".into(), "a".into());
    assert!(rejects(&cfg).contains("itself"));
}

#[test]
fn alias_chains_terminate_even_when_they_loop() {
    let mut cfg = valid();
    cfg.alias.insert("a".into(), "b".into());
    cfg.alias.insert("b".into(), "c".into());
    cfg.alias.insert("c".into(), "a".into());
    // The value does not matter; not hanging does.
    assert!(["a", "b", "c"].contains(&cfg.resolve_alias("a")));
}

/// The `tls` feature is what makes an https provider reachable, and every
/// remote provider is https, so the configuration layer refuses one in a build
/// that cannot honour it -- rather than failing per-request at 3am.
#[test]
fn https_providers_track_the_tls_feature() {
    let mut cfg = valid();
    cfg.upstreams[0].url = "https://api.openai.com/v1".into();
    if cfg!(feature = "tls") {
        assert!(cfg.validate().is_ok());
    } else {
        assert!(rejects(&cfg).contains("tls"));
    }
}

#[test]
fn base_url_join_is_slash_safe() {
    let up = UpstreamConfig::stub("a", "http://api.openai.invalid/v1/");
    assert_eq!(
        up.join("/chat/completions"),
        "http://api.openai.invalid/v1/chat/completions"
    );
    assert_eq!(up.join("models"), "http://api.openai.invalid/v1/models");
}

// ---------------------------------------------------------------------------
// The files this repo ships must still say things mini-router understands
// ---------------------------------------------------------------------------

/// Pull `KEY: value` pairs out of the `environment:` block of a compose file,
/// including the commented-out examples, which are documentation too.
fn compose_environment(yaml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut indent = None;
    for line in yaml.lines() {
        let trimmed = line.trim_start();
        let depth = line.len() - trimmed.len();
        match indent {
            None => {
                if trimmed == "environment:" {
                    indent = Some(depth);
                }
            }
            Some(block) => {
                if !trimmed.is_empty() && depth <= block {
                    break; // out of the environment block
                }
                let content = trimmed.trim_start_matches("# ").trim_start_matches('#');
                let Some((key, value)) = content.split_once(american_colon()) else {
                    continue;
                };
                let key = key.trim();
                if !key
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                {
                    continue;
                }
                let value = value.trim().trim_matches('"');
                // `${VAR}` and `${VAR:-default}` are compose's own syntax; a
                // real deployment substitutes them, so stand in a value.
                let value = if value.starts_with("${") {
                    if value.contains(":-}") {
                        ""
                    } else {
                        "sk-test"
                    }
                } else {
                    value
                };
                out.push((key.to_string(), value.to_string()));
            }
        }
    }
    out
}

fn american_colon() -> char {
    ':'
}

/// Make a parsed example self-contained so it can be validated offline.
///
/// The shipped examples lean on autodetection, whose provider URLs are all
/// https and so need the `tls` feature. Declaring every provider the pools
/// mention, over plain http, lets both builds check the file -- and it adapts
/// automatically when the example changes.
fn make_self_contained(pairs: &mut Vec<(String, String)>) {
    let mut needed: Vec<String> = Vec::new();
    for (key, value) in pairs.iter() {
        let Some(rest) = key.strip_prefix("MINI_ROUTER_POOL_") else {
            continue;
        };
        if rest.ends_with("_STRATEGY")
            || rest.ends_with("_WEIGHTS")
            || rest.ends_with("_DESCRIPTION")
        {
            continue;
        }
        for member in value.split(',') {
            if let Some((provider, _)) = member.trim().split_once(':') {
                let name = provider.trim();
                if !name.is_empty() && !needed.iter().any(|n| n == name) {
                    needed.push(name.to_owned());
                }
            }
        }
    }
    // Always have at least one provider, even for a file with no pools.
    if needed.is_empty() {
        needed.push("localtest".into());
    }
    for name in needed {
        let key = format!(
            "MINI_ROUTER_PROVIDER_{}_URL",
            name.to_uppercase().replace('-', "_")
        );
        if !pairs.iter().any(|(k, _)| *k == key) {
            pairs.push((key, format!("http://{name}.invalid/v1")));
        }
    }
    pairs.push(("MINI_ROUTER_AUTODETECT".into(), "off".into()));
}

#[test]
fn the_shipped_compose_file_only_uses_settings_that_exist() {
    let yaml = include_str!("../docker-compose.yml");
    let mut pairs = compose_environment(yaml);
    assert!(
        pairs
            .iter()
            .any(|(k, _)| k.starts_with("MINI_ROUTER_POOL_")),
        "the example should demonstrate a pool; parsed {pairs:#?}"
    );
    make_self_contained(&mut pairs);

    if let Err(e) = env::load(&pairs) {
        panic!("docker-compose.yml has drifted from what mini-router accepts: {e}");
    }
}

#[test]
fn the_shipped_env_example_only_uses_settings_that_exist() {
    let text = include_str!("../.env.example");
    let mut pairs: Vec<(String, String)> = text
        .lines()
        .map(|l| l.trim().trim_start_matches("# ").trim_start_matches('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, _)| {
            k.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        })
        .collect();
    assert!(
        pairs.iter().any(|(k, _)| k == "OPENAI_API_KEY"),
        "parsed nothing useful: {pairs:#?}"
    );
    make_self_contained(&mut pairs);

    if let Err(e) = env::load(&pairs) {
        panic!(".env.example has drifted from what mini-router accepts: {e}");
    }
}
