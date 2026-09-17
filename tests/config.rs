//! Configuration parsing and validation.
//!
//! A router that starts with a subtly wrong config is worse than one that
//! refuses to start, so the invalid cases get as much attention as the valid
//! ones.

use mini_router::config::{Config, Spillover, Strategy};
use mini_router::protocol::Protocol;

/// A plain-http provider, so these cases hold in the no-TLS build too.
/// The https-specific behaviour has its own tests below.
const MINIMAL: &str = r#"
[[upstream]]
name = "openai"
url = "http://127.0.0.1:11434/v1"
"#;

#[test]
fn minimal_config_gets_sensible_defaults() {
    let cfg = Config::from_toml(MINIMAL).unwrap();
    assert_eq!(cfg.upstreams.len(), 1);
    assert_eq!(cfg.server.listen.to_string(), "0.0.0.0:8080");
    assert_eq!(cfg.server.worker_threads, 2);
    // Providers are assumed OpenAI-compatible, which most of them are.
    assert_eq!(cfg.upstreams[0].protocol, Protocol::Openai);
    // Remote APIs handle concurrency fine; the limit is about rate limits.
    assert_eq!(cfg.upstreams[0].max_concurrency, 8);
    // Declared order is honoured, and anything that fails spills over.
    assert_eq!(cfg.balance.strategy, Strategy::Priority);
    assert_eq!(cfg.balance.spillover, Spillover::AnyError);
    // 0 means "try every candidate".
    assert_eq!(cfg.balance.max_attempts, 0);
    assert_eq!(cfg.translate.default_max_tokens, 4096);
    assert_eq!(cfg.translate.anthropic_version, "2023-06-01");
    assert!(!cfg.server.auth.require_auth);
}

#[test]
fn spillover_defaults_to_treating_every_non_success_as_a_failure() {
    let cfg = Config::from_toml(MINIMAL).unwrap();
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
    let cfg = Config::from_toml(&format!(
        "[balance]\nspillover = \"status-list\"\nretry_on_status = [429, 503]\n{MINIMAL}"
    ))
    .unwrap();
    assert!(cfg.balance.should_spill(429));
    assert!(cfg.balance.should_spill(503));
    assert!(!cfg.balance.should_spill(400));
    assert!(!cfg.balance.should_spill(500));
}

#[test]
fn a_full_config_round_trips() {
    let cfg = Config::from_toml(
        r#"
        [server]
        listen = "127.0.0.1:9000"
        worker_threads = 1
        max_body_bytes = 4096
        max_translate_bytes = 65536
        upstream_header_timeout_secs = 30
        queue_timeout_secs = 10
        pool_idle_timeout_secs = 45
        log_level = "debug"

        [server.auth]
        api_keys = ["sk-a"]
        require_auth = true

        [balance]
        strategy = "weighted"
        max_attempts = 2
        spillover = "any-error"

        [health]
        interval_secs = 30
        timeout_secs = 5
        failure_threshold = 2
        success_threshold = 1
        cooldown_secs = 10
        max_cooldown_secs = 120

        [translate]
        default_max_tokens = 1024
        anthropic_version = "2024-10-22"

        [alias]
        "gpt-3.5-turbo" = "fast"

        [pool.fast]
        description = "cheap and quick"
        strategy = "round-robin"
        members = [
          { upstream = "openai", model = "gpt-4o-mini", weight = 3 },
          { upstream = "anthropic", model = "claude-haiku-4-5" },
        ]

        [[upstream]]
        name = "openai"
        url = "http://api.openai.invalid/v1"
        api_key_env = "OPENAI_API_KEY"
        max_concurrency = 16

        [[upstream]]
        name = "anthropic"
        url = "http://api.anthropic.invalid/v1"
        protocol = "anthropic"
        api_key = "sk-ant"
        fallback_only = true

        [upstream.headers]
        "x-custom" = "value"
        "#,
    )
    .unwrap();

    assert_eq!(cfg.balance.strategy, Strategy::Weighted);
    assert_eq!(cfg.balance.max_attempts, 2);
    assert_eq!(cfg.translate.default_max_tokens, 1024);
    assert_eq!(cfg.translate.anthropic_version, "2024-10-22");
    assert_eq!(cfg.upstreams[1].protocol, Protocol::Anthropic);
    assert!(cfg.upstreams[1].fallback_only);
    assert_eq!(cfg.upstreams[1].headers.get("x-custom").unwrap(), "value");

    let pool = cfg.pools.get("fast").unwrap();
    assert_eq!(pool.members.len(), 2);
    assert_eq!(pool.members[0].weight, 3);
    assert_eq!(pool.members[1].weight, 1, "weight should default to 1");
    assert_eq!(pool.strategy, Some(Strategy::RoundRobin));
    assert_eq!(pool.description.as_deref(), Some("cheap and quick"));
    // An alias may point at a pool.
    assert_eq!(cfg.resolve_alias("gpt-3.5-turbo"), "fast");
}

#[test]
fn every_strategy_and_protocol_name_parses() {
    for (name, expected) in [
        ("priority", Strategy::Priority),
        ("round-robin", Strategy::RoundRobin),
        ("least-conn", Strategy::LeastConn),
        ("weighted", Strategy::Weighted),
        ("p2c-latency", Strategy::P2cLatency),
    ] {
        let cfg = Config::from_toml(&format!("[balance]\nstrategy = \"{name}\"\n{MINIMAL}"))
            .unwrap_or_else(|e| panic!("{name} should parse: {e}"));
        assert_eq!(cfg.balance.strategy, expected);
        assert_eq!(cfg.balance.strategy.to_string(), name);
    }

    for (name, expected) in [
        ("openai", Protocol::Openai),
        ("openai-compatible", Protocol::Openai),
        ("anthropic", Protocol::Anthropic),
        ("claude", Protocol::Anthropic),
    ] {
        let cfg = Config::from_toml(&format!(
            "[[upstream]]\nname = \"a\"\nurl = \"http://x.invalid/v1\"\nprotocol = \"{name}\"\n"
        ))
        .unwrap_or_else(|e| panic!("{name} should parse: {e}"));
        assert_eq!(cfg.upstreams[0].protocol, expected);
    }
}

fn error_for(toml: &str) -> String {
    Config::from_toml(toml)
        .map(|_| String::new())
        .unwrap_err()
        .to_string()
}

#[test]
fn rejects_a_config_with_no_upstreams() {
    assert!(error_for("[server]\nlisten = \"0.0.0.0:8080\"").contains("no [[upstream]]"));
}

#[test]
fn rejects_duplicate_upstream_names() {
    let err = error_for(
        r#"
        [[upstream]]
        name = "a"
        url = "http://one.invalid/v1"
        [[upstream]]
        name = "a"
        url = "http://two.invalid/v1"
        "#,
    );
    assert!(err.contains("duplicate upstream name"), "{err}");
}

#[test]
fn rejects_a_url_without_a_scheme() {
    let err = error_for("[[upstream]]\nname = \"a\"\nurl = \"api.openai.com/v1\"\n");
    assert!(err.contains("http://"), "{err}");
}

#[test]
fn rejects_zero_concurrency_and_zero_weight() {
    for field in ["max_concurrency = 0", "weight = 0"] {
        let err = error_for(&format!(
            "[[upstream]]\nname = \"a\"\nurl = \"http://x.invalid/v1\"\n{field}\n"
        ));
        assert!(err.contains("must be at least 1"), "{field}: {err}");
    }
}

#[test]
fn rejects_a_pool_that_points_at_nothing() {
    let err = error_for(&format!(
        "[pool.fast]\nmembers = [{{ upstream = \"nope\", model = \"m\" }}]\n{MINIMAL}"
    ));
    assert!(err.contains("unknown upstream"), "{err}");

    let empty = error_for(&format!("[pool.fast]\nmembers = []\n{MINIMAL}"));
    assert!(empty.contains("no members"), "{empty}");
}

#[test]
fn rejects_a_pool_named_after_an_upstream() {
    // Otherwise it is ambiguous which one a client means.
    let err = error_for(&format!(
        "[pool.openai]\nmembers = [{{ upstream = \"openai\", model = \"m\" }}]\n{MINIMAL}"
    ));
    assert!(err.contains("shares its name"), "{err}");
}

#[test]
fn rejects_a_pool_member_without_a_model() {
    let err = error_for(&format!(
        "[pool.fast]\nmembers = [{{ upstream = \"openai\", model = \"\" }}]\n{MINIMAL}"
    ));
    assert!(err.contains("empty model"), "{err}");
}

#[test]
fn rejects_auth_without_keys() {
    let err = error_for(&format!("[server.auth]\nrequire_auth = true\n{MINIMAL}"));
    assert!(err.contains("no api_keys"), "{err}");
}

#[test]
fn rejects_zero_default_max_tokens() {
    // Anthropic requires max_tokens, so zero would make every translated
    // request fail at the provider instead of here.
    let err = error_for(&format!("[translate]\ndefault_max_tokens = 0\n{MINIMAL}"));
    assert!(err.contains("max_tokens"), "{err}");
}

#[test]
fn rejects_unknown_keys_so_typos_are_not_silent() {
    assert!(
        error_for(&format!("[balnce]\nstrategy = \"weighted\"\n{MINIMAL}"))
            .contains("unknown field")
    );
    assert!(error_for(
        "[[upstream]]\nname = \"a\"\nurl = \"http://x.invalid/v1\"\nmax_concurency = 2\n"
    )
    .contains("unknown field"));
    assert!(error_for(&format!(
        "[pool.p]\nmembers = [{{ upstream = \"openai\", model = \"m\", wieght = 2 }}]\n{MINIMAL}"
    ))
    .contains("unknown field"));
}

#[test]
fn rejects_a_self_referencing_alias() {
    let err = error_for(&format!("[alias]\n\"a\" = \"a\"\n{MINIMAL}"));
    assert!(err.contains("itself"), "{err}");
}

#[test]
fn alias_chains_terminate_even_when_they_loop() {
    let cfg = Config::from_toml(&format!(
        "[alias]\n\"a\" = \"b\"\n\"b\" = \"c\"\n\"c\" = \"a\"\n{MINIMAL}"
    ))
    .unwrap();
    // The value does not matter; not hanging does.
    assert!(["a", "b", "c"].contains(&cfg.resolve_alias("a")));
}

/// The `tls` feature is what makes an https provider reachable, and every
/// remote provider is https, so the config layer refuses one in a build that
/// cannot honour it -- rather than failing per-request at 3am.
#[test]
fn https_upstreams_track_the_tls_feature() {
    let result =
        Config::from_toml("[[upstream]]\nname = \"openai\"\nurl = \"https://api.openai.com/v1\"\n");
    if cfg!(feature = "tls") {
        assert!(result.is_ok(), "tls build should accept https: {result:?}");
    } else {
        let err = result.unwrap_err().to_string();
        assert!(err.contains("tls"), "{err}");
    }
}

#[test]
fn upstream_key_prefers_the_environment() {
    let var = "MINI_ROUTER_TEST_KEY_PREFERS_ENV";
    std::env::set_var(var, "sk-from-env");
    let cfg = Config::from_toml(&format!(
        r#"
        [[upstream]]
        name = "a"
        url = "http://x.invalid/v1"
        api_key = "sk-from-file"
        api_key_env = "{var}"
        "#
    ))
    .unwrap();
    assert_eq!(
        cfg.upstreams[0].resolve_key().as_deref(),
        Some("sk-from-env")
    );
    std::env::remove_var(var);
    assert_eq!(
        cfg.upstreams[0].resolve_key().as_deref(),
        Some("sk-from-file"),
        "an unset env var should fall back to the file value"
    );
}

#[test]
fn base_url_join_is_slash_safe() {
    let cfg =
        Config::from_toml("[[upstream]]\nname = \"a\"\nurl = \"http://api.openai.invalid/v1/\"\n")
            .unwrap();
    let up = &cfg.upstreams[0];
    assert_eq!(
        up.join("/chat/completions"),
        "http://api.openai.invalid/v1/chat/completions"
    );
    assert_eq!(up.join("models"), "http://api.openai.invalid/v1/models");
}

/// The shipped example points at real provider APIs, which are https, so it
/// can only be validated by a build that has TLS.
#[cfg(feature = "tls")]
#[test]
fn the_shipped_example_config_is_valid() {
    let text = include_str!("../mini-router.example.toml");
    let cfg = Config::from_toml(text).expect("the shipped example must parse");
    assert!(!cfg.upstreams.is_empty());
    assert!(
        !cfg.pools.is_empty(),
        "the example should demonstrate a pool"
    );
}
