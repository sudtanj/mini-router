//! Configuration parsing and validation.
//!
//! A router that starts with a subtly wrong config is worse than one that
//! refuses to start, so the invalid cases get as much attention as the valid
//! ones.

use mini_router::config::{Config, Strategy};

const MINIMAL: &str = r#"
[[upstream]]
name = "a"
url = "http://127.0.0.1:11434/v1"
"#;

#[test]
fn minimal_config_gets_sbc_friendly_defaults() {
    let cfg = Config::from_toml(MINIMAL).unwrap();
    assert_eq!(cfg.upstreams.len(), 1);
    assert_eq!(cfg.server.listen.to_string(), "0.0.0.0:8080");
    // Two workers, not one-per-core: the board is also running the model.
    assert_eq!(cfg.server.worker_threads, 2);
    // One request at a time per board, because that is what fits in 1 GB.
    assert_eq!(cfg.upstreams[0].max_concurrency, 1);
    assert_eq!(cfg.upstreams[0].weight, 1);
    assert!(!cfg.upstreams[0].fallback_only);
    assert_eq!(cfg.balance.strategy, Strategy::P2cLatency);
    assert_eq!(cfg.balance.retries, 2);
    assert!(cfg.balance.retry_on_status.contains(&503));
    assert!(!cfg.server.auth.require_auth);
}

#[test]
fn full_config_round_trips() {
    let cfg = Config::from_toml(
        r#"
        [server]
        listen = "127.0.0.1:9000"
        worker_threads = 1
        max_body_bytes = 4096
        upstream_header_timeout_secs = 30
        queue_timeout_secs = 10
        pool_idle_timeout_secs = 45
        log_level = "debug"

        [server.auth]
        api_keys = ["sk-a"]
        require_auth = true

        [balance]
        strategy = "weighted"
        retries = 1
        retry_on_status = [503]

        [health]
        interval_secs = 5
        timeout_secs = 2
        path = "/models"
        failure_threshold = 2
        success_threshold = 1
        cooldown_secs = 10

        [alias]
        "gpt-4o-mini" = "qwen2.5:1.5b"

        [[upstream]]
        name = "pi-1"
        url = "http://10.0.0.11:8080/v1"
        weight = 3
        max_concurrency = 2
        models = ["qwen2.5:1.5b"]

        [[upstream]]
        name = "cloud"
        url = "http://api.example.invalid/v1"
        api_key = "sk-upstream"
        fallback_only = true
        "#,
    )
    .unwrap();

    assert_eq!(cfg.balance.strategy, Strategy::Weighted);
    assert_eq!(cfg.server.log_level, "debug");
    assert_eq!(cfg.upstreams[0].weight, 3);
    assert!(cfg.upstreams[1].fallback_only);
    assert_eq!(cfg.resolve_alias("gpt-4o-mini"), "qwen2.5:1.5b");
    assert_eq!(cfg.resolve_alias("unknown"), "unknown");
}

#[test]
fn every_strategy_name_parses() {
    for (name, expected) in [
        ("round-robin", Strategy::RoundRobin),
        ("least-conn", Strategy::LeastConn),
        ("weighted", Strategy::Weighted),
        ("p2c-latency", Strategy::P2cLatency),
        ("first-available", Strategy::FirstAvailable),
    ] {
        let cfg = Config::from_toml(&format!("[balance]\nstrategy = \"{name}\"\n{MINIMAL}"))
            .unwrap_or_else(|e| panic!("{name} should parse: {e}"));
        assert_eq!(cfg.balance.strategy, expected);
        assert_eq!(cfg.balance.strategy.to_string(), name);
    }
}

fn error_for(toml: &str) -> String {
    Config::from_toml(toml)
        .map(|_| String::new())
        .unwrap_err()
        .to_string()
}

/// The `tls` feature is what makes an https upstream reachable, so the config
/// layer refuses one in a build that cannot honour it -- rather than failing
/// per-request at 3am.
#[test]
fn https_upstreams_track_the_tls_feature() {
    let result = Config::from_toml(
        r#"
        [[upstream]]
        name = "cloud"
        url = "https://api.example.invalid/v1"
        "#,
    );
    if cfg!(feature = "tls") {
        assert!(result.is_ok(), "tls build should accept https: {result:?}");
    } else {
        let err = result.unwrap_err().to_string();
        assert!(err.contains("tls"), "{err}");
    }
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
        url = "http://127.0.0.1:1/v1"
        [[upstream]]
        name = "a"
        url = "http://127.0.0.1:2/v1"
        "#,
    );
    assert!(err.contains("duplicate upstream name"), "{err}");
}

#[test]
fn rejects_a_url_without_a_scheme() {
    let err = error_for(
        r#"
        [[upstream]]
        name = "a"
        url = "127.0.0.1:11434/v1"
        "#,
    );
    assert!(err.contains("http://"), "{err}");
}

#[test]
fn rejects_zero_concurrency_and_zero_weight() {
    for field in ["max_concurrency = 0", "weight = 0"] {
        let err = error_for(&format!(
            r#"
            [[upstream]]
            name = "a"
            url = "http://127.0.0.1:1/v1"
            {field}
            "#
        ));
        assert!(err.contains("must be at least 1"), "{field}: {err}");
    }
}

#[test]
fn rejects_a_shelf_that_is_entirely_fallback() {
    let err = error_for(
        r#"
        [[upstream]]
        name = "a"
        url = "http://127.0.0.1:1/v1"
        fallback_only = true
        "#,
    );
    assert!(err.contains("fallback_only"), "{err}");
}

#[test]
fn rejects_auth_without_keys() {
    let err = error_for(&format!("[server.auth]\nrequire_auth = true\n{MINIMAL}"));
    assert!(err.contains("no api_keys"), "{err}");
}

#[test]
fn rejects_unknown_keys_so_typos_are_not_silent() {
    let err = error_for(&format!("[balnce]\nstrategy = \"weighted\"\n{MINIMAL}"));
    assert!(err.contains("unknown field"), "{err}");

    let err = error_for(
        r#"
        [[upstream]]
        name = "a"
        url = "http://127.0.0.1:1/v1"
        max_concurency = 2
        "#,
    );
    assert!(err.contains("unknown field"), "{err}");
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
    let resolved = cfg.resolve_alias("a");
    assert!(["a", "b", "c"].contains(&resolved));
}

#[test]
fn upstream_key_prefers_the_environment() {
    // Unique name so this test does not race other tests touching the env.
    let var = "MINI_ROUTER_TEST_KEY_PREFERS_ENV";
    std::env::set_var(var, "sk-from-env");
    let cfg = Config::from_toml(&format!(
        r#"
        [[upstream]]
        name = "a"
        url = "http://127.0.0.1:1/v1"
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
    let cfg = Config::from_toml(
        r#"
        [[upstream]]
        name = "a"
        url = "http://127.0.0.1:11434/v1/"
        "#,
    )
    .unwrap();
    let up = &cfg.upstreams[0];
    assert_eq!(
        up.join("/chat/completions"),
        "http://127.0.0.1:11434/v1/chat/completions"
    );
    assert_eq!(up.join("models"), "http://127.0.0.1:11434/v1/models");
}

#[test]
fn example_config_in_the_repo_is_valid() {
    let text = include_str!("../mini-router.example.toml");
    let cfg = Config::from_toml(text).expect("the shipped example must parse");
    assert!(!cfg.upstreams.is_empty());
}
