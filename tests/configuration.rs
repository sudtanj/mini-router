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
    compose_environment_inner(yaml, true)
}

/// Only the lines that are actually in effect, ignoring commented examples.
fn compose_environment_active(yaml: &str) -> Vec<(String, String)> {
    compose_environment_inner(yaml, false)
}

/// Pull `KEY: value` pairs out of the `environment:` block of a compose file.
///
/// Handles YAML block scalars (`KEY: |` followed by indented lines), because
/// that is how a pool is written one member per line, and optionally reads the
/// commented-out examples too -- those are documentation, and documentation
/// that names a setting mini-router dropped is exactly the drift worth
/// catching.
fn compose_environment_inner(yaml: &str, include_commented: bool) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut block: Option<usize> = None;

    // Strip one layer of comment marker. Returns the content with its own
    // indentation removed, plus how deep it sits -- for a commented line that
    // is the `#` column plus whatever spaces follow inside the comment, so a
    // commented block scalar nests exactly like an active one.
    let read = |line: &str| -> Option<(usize, String)> {
        let trimmed = line.trim_start();
        let base = line.len() - trimmed.len();
        if trimmed.starts_with('#') {
            if !include_commented {
                return None;
            }
            let rest = trimmed
                .strip_prefix("# ")
                .or_else(|| trimmed.strip_prefix('#'))
                .unwrap_or(trimmed);
            let inner = rest.len() - rest.trim_start().len();
            return Some((base + inner, rest.trim().to_string()));
        }
        Some((base, trimmed.to_string()))
    };

    let lines: Vec<&str> = yaml.lines().collect();
    let mut i = 0;
    // Find the environment block.
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if trimmed == "environment:" {
            block = Some(lines[i].len() - trimmed.len());
            i += 1;
            break;
        }
        i += 1;
    }
    let Some(block_indent) = block else {
        return out;
    };

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let depth = line.len() - trimmed.len();
        if !trimmed.is_empty() && depth <= block_indent {
            break; // out of the environment block
        }
        i += 1;

        let Some((key_depth, content)) = read(line) else {
            continue;
        };
        let Some((key, value)) = content.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            continue;
        }
        let value = value.trim();

        // A block scalar: the value is the indented lines that follow.
        if value.starts_with('|') || value.starts_with('>') {
            let mut parts: Vec<String> = Vec::new();
            while i < lines.len() {
                let Some((depth, next)) = read(lines[i]) else {
                    break;
                };
                // Continuation lines sit deeper than the key they belong to.
                if next.is_empty() || depth <= key_depth {
                    break;
                }
                parts.push(next);
                i += 1;
            }
            out.push((key.to_string(), parts.join("\n")));
            continue;
        }

        let value = value.trim_matches('"');
        // `${VAR}` and `${VAR:-default}` are compose's own syntax; a real
        // deployment substitutes them, so stand in a value.
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
    out
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
        // Present-but-empty is not defined: an empty value is how compose
        // renders an unset `${VAR:-}`, and mini-router skips those. Checking
        // only for the key would leave the provider without a URL.
        if !pairs.iter().any(|(k, v)| *k == key && !v.is_empty()) {
            pairs.push((key, format!("http://{name}.invalid/v1")));
        }
    }
    pairs.push(("MINI_ROUTER_AUTODETECT".into(), "off".into()));
}

#[test]
fn the_shipped_compose_files_only_use_settings_that_exist() {
    // Both of them: the one that builds from this checkout and the one that
    // pulls the published image. They carry the same environment block, and
    // either drifting is the same bug.
    for (name, yaml) in [
        ("docker-compose.yml", include_str!("../docker-compose.yml")),
        (
            "docker-compose.hub.yml",
            include_str!("../docker-compose.hub.yml"),
        ),
    ] {
        let mut pairs = compose_environment(yaml);
        assert!(
            pairs
                .iter()
                .any(|(k, _)| k.starts_with("MINI_ROUTER_POOL_")),
            "{name} should demonstrate a pool; parsed {pairs:#?}"
        );
        make_self_contained(&mut pairs);

        if let Err(e) = env::load(&pairs) {
            panic!("{name} has drifted from what mini-router accepts: {e}");
        }
    }
}

/// The published image has no shell tooling -- `--healthcheck` exists so it
/// does not need any -- so a compose file that overrides the healthcheck with
/// `wget` or `curl` would break against it. The build-from-source image is
/// alpine and does have busybox wget, which is why only the hub file is
/// checked here.
#[test]
fn the_hub_compose_file_does_not_reach_for_shell_tooling() {
    let yaml = include_str!("../docker-compose.hub.yml");
    for line in yaml.lines() {
        let code = line.split('#').next().unwrap_or("");
        assert!(
            !code.contains("wget") && !code.contains("curl"),
            "docker-compose.hub.yml must not depend on shell tooling the \
             published image does not carry: {line}"
        );
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

/// Read `KEY=value` pairs out of a dotenv file, ignoring commented examples.
#[cfg(feature = "tls")]
fn dotenv(text: &str) -> std::collections::BTreeMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// Resolve compose's `${VAR}`, `${VAR:-default}` and `${VAR:?message}` against
/// a dotenv map, the way `docker compose` would.
#[cfg(feature = "tls")]
fn substitute(value: &str, env: &std::collections::BTreeMap<String, String>) -> Option<String> {
    let Some(inner) = value.strip_prefix("${").and_then(|v| v.strip_suffix('}')) else {
        return Some(value.to_string());
    };
    let (name, fallback) = match inner.split_once(":-") {
        Some((n, d)) => (n, Some(d.to_string())),
        None => match inner.split_once(":?") {
            // `:?` means compose refuses to start when the value is empty.
            Some((n, _)) => (n, None),
            None => (inner, Some(String::new())),
        },
    };
    match env.get(name) {
        Some(v) if !v.is_empty() => Some(v.clone()),
        _ => fallback,
    }
}

/// The first run a real person has: copy both shipped files, fill in one
/// provider key and the client key the compose file insists on, start it.
///
/// Needs a TLS build: the key it fills in is a real provider's, and every
/// remote provider is https.
///
/// This is the whole point of the pair being shipped together, and it is easy
/// to break from either side -- a pool default naming a provider the user has
/// no key for stops mini-router dead, and so does a half-filled custom
/// provider. Both have happened.
#[cfg(feature = "tls")]
#[test]
fn the_shipped_files_start_with_a_single_provider_key() {
    let dotenv_map = dotenv(include_str!("../.env.example"));

    for (name, yaml) in [
        ("docker-compose.yml", include_str!("../docker-compose.yml")),
        (
            "docker-compose.hub.yml",
            include_str!("../docker-compose.hub.yml"),
        ),
    ] {
        let mut env = dotenv_map.clone();
        // The two things the instructions tell you to fill in.
        env.insert("OPENAI_API_KEY".into(), "sk-test".into());
        env.insert("MINI_ROUTER_API_KEYS".into(), "sk-client".into());

        let mut pairs: Vec<(String, String)> = Vec::new();
        for (key, raw) in compose_environment_active(yaml) {
            match substitute(&raw, &env) {
                Some(v) => pairs.push((key, v)),
                None => panic!(
                    "{name}: {key} uses compose's `:?` form and .env.example leaves it \
                     empty, so `docker compose up` would refuse to start"
                ),
            }
        }
        // Provider keys reach mini-router from the process environment, not
        // through the compose `environment:` block, so pass them alongside.
        pairs.push(("OPENAI_API_KEY".into(), "sk-test".into()));

        let cfg = match env::load(&pairs) {
            Ok(r) => r.config,
            Err(e) => panic!(
                "{name} plus the shipped .env.example does not start with one \
                 provider key: {e}"
            ),
        };
        assert_eq!(
            cfg.upstreams.len(),
            1,
            "{name}: expected just the one provider whose key was filled in, got {:?}",
            cfg.upstreams.iter().map(|u| &u.name).collect::<Vec<_>>()
        );
        assert!(
            cfg.server.auth.require_auth,
            "{name}: the shipped files should not leave the port open"
        );
    }
}

/// The compose parser above is test scaffolding, but two bugs have already
/// hidden in it, so it gets its own test rather than being trusted.
#[test]
fn the_compose_parser_reads_block_scalars_and_comments() {
    let yaml = "\
services:
  mini-router:
    image: x
    environment:
      PLAIN: value
      QUOTED: \"1\"
      FROM_ENV: ${SOMETHING:-}
      REQUIRED: ${MUST_SET:?fill this in}
      MINI_ROUTER_POOL_FAST: |
        openai:gpt-4o-mini
        anthropic:claude-haiku-4-5
      # COMMENTED_PLAIN: example
      # MINI_ROUTER_POOL_SMART: |
      #   anthropic:claude-sonnet-4-5
      #   openai:gpt-4o
    ports:
      - \"8080:8080\"
";

    let active: std::collections::BTreeMap<String, String> =
        compose_environment_active(yaml).into_iter().collect();
    assert_eq!(active.get("PLAIN").unwrap(), "value");
    assert_eq!(active.get("QUOTED").unwrap(), "1", "quotes are stripped");
    assert_eq!(active.get("FROM_ENV").unwrap(), "", "`:-` renders empty");
    assert_eq!(active.get("REQUIRED").unwrap(), "sk-test", "`:?` stands in");
    assert_eq!(
        active.get("MINI_ROUTER_POOL_FAST").unwrap(),
        "openai:gpt-4o-mini\nanthropic:claude-haiku-4-5",
        "a block scalar keeps one member per line, in order"
    );
    assert!(
        !active.contains_key("COMMENTED_PLAIN"),
        "commented lines are not active"
    );
    assert!(
        !active.contains_key("PORTS") && active.len() == 5,
        "nothing outside the environment block leaks in: {active:#?}"
    );

    let all: std::collections::BTreeMap<String, String> =
        compose_environment(yaml).into_iter().collect();
    assert_eq!(all.get("COMMENTED_PLAIN").unwrap(), "example");
    assert_eq!(
        all.get("MINI_ROUTER_POOL_SMART").unwrap(),
        "anthropic:claude-sonnet-4-5\nopenai:gpt-4o",
        "a commented block scalar reads the same as an active one"
    );
}
