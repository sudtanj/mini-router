//! Configuration from the environment.
//!
//! This is the only way mini-router is configured. The deployment it is written
//! for is a `docker compose up` with a block of `environment:` entries and
//! nothing else -- no file to write, mount, template or keep in sync with the
//! image.
//!
//! Two rules make that safe to rely on:
//!
//! - A variable mini-router does not recognise is a startup error naming the
//!   variable, not a silent default. A typo costs you a failed start, not a
//!   week of wondering why a setting had no effect.
//! - A bad value names the variable, the value and what was expected.
//!
//! Everything here works over an explicit list of variables rather than
//! reading the process environment, which keeps the parsing testable and keeps
//! tests from racing each other over a shared global.

use std::collections::BTreeMap;

use crate::config::{Config, ConfigError, PoolMember, Spillover, Strategy, UpstreamConfig};
use crate::protocol::Protocol;

/// Prefix on every setting mini-router reads for itself.
pub const PREFIX: &str = "MINI_ROUTER_";

/// Providers that can be configured by dropping in the key alone.
///
/// These are the defaults, not a fixed list: every one of them can be pointed
/// somewhere else with `MINI_ROUTER_PROVIDER_<NAME>_URL`, and any provider not
/// listed here is configured with the same variables under a name you choose.
pub const WELL_KNOWN: &[(&str, &str, &str, Protocol)] = &[
    (
        "anthropic",
        "ANTHROPIC_API_KEY",
        "https://api.anthropic.com/v1",
        Protocol::Anthropic,
    ),
    (
        "cerebras",
        "CEREBRAS_API_KEY",
        "https://api.cerebras.ai/v1",
        Protocol::Openai,
    ),
    (
        "deepseek",
        "DEEPSEEK_API_KEY",
        "https://api.deepseek.com/v1",
        Protocol::Openai,
    ),
    (
        "gemini",
        "GEMINI_API_KEY",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        Protocol::Openai,
    ),
    (
        "groq",
        "GROQ_API_KEY",
        "https://api.groq.com/openai/v1",
        Protocol::Openai,
    ),
    (
        "mistral",
        "MISTRAL_API_KEY",
        "https://api.mistral.ai/v1",
        Protocol::Openai,
    ),
    (
        "openai",
        "OPENAI_API_KEY",
        "https://api.openai.com/v1",
        Protocol::Openai,
    ),
    (
        "openrouter",
        "OPENROUTER_API_KEY",
        "https://openrouter.ai/api/v1",
        Protocol::Openai,
    ),
    (
        "together",
        "TOGETHER_API_KEY",
        "https://api.together.xyz/v1",
        Protocol::Openai,
    ),
    (
        "xai",
        "XAI_API_KEY",
        "https://api.x.ai/v1",
        Protocol::Openai,
    ),
];

/// Per-provider setting suffixes, longest first so that a provider called
/// `MY_API` does not swallow the `_API_KEY` of `MY`.
const PROVIDER_FIELDS: &[&str] = &[
    "_MAX_CONCURRENCY",
    "_FALLBACK_ONLY",
    "_API_KEY_ENV",
    "_API_KEY",
    "_PROTOCOL",
    "_HEADERS",
    "_MODELS",
    "_WEIGHT",
    "_URL",
];

/// Per-pool setting suffixes. A bare `MINI_ROUTER_POOL_<NAME>` is the members.
const POOL_FIELDS: &[&str] = &["_DESCRIPTION", "_STRATEGY", "_WEIGHTS"];

/// How a provider came to be configured, for `--check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Spelled out with `MINI_ROUTER_PROVIDER_<NAME>_*`.
    Declared,
    /// Recognised from a well-known key such as `OPENAI_API_KEY`.
    Autodetected,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Source::Declared => "set",
            Source::Autodetected => "auto",
        })
    }
}

/// A resolved configuration, with a note of how each provider came to be
/// configured so `--check` can show it.
#[derive(Debug)]
pub struct Resolved {
    pub config: Config,
    pub sources: BTreeMap<String, Source>,
}

fn err(msg: impl Into<String>) -> ConfigError {
    ConfigError::new(msg)
}

/// Read the process environment.
pub fn vars() -> Vec<(String, String)> {
    std::env::vars().collect()
}

/// Build the whole configuration from a set of environment variables.
pub fn load(vars: &[(String, String)]) -> Result<Resolved, ConfigError> {
    let mut config = Config::default();
    let mut sources: BTreeMap<String, Source> = BTreeMap::new();

    let lookup: BTreeMap<&str, &str> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    let mut providers: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut pools: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut unknown: Vec<String> = Vec::new();

    for (key, value) in vars {
        let Some(rest) = key.strip_prefix(PREFIX) else {
            continue;
        };
        if value.is_empty() {
            // An empty value in compose usually means "not set", not "set to
            // the empty string". Treat it as absent.
            continue;
        }
        // `PROVIDER_ORDER` is a setting of its own, not a provider called
        // `ORDER`, so it has to be claimed before the prefix match below.
        let provider_rest = if rest == "PROVIDER_ORDER" {
            None
        } else {
            rest.strip_prefix("PROVIDER_")
                .or_else(|| rest.strip_prefix("UPSTREAM_"))
        };
        if let Some(tail) = provider_rest {
            let Some((name, field)) = split_suffix(tail, PROVIDER_FIELDS) else {
                return Err(err(format!(
                    "{key}: not a recognised provider setting. Expected one of {}",
                    PROVIDER_FIELDS.join(", ")
                )));
            };
            providers
                .entry(env_name_to_id(name))
                .or_default()
                .insert(field.to_owned(), value.clone());
            continue;
        }
        if let Some(tail) = rest.strip_prefix("POOL_") {
            let (name, field) = match split_suffix(tail, POOL_FIELDS) {
                Some((n, f)) => (n, f),
                None => (tail, "_MEMBERS"),
            };
            pools
                .entry(env_name_to_id(name))
                .or_default()
                .insert(field.to_owned(), value.clone());
            continue;
        }
        if !apply_scalar(&mut config, rest, value)? {
            unknown.push(key.clone());
        }
    }

    if !unknown.is_empty() {
        unknown.sort();
        return Err(err(format!(
            "unrecognised setting(s): {}. Run `mini-router --help` for the list",
            unknown.join(", ")
        )));
    }

    for (name, fields) in providers {
        apply_provider(&mut config, &name, &fields, &lookup)?;
        sources.insert(name, Source::Declared);
    }

    let autodetect = match lookup.get("MINI_ROUTER_AUTODETECT") {
        Some(v) => parse_bool("MINI_ROUTER_AUTODETECT", v)?,
        // The zero-config path: keys alone are enough, but only when nothing
        // has been configured explicitly, so a stray OPENAI_API_KEY belonging
        // to some other tool cannot quietly add a provider.
        None => config.upstreams.is_empty(),
    };
    if autodetect {
        for (name, key_var, url, protocol) in WELL_KNOWN {
            if config.upstreams.iter().any(|u| u.name == *name) {
                continue;
            }
            let Some(key) = lookup.get(key_var) else {
                continue;
            };
            if key.is_empty() {
                continue;
            }
            config.upstreams.push(UpstreamConfig {
                name: (*name).to_owned(),
                url: (*url).to_owned(),
                protocol: *protocol,
                api_key_env: Some((*key_var).to_owned()),
                ..UpstreamConfig::stub(name, url)
            });
            sources.insert((*name).to_owned(), Source::Autodetected);
        }
    }

    // Provider order decides priority for requests that name a bare model id
    // rather than a pool. Alphabetical by default so it is at least stable;
    // explicit when it matters.
    if let Some(order) = lookup.get("MINI_ROUTER_PROVIDER_ORDER") {
        let wanted: Vec<String> = split_list(order)
            .iter()
            .map(|s| env_name_to_id(s))
            .collect();
        for name in &wanted {
            if !config.upstreams.iter().any(|u| u.name == *name) {
                return Err(err(format!(
                    "MINI_ROUTER_PROVIDER_ORDER names unknown provider {name:?}"
                )));
            }
        }
        config.upstreams.sort_by_key(|u| {
            wanted
                .iter()
                .position(|w| *w == u.name)
                .unwrap_or(usize::MAX)
        });
    }

    for (name, fields) in pools {
        apply_pool(&mut config, &name, &fields)?;
    }

    config.validate()?;
    Ok(Resolved { config, sources })
}

/// Split `MY_PROVIDER_API_KEY` into (`MY_PROVIDER`, `_API_KEY`).
fn split_suffix<'a>(tail: &'a str, fields: &[&'static str]) -> Option<(&'a str, &'static str)> {
    for field in fields {
        if let Some(name) = tail.strip_suffix(field) {
            if !name.is_empty() {
                return Some((name, field));
            }
        }
    }
    None
}

/// `MY_PROVIDER` -> `my-provider`. Environment variable names cannot hold a
/// dash, so underscores stand in for them.
fn env_name_to_id(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('_', "-")
}

fn apply_provider(
    config: &mut Config,
    name: &str,
    fields: &BTreeMap<String, String>,
    lookup: &BTreeMap<&str, &str>,
) -> Result<(), ConfigError> {
    let var = |f: &str| {
        format!(
            "{PREFIX}PROVIDER_{}{f}",
            name.to_uppercase().replace('-', "_")
        )
    };

    // Editing an existing provider from the file, or defining a new one.
    let existing = config.upstreams.iter().position(|u| u.name == name);
    let known = WELL_KNOWN.iter().find(|(n, ..)| *n == name);

    let mut up = match existing {
        Some(i) => config.upstreams[i].clone(),
        None => {
            let url = match fields.get("_URL") {
                Some(u) => u.clone(),
                // A well-known name does not need its URL spelled out.
                None => match known {
                    Some((_, _, url, _)) => (*url).to_owned(),
                    None => {
                        return Err(err(format!(
                            "{}: provider {name:?} needs a URL. Set {}",
                            var("_URL"),
                            var("_URL")
                        )))
                    }
                },
            };
            let mut stub = UpstreamConfig::stub(name, &url);
            if let Some((_, key_var, _, protocol)) = known {
                stub.protocol = *protocol;
                if lookup.contains_key(key_var) {
                    stub.api_key_env = Some((*key_var).to_owned());
                }
            }
            stub
        }
    };

    for (field, value) in fields {
        match field.as_str() {
            "_URL" => up.url = value.clone(),
            "_PROTOCOL" => {
                up.protocol = parse_protocol(&var("_PROTOCOL"), value)?;
            }
            "_API_KEY" => up.api_key = Some(value.clone()),
            "_API_KEY_ENV" => up.api_key_env = Some(value.clone()),
            "_WEIGHT" => up.weight = parse_num(&var("_WEIGHT"), value)?,
            "_MAX_CONCURRENCY" => {
                up.max_concurrency = parse_num(&var("_MAX_CONCURRENCY"), value)?;
            }
            "_MODELS" => up.models = split_list(value),
            "_FALLBACK_ONLY" => {
                up.fallback_only = parse_bool(&var("_FALLBACK_ONLY"), value)?;
            }
            "_HEADERS" => up.headers = parse_pairs(&var("_HEADERS"), value)?,
            other => return Err(err(format!("unhandled provider field {other}"))),
        }
    }

    match existing {
        Some(i) => config.upstreams[i] = up,
        None => config.upstreams.push(up),
    }
    Ok(())
}

fn apply_pool(
    config: &mut Config,
    name: &str,
    fields: &BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    let var = |f: &str| format!("{PREFIX}POOL_{}{f}", name.to_uppercase().replace('-', "_"));
    let mut pool = config.pools.get(name).cloned().unwrap_or_default();

    if let Some(members) = fields.get("_MEMBERS") {
        let weights: Vec<u32> = match fields.get("_WEIGHTS") {
            Some(w) => split_list(w)
                .iter()
                .map(|x| parse_num(&var("_WEIGHTS"), x))
                .collect::<Result<_, _>>()?,
            None => Vec::new(),
        };
        let parts = split_list(members);
        if !weights.is_empty() && weights.len() != parts.len() {
            return Err(err(format!(
                "{}: {} weight(s) for {} member(s); give one per member or none",
                var("_WEIGHTS"),
                weights.len(),
                parts.len()
            )));
        }
        pool.members = parts
            .iter()
            .enumerate()
            .map(|(i, part)| {
                // Split on the FIRST colon: a model id may well contain more,
                // as `local:qwen2.5:0.5b` does.
                let (upstream, model) = part.split_once(':').ok_or_else(|| {
                    err(format!(
                        "{}: member {part:?} should look like `provider:model`",
                        var("")
                    ))
                })?;
                if model.trim().is_empty() {
                    return Err(err(format!(
                        "{}: member {part:?} has no model after the colon",
                        var("")
                    )));
                }
                Ok(PoolMember {
                    upstream: upstream.trim().to_owned(),
                    model: model.trim().to_owned(),
                    weight: weights.get(i).copied().unwrap_or(1),
                })
            })
            .collect::<Result<Vec<_>, ConfigError>>()?;
    }

    if let Some(s) = fields.get("_STRATEGY") {
        pool.strategy = Some(parse_strategy(&var("_STRATEGY"), s)?);
    }
    if let Some(d) = fields.get("_DESCRIPTION") {
        pool.description = Some(d.clone());
    }

    if pool.members.is_empty() {
        return Err(err(format!(
            "{}: pool {name:?} has no members. Set {}=provider:model,provider:model",
            var(""),
            var("")
        )));
    }

    config.pools.insert(name.to_owned(), pool);
    Ok(())
}

/// Apply one non-repeating setting. Returns false if the name is unknown.
fn apply_scalar(config: &mut Config, rest: &str, value: &str) -> Result<bool, ConfigError> {
    let name = format!("{PREFIX}{rest}");
    match rest {
        // Handled elsewhere, but must not be reported as unknown.
        "CONFIG" | "LOG" | "AUTODETECT" | "PROVIDER_ORDER" => {}

        "LISTEN" => {
            config.server.listen = value
                .parse()
                .map_err(|e| err(format!("{name}: {value:?} is not an address: {e}")))?;
        }
        "WORKER_THREADS" => config.server.worker_threads = parse_num(&name, value)?,
        "LOG_LEVEL" => config.server.log_level = value.to_owned(),
        "MAX_BODY_BYTES" => config.server.max_body_bytes = parse_num(&name, value)?,
        "MAX_TRANSLATE_BYTES" => config.server.max_translate_bytes = parse_num(&name, value)?,
        "HEADER_TIMEOUT_SECS" => {
            config.server.upstream_header_timeout_secs = parse_num(&name, value)?;
        }
        "QUEUE_TIMEOUT_SECS" => config.server.queue_timeout_secs = parse_num(&name, value)?,
        "POOL_IDLE_TIMEOUT_SECS" => config.server.pool_idle_timeout_secs = parse_num(&name, value)?,
        "ADMIN" => config.server.admin = parse_bool(&name, value)?,
        "METRICS" => config.server.metrics = parse_bool(&name, value)?,

        "REQUIRE_AUTH" => config.server.auth.require_auth = parse_bool(&name, value)?,
        "API_KEYS" => config.server.auth.api_keys = split_list(value),
        "API_KEY_ENVS" => config.server.auth.api_key_envs = split_list(value),

        "STRATEGY" => config.balance.strategy = parse_strategy(&name, value)?,
        "SPILLOVER" => {
            config.balance.spillover = match value.trim().to_ascii_lowercase().as_str() {
                "any-error" | "any" | "all" => Spillover::AnyError,
                "status-list" | "status" | "list" => Spillover::StatusList,
                other => {
                    return Err(err(format!(
                        "{name}: {other:?} is not a spillover mode. Use any-error or status-list"
                    )))
                }
            };
        }
        "MAX_ATTEMPTS" => config.balance.max_attempts = parse_num(&name, value)?,
        "RETRY_ON_STATUS" => {
            config.balance.retry_on_status = split_list(value)
                .iter()
                .map(|s| parse_num(&name, s))
                .collect::<Result<_, _>>()?;
        }

        "HEALTH_INTERVAL_SECS" => config.health.interval_secs = parse_num(&name, value)?,
        "HEALTH_TIMEOUT_SECS" => config.health.timeout_secs = parse_num(&name, value)?,
        "HEALTH_PATH" => config.health.path = value.to_owned(),
        "FAILURE_THRESHOLD" => config.health.failure_threshold = parse_num(&name, value)?,
        "SUCCESS_THRESHOLD" => config.health.success_threshold = parse_num(&name, value)?,
        "COOLDOWN_SECS" => config.health.cooldown_secs = parse_num(&name, value)?,
        "MAX_COOLDOWN_SECS" => config.health.max_cooldown_secs = parse_num(&name, value)?,

        "DEFAULT_MAX_TOKENS" => config.translate.default_max_tokens = parse_num(&name, value)?,
        "ANTHROPIC_VERSION" => config.translate.anthropic_version = value.to_owned(),

        "ALIASES" => {
            for (from, to) in parse_pairs(&name, value)? {
                config.alias.insert(from, to);
            }
        }

        _ => return Ok(false),
    }
    Ok(true)
}

fn parse_strategy(name: &str, value: &str) -> Result<Strategy, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "priority" => Ok(Strategy::Priority),
        "round-robin" | "roundrobin" | "rr" => Ok(Strategy::RoundRobin),
        "least-conn" | "leastconn" => Ok(Strategy::LeastConn),
        "weighted" => Ok(Strategy::Weighted),
        "p2c-latency" | "p2c" | "latency" => Ok(Strategy::P2cLatency),
        other => Err(err(format!(
            "{name}: {other:?} is not a strategy. Use priority, round-robin, least-conn, \
             weighted or p2c-latency"
        ))),
    }
}

fn parse_protocol(name: &str, value: &str) -> Result<Protocol, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "openai" | "openai-compatible" | "oai" => Ok(Protocol::Openai),
        "anthropic" | "claude" => Ok(Protocol::Anthropic),
        other => Err(err(format!(
            "{name}: {other:?} is not a protocol. Use openai or anthropic"
        ))),
    }
}

fn parse_bool(name: &str, value: &str) -> Result<bool, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enabled" => Ok(true),
        "0" | "false" | "no" | "off" | "disabled" => Ok(false),
        other => Err(err(format!(
            "{name}: {other:?} is not a yes/no value. Use true or false"
        ))),
    }
}

fn parse_num<T>(name: &str, value: &str) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .trim()
        .parse()
        .map_err(|e| err(format!("{name}: {value:?} is not a number: {e}")))
}

/// Split a comma-separated list, ignoring blanks and surrounding space.
fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Split `a=1,b=2` into pairs.
fn parse_pairs(name: &str, value: &str) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut out = BTreeMap::new();
    for item in split_list(value) {
        let (k, v) = item
            .split_once('=')
            .ok_or_else(|| err(format!("{name}: {item:?} should look like `key=value`")))?;
        if k.trim().is_empty() {
            return Err(err(format!("{name}: {item:?} has an empty key")));
        }
        out.insert(k.trim().to_owned(), v.trim().to_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
