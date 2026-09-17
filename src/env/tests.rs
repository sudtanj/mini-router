use super::*;

/// Build a variable list the way `docker compose` would hand one over.
fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn resolve(pairs: &[(&str, &str)]) -> Resolved {
    load(&vars(pairs)).unwrap_or_else(|e| panic!("should resolve: {e}"))
}

/// Providers spelled out in full. Tests that merely need a provider to exist
/// use these rather than a well-known key, so they do not quietly depend on
/// autodetection -- whose URLs are all https, and so need the `tls` feature.
const OPENAI: (&str, &str) = (
    "MINI_ROUTER_PROVIDER_OPENAI_URL",
    "http://openai.invalid/v1",
);
const ANTHROPIC_URL: (&str, &str) = (
    "MINI_ROUTER_PROVIDER_ANTHROPIC_URL",
    "http://anthropic.invalid/v1",
);
const ANTHROPIC_PROTO: (&str, &str) = ("MINI_ROUTER_PROVIDER_ANTHROPIC_PROTOCOL", "anthropic");

fn resolve_err(pairs: &[(&str, &str)]) -> String {
    load(&vars(pairs))
        .map(|_| String::new())
        .unwrap_err()
        .to_string()
}

// ---------------------------------------------------------------------------
// The zero-config path
// ---------------------------------------------------------------------------

// Well-known providers are all https, so this needs a TLS build.
#[cfg(feature = "tls")]
#[test]
fn a_provider_key_alone_is_enough() {
    // This is the whole docker-compose story: drop in the keys, get a router.
    let r = resolve(&[
        ("OPENAI_API_KEY", "sk-oai"),
        ("ANTHROPIC_API_KEY", "sk-ant"),
    ]);
    let names: Vec<&str> = r.config.upstreams.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(names, ["anthropic", "openai"]);

    let ant = &r.config.upstreams[0];
    assert_eq!(ant.url, "https://api.anthropic.com/v1");
    assert_eq!(ant.protocol, Protocol::Anthropic);
    // The key is referenced by name, so it is never copied into the config.
    assert_eq!(ant.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    assert_eq!(ant.api_key, None);

    let oai = &r.config.upstreams[1];
    assert_eq!(oai.url, "https://api.openai.com/v1");
    assert_eq!(oai.protocol, Protocol::Openai);

    assert_eq!(r.sources["openai"], Source::Autodetected);
}

// Well-known providers are all https, so this needs a TLS build.
#[cfg(feature = "tls")]
#[test]
fn every_well_known_provider_is_detected() {
    for (name, key_var, url, protocol) in WELL_KNOWN {
        let r = resolve(&[(key_var, "sk-test")]);
        assert_eq!(r.config.upstreams.len(), 1, "{name}");
        assert_eq!(r.config.upstreams[0].name, *name);
        assert_eq!(r.config.upstreams[0].url, *url, "{name}");
        assert_eq!(r.config.upstreams[0].protocol, *protocol, "{name}");
    }
}

#[test]
fn an_empty_key_is_treated_as_unset() {
    // Compose writes an empty string for a variable that is not in the .env.
    let err = resolve_err(&[("OPENAI_API_KEY", "")]);
    assert!(err.contains("no providers configured"), "{err}");
}

// Well-known providers are all https, so this needs a TLS build.
#[cfg(feature = "tls")]
#[test]
fn autodetection_stands_down_once_a_provider_is_configured() {
    // A stray OPENAI_API_KEY belonging to some other tool in the same
    // container must not quietly add a provider.
    let r = resolve(&[
        ("OPENAI_API_KEY", "sk-not-for-us"),
        ("MINI_ROUTER_PROVIDER_LOCAL_URL", "http://ollama:11434/v1"),
    ]);
    let names: Vec<&str> = r.config.upstreams.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(names, ["local"]);

    // Unless you ask for it.
    let both = resolve(&[
        ("OPENAI_API_KEY", "sk-oai"),
        ("MINI_ROUTER_PROVIDER_LOCAL_URL", "http://ollama:11434/v1"),
        ("MINI_ROUTER_AUTODETECT", "on"),
    ]);
    let mut names: Vec<&str> = both
        .config
        .upstreams
        .iter()
        .map(|u| u.name.as_str())
        .collect();
    names.sort();
    assert_eq!(names, ["local", "openai"]);

    // Or explicitly refuse it.
    let off = load(&vars(&[
        ("OPENAI_API_KEY", "sk-oai"),
        ("MINI_ROUTER_AUTODETECT", "off"),
    ]));
    assert!(
        off.is_err(),
        "autodetect off with no explicit provider has nothing to route to"
    );
}

#[test]
fn a_well_known_provider_can_be_pointed_somewhere_else() {
    // A proxy, a gateway, an Azure deployment: the built-in URL is a default,
    // never a constraint.
    let r = resolve(&[
        ("OPENAI_API_KEY", "sk-oai"),
        (
            "MINI_ROUTER_PROVIDER_OPENAI_URL",
            "http://my-gateway.internal/v1",
        ),
    ]);
    assert_eq!(r.config.upstreams.len(), 1);
    assert_eq!(r.config.upstreams[0].url, "http://my-gateway.internal/v1");
    assert_eq!(r.config.upstreams[0].protocol, Protocol::Openai);
    assert_eq!(
        r.config.upstreams[0].api_key_env.as_deref(),
        Some("OPENAI_API_KEY"),
        "the well-known key should still be picked up"
    );
}

// ---------------------------------------------------------------------------
// Explicit providers
// ---------------------------------------------------------------------------

#[test]
fn a_provider_can_be_defined_entirely_by_environment() {
    let r = resolve(&[
        ("MINI_ROUTER_PROVIDER_MYBOX_URL", "http://10.0.0.5:8000/v1"),
        ("MINI_ROUTER_PROVIDER_MYBOX_PROTOCOL", "anthropic"),
        ("MINI_ROUTER_PROVIDER_MYBOX_API_KEY", "sk-inline"),
        ("MINI_ROUTER_PROVIDER_MYBOX_MAX_CONCURRENCY", "3"),
        ("MINI_ROUTER_PROVIDER_MYBOX_WEIGHT", "5"),
        ("MINI_ROUTER_PROVIDER_MYBOX_MODELS", "a, b ,c"),
        ("MINI_ROUTER_PROVIDER_MYBOX_FALLBACK_ONLY", "false"),
        (
            "MINI_ROUTER_PROVIDER_MYBOX_HEADERS",
            "x-title=mini-router,http-referer=https://my.lan",
        ),
    ]);
    let up = &r.config.upstreams[0];
    assert_eq!(up.name, "mybox");
    assert_eq!(up.url, "http://10.0.0.5:8000/v1");
    assert_eq!(up.protocol, Protocol::Anthropic);
    assert_eq!(up.api_key.as_deref(), Some("sk-inline"));
    assert_eq!(up.max_concurrency, 3);
    assert_eq!(up.weight, 5);
    assert_eq!(up.models, ["a", "b", "c"]);
    assert!(!up.fallback_only);
    assert_eq!(up.headers["x-title"], "mini-router");
    assert_eq!(up.headers["http-referer"], "https://my.lan");
    assert_eq!(r.sources["mybox"], Source::Declared);
}

#[test]
fn underscores_in_a_provider_name_become_dashes() {
    let r = resolve(&[("MINI_ROUTER_PROVIDER_MY_BOX_URL", "http://x.invalid/v1")]);
    assert_eq!(r.config.upstreams[0].name, "my-box");
}

#[test]
fn a_provider_name_ending_in_a_field_word_still_parses() {
    // `MY_API` + `_KEY` must not be read as `MY` + `_API_KEY`: the longest
    // known suffix wins.
    let r = resolve(&[
        ("MINI_ROUTER_PROVIDER_MY_API_URL", "http://x.invalid/v1"),
        ("MINI_ROUTER_PROVIDER_MY_API_API_KEY", "sk-1"),
    ]);
    assert_eq!(r.config.upstreams.len(), 1);
    assert_eq!(r.config.upstreams[0].name, "my-api");
    assert_eq!(r.config.upstreams[0].api_key.as_deref(), Some("sk-1"));
}

#[test]
fn the_upstream_prefix_is_accepted_too() {
    // The config file calls them upstreams; the docs call them providers.
    let r = resolve(&[("MINI_ROUTER_UPSTREAM_BOX_URL", "http://x.invalid/v1")]);
    assert_eq!(r.config.upstreams[0].name, "box");
}

#[test]
fn provider_order_can_be_pinned() {
    let pairs = [
        ("MINI_ROUTER_PROVIDER_ALPHA_URL", "http://a.invalid/v1"),
        ("MINI_ROUTER_PROVIDER_BETA_URL", "http://b.invalid/v1"),
        ("MINI_ROUTER_PROVIDER_GAMMA_URL", "http://c.invalid/v1"),
    ];
    // Alphabetical by default, so it is at least stable.
    let names: Vec<String> = resolve(&pairs)
        .config
        .upstreams
        .iter()
        .map(|u| u.name.clone())
        .collect();
    assert_eq!(names, ["alpha", "beta", "gamma"]);

    let mut with_order = pairs.to_vec();
    with_order.push(("MINI_ROUTER_PROVIDER_ORDER", "gamma,alpha,beta"));
    let names: Vec<String> = resolve(&with_order)
        .config
        .upstreams
        .iter()
        .map(|u| u.name.clone())
        .collect();
    assert_eq!(names, ["gamma", "alpha", "beta"]);
}

#[test]
fn provider_order_is_a_setting_not_a_provider_called_order() {
    // It sits under the same PROVIDER_ prefix, so it has to be claimed first.
    let r = resolve(&[
        ("MINI_ROUTER_PROVIDER_A_URL", "http://a.invalid/v1"),
        ("MINI_ROUTER_PROVIDER_ORDER", "a"),
    ]);
    let names: Vec<&str> = r.config.upstreams.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(names, ["a"], "no provider named `order` should appear");
}

#[test]
fn provider_order_naming_an_unknown_provider_is_rejected() {
    let err = resolve_err(&[
        ("MINI_ROUTER_PROVIDER_A_URL", "http://a.invalid/v1"),
        ("MINI_ROUTER_PROVIDER_ORDER", "a,ghost"),
    ]);
    assert!(err.contains("unknown provider"), "{err}");
}

#[test]
fn a_provider_without_a_url_is_rejected() {
    let err = resolve_err(&[("MINI_ROUTER_PROVIDER_MYSTERY_API_KEY", "sk-1")]);
    assert!(err.contains("needs a URL"), "{err}");
    assert!(err.contains("MINI_ROUTER_PROVIDER_MYSTERY_URL"), "{err}");
}

/// ...unless the name is one mini-router already knows, which brings its own.
#[cfg(feature = "tls")]
#[test]
fn a_well_known_name_supplies_its_own_url() {
    let ok = resolve(&[("MINI_ROUTER_PROVIDER_GROQ_API_KEY", "sk-1")]);
    assert_eq!(ok.config.upstreams[0].url, "https://api.groq.com/openai/v1");
    assert_eq!(ok.config.upstreams[0].protocol, Protocol::Openai);
}

// ---------------------------------------------------------------------------
// Pools
// ---------------------------------------------------------------------------

#[test]
fn a_pool_is_one_variable() {
    let r = resolve(&[
        OPENAI,
        ANTHROPIC_URL,
        ANTHROPIC_PROTO,
        (
            "MINI_ROUTER_POOL_FAST",
            "openai:gpt-4o-mini,anthropic:claude-haiku-4-5",
        ),
    ]);
    let pool = &r.config.pools["fast"];
    assert_eq!(pool.members.len(), 2);
    assert_eq!(pool.members[0].upstream, "openai");
    assert_eq!(pool.members[0].model, "gpt-4o-mini");
    assert_eq!(pool.members[1].upstream, "anthropic");
    assert_eq!(pool.members[1].model, "claude-haiku-4-5");
    // Order in the variable is priority order.
    assert_eq!(pool.strategy, None);
}

#[test]
fn a_model_id_may_contain_colons() {
    // Only the first colon separates; `local:qwen2.5:0.5b` is one member.
    let r = resolve(&[
        ("MINI_ROUTER_PROVIDER_LOCAL_URL", "http://ollama:11434/v1"),
        ("MINI_ROUTER_POOL_TINY", "local:qwen2.5:0.5b"),
    ]);
    let m = &r.config.pools["tiny"].members[0];
    assert_eq!(m.upstream, "local");
    assert_eq!(m.model, "qwen2.5:0.5b");
}

#[test]
fn pool_strategy_weights_and_description_are_separate_variables() {
    let r = resolve(&[
        OPENAI,
        ANTHROPIC_URL,
        ANTHROPIC_PROTO,
        (
            "MINI_ROUTER_POOL_SPREAD",
            "openai:gpt-4o-mini,anthropic:claude-haiku-4-5",
        ),
        ("MINI_ROUTER_POOL_SPREAD_STRATEGY", "round-robin"),
        ("MINI_ROUTER_POOL_SPREAD_WEIGHTS", "3,1"),
        ("MINI_ROUTER_POOL_SPREAD_DESCRIPTION", "burn both quotas"),
    ]);
    let pool = &r.config.pools["spread"];
    assert_eq!(pool.strategy, Some(Strategy::RoundRobin));
    assert_eq!(pool.members[0].weight, 3);
    assert_eq!(pool.members[1].weight, 1);
    assert_eq!(pool.description.as_deref(), Some("burn both quotas"));
}

#[test]
fn mismatched_weights_are_rejected() {
    let err = resolve_err(&[
        OPENAI,
        ("MINI_ROUTER_POOL_P", "openai:a,openai:b"),
        ("MINI_ROUTER_POOL_P_WEIGHTS", "3"),
    ]);
    assert!(err.contains("one per member"), "{err}");
}

#[test]
fn a_malformed_pool_member_says_what_it_should_look_like() {
    let err = resolve_err(&[OPENAI, ("MINI_ROUTER_POOL_P", "gpt-4o-mini")]);
    assert!(err.contains("provider:model"), "{err}");

    let empty = resolve_err(&[OPENAI, ("MINI_ROUTER_POOL_P", "openai:")]);
    assert!(empty.contains("no model after the colon"), "{empty}");
}

#[test]
fn a_pool_pointing_at_an_unknown_provider_is_rejected() {
    let err = resolve_err(&[OPENAI, ("MINI_ROUTER_POOL_P", "nosuch:model")]);
    assert!(err.contains("unknown upstream"), "{err}");
}

// ---------------------------------------------------------------------------
// Scalar settings
// ---------------------------------------------------------------------------

#[test]
fn server_and_balance_settings_come_from_the_environment() {
    let r = resolve(&[
        OPENAI,
        ("MINI_ROUTER_LISTEN", "127.0.0.1:9999"),
        ("MINI_ROUTER_WORKER_THREADS", "1"),
        ("MINI_ROUTER_LOG_LEVEL", "debug"),
        ("MINI_ROUTER_MAX_BODY_BYTES", "4096"),
        ("MINI_ROUTER_HEADER_TIMEOUT_SECS", "45"),
        ("MINI_ROUTER_QUEUE_TIMEOUT_SECS", "15"),
        ("MINI_ROUTER_STRATEGY", "p2c-latency"),
        ("MINI_ROUTER_SPILLOVER", "status-list"),
        ("MINI_ROUTER_RETRY_ON_STATUS", "429,503"),
        ("MINI_ROUTER_MAX_ATTEMPTS", "2"),
        ("MINI_ROUTER_HEALTH_INTERVAL_SECS", "120"),
        ("MINI_ROUTER_COOLDOWN_SECS", "45"),
        ("MINI_ROUTER_DEFAULT_MAX_TOKENS", "2048"),
        ("MINI_ROUTER_ANTHROPIC_VERSION", "2024-10-22"),
    ]);
    let c = &r.config;
    assert_eq!(c.server.listen.to_string(), "127.0.0.1:9999");
    assert_eq!(c.server.worker_threads, 1);
    assert_eq!(c.server.log_level, "debug");
    assert_eq!(c.server.max_body_bytes, 4096);
    assert_eq!(c.server.upstream_header_timeout_secs, 45);
    assert_eq!(c.server.queue_timeout_secs, 15);
    assert_eq!(c.balance.strategy, Strategy::P2cLatency);
    assert_eq!(c.balance.spillover, Spillover::StatusList);
    assert_eq!(c.balance.retry_on_status, [429, 503]);
    assert_eq!(c.balance.max_attempts, 2);
    assert_eq!(c.health.interval_secs, 120);
    assert_eq!(c.health.cooldown_secs, 45);
    assert_eq!(c.translate.default_max_tokens, 2048);
    assert_eq!(c.translate.anthropic_version, "2024-10-22");
}

#[test]
fn auth_is_configurable_without_a_file() {
    let r = resolve(&[
        OPENAI,
        ("MINI_ROUTER_REQUIRE_AUTH", "true"),
        ("MINI_ROUTER_API_KEYS", "sk-one, sk-two"),
    ]);
    assert!(r.config.server.auth.require_auth);
    assert_eq!(r.config.server.auth.api_keys, ["sk-one", "sk-two"]);
}

#[test]
fn admin_and_metrics_can_be_switched_off() {
    let on = resolve(&[OPENAI]);
    assert!(on.config.server.admin, "on by default");
    assert!(on.config.server.metrics);

    let off = resolve(&[
        OPENAI,
        ("MINI_ROUTER_ADMIN", "off"),
        ("MINI_ROUTER_METRICS", "off"),
    ]);
    assert!(!off.config.server.admin);
    assert!(!off.config.server.metrics);
}

#[test]
fn aliases_are_one_variable() {
    let r = resolve(&[
        OPENAI,
        ("MINI_ROUTER_POOL_FAST", "openai:gpt-4o-mini"),
        (
            "MINI_ROUTER_ALIASES",
            "gpt-3.5-turbo=fast, claude-3-5-haiku-20241022=fast",
        ),
    ]);
    assert_eq!(r.config.alias["gpt-3.5-turbo"], "fast");
    assert_eq!(r.config.alias["claude-3-5-haiku-20241022"], "fast");
}

#[test]
fn booleans_accept_the_spellings_people_actually_use() {
    for truthy in ["1", "true", "TRUE", "yes", "on", "enabled"] {
        assert!(parse_bool("X", truthy).unwrap(), "{truthy}");
    }
    for falsy in ["0", "false", "FALSE", "no", "off", "disabled"] {
        assert!(!parse_bool("X", falsy).unwrap(), "{falsy}");
    }
    let err = parse_bool("MINI_ROUTER_ADMIN", "maybe")
        .unwrap_err()
        .to_string();
    assert!(err.contains("yes/no"), "{err}");
}

// ---------------------------------------------------------------------------
// Errors that save an afternoon
// ---------------------------------------------------------------------------

#[test]
fn a_typo_is_an_error_not_a_silent_default() {
    let err = resolve_err(&[OPENAI, ("MINI_ROUTER_STRATEGIE", "priority")]);
    assert!(err.contains("unrecognised setting"), "{err}");
    assert!(err.contains("MINI_ROUTER_STRATEGIE"), "{err}");
}

#[test]
fn an_unrecognised_provider_field_is_an_error() {
    let err = resolve_err(&[("MINI_ROUTER_PROVIDER_BOX_ENDPOINT", "http://x.invalid")]);
    assert!(err.contains("not a recognised provider setting"), "{err}");
    assert!(
        err.contains("_URL"),
        "the message should list what is valid: {err}"
    );
}

#[test]
fn bad_values_name_the_variable_and_the_value() {
    let err = resolve_err(&[OPENAI, ("MINI_ROUTER_WORKER_THREADS", "lots")]);
    assert!(err.contains("MINI_ROUTER_WORKER_THREADS"), "{err}");
    assert!(err.contains("lots"), "{err}");

    let strategy = resolve_err(&[OPENAI, ("MINI_ROUTER_STRATEGY", "fastest")]);
    assert!(
        strategy.contains("priority"),
        "should list the options: {strategy}"
    );

    let listen = resolve_err(&[("OPENAI_API_KEY", "sk-oai"), ("MINI_ROUTER_LISTEN", "8080")]);
    assert!(listen.contains("not an address"), "{listen}");
}

#[test]
fn other_variables_in_the_environment_are_ignored() {
    // A container has PATH, HOME, HOSTNAME and whatever else in it.
    let r = resolve(&[
        ("PATH", "/usr/bin"),
        ("HOME", "/root"),
        ("SOME_OTHER_APP_SETTING", "1"),
        OPENAI,
    ]);
    assert_eq!(r.config.upstreams.len(), 1);
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

#[test]
fn lists_tolerate_spacing() {
    assert_eq!(split_list("a, b ,c"), ["a", "b", "c"]);
    assert_eq!(split_list(" a ,, b "), ["a", "b"]);
    assert!(split_list("").is_empty());
    assert!(split_list(" , ").is_empty());
}

/// A compose block scalar arrives as one string with newlines in it. Order is
/// the whole point for a pool, so it has to survive.
#[test]
fn lists_may_be_written_one_per_line() {
    assert_eq!(
        split_list("openai:gpt-4o-mini\nanthropic:claude-haiku-4-5\n"),
        ["openai:gpt-4o-mini", "anthropic:claude-haiku-4-5"]
    );
    // Indented, the way a block scalar under `environment:` is written.
    assert_eq!(
        split_list("\n  first\n  second\n  third\n"),
        ["first", "second", "third"]
    );
    // Mixed, and CRLF, because someone will.
    assert_eq!(split_list("a,b\nc\r\nd"), ["a", "b", "c", "d"]);
    assert!(split_list("\n\n  \n").is_empty());
}

#[test]
fn a_pool_can_be_written_one_member_per_line() {
    let r = resolve(&[
        OPENAI,
        ANTHROPIC_URL,
        ANTHROPIC_PROTO,
        (
            "MINI_ROUTER_POOL_FAST",
            "openai:gpt-4o-mini\nanthropic:claude-haiku-4-5\n",
        ),
    ]);
    let pool = &r.config.pools["fast"];
    assert_eq!(pool.members.len(), 2);
    // Written order is priority order, exactly as with commas.
    assert_eq!(pool.members[0].upstream, "openai");
    assert_eq!(pool.members[0].model, "gpt-4o-mini");
    assert_eq!(pool.members[1].upstream, "anthropic");
    assert_eq!(pool.members[1].model, "claude-haiku-4-5");
}

#[test]
fn pairs_need_an_equals() {
    let err = parse_pairs("X", "a").unwrap_err().to_string();
    assert!(err.contains("key=value"), "{err}");
    // A value containing an equals sign survives.
    let ok = parse_pairs("X", "url=https://a.invalid/?x=1").unwrap();
    assert_eq!(ok["url"], "https://a.invalid/?x=1");
}

#[test]
fn provider_name_mapping_round_trips_through_the_documented_rule() {
    assert_eq!(env_name_to_id("OPENAI"), "openai");
    assert_eq!(env_name_to_id("MY_BOX"), "my-box");
    assert_eq!(env_name_to_id(" Spaced "), "spaced");
}
