//! mini-router entry point.

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::Arc;

use mini_router::config::Config;
use mini_router::env::{self, Source};
use mini_router::state::{init_crypto, AppState};
use mini_router::{health, router, version_line};

const USAGE: &str = "\
mini-router -- one OpenAI- and Anthropic-compatible endpoint over every
              LLM provider you use

USAGE:
    mini-router [OPTIONS]

    There is no configuration file. Everything below is an environment
    variable, so a docker compose `environment:` block is a whole deployment.

OPTIONS:
        --check            Print the resolved configuration and exit
    -l, --listen <ADDR>    Override MINI_ROUTER_LISTEN, e.g. 0.0.0.0:8080
    -h, --help             Print this help
    -V, --version          Print the version

ENVIRONMENT

  Providers -- the zero-config path is a key on its own:
    OPENAI_API_KEY, ANTHROPIC_API_KEY, GROQ_API_KEY, OPENROUTER_API_KEY,
    DEEPSEEK_API_KEY, MISTRAL_API_KEY, TOGETHER_API_KEY, XAI_API_KEY,
    GEMINI_API_KEY, CEREBRAS_API_KEY
        Any of these registers that provider with its usual URL and protocol.
        Only applies when nothing else is configured; MINI_ROUTER_AUTODETECT
        forces it on or off.

  Providers -- anything else, or to override a well-known one:
    MINI_ROUTER_PROVIDER_<NAME>_URL              https://host/v1
    MINI_ROUTER_PROVIDER_<NAME>_PROTOCOL         openai | anthropic
    MINI_ROUTER_PROVIDER_<NAME>_API_KEY          the key itself
    MINI_ROUTER_PROVIDER_<NAME>_API_KEY_ENV      name of the var holding it
    MINI_ROUTER_PROVIDER_<NAME>_MAX_CONCURRENCY  in-flight requests
    MINI_ROUTER_PROVIDER_<NAME>_WEIGHT           for the weighted strategy
    MINI_ROUTER_PROVIDER_<NAME>_MODELS           a,b,c (default: discover)
    MINI_ROUTER_PROVIDER_<NAME>_FALLBACK_ONLY    true | false
    MINI_ROUTER_PROVIDER_<NAME>_HEADERS          k=v,k=v
    MINI_ROUTER_PROVIDER_ORDER                   priority order of providers
        <NAME> is uppercase; underscores in it become dashes.

  Pools -- one client-facing model name over several provider models:
    MINI_ROUTER_POOL_<NAME>              provider:model,provider:model
    MINI_ROUTER_POOL_<NAME>_STRATEGY     overrides MINI_ROUTER_STRATEGY
    MINI_ROUTER_POOL_<NAME>_WEIGHTS      one per member
    MINI_ROUTER_POOL_<NAME>_DESCRIPTION  shown in the catalogue

  Routing:
    MINI_ROUTER_STRATEGY          priority | round-robin | least-conn |
                                  weighted | p2c-latency
    MINI_ROUTER_SPILLOVER         any-error | status-list
    MINI_ROUTER_RETRY_ON_STATUS   429,500,503 (status-list mode only)
    MINI_ROUTER_MAX_ATTEMPTS      0 = try every candidate
    MINI_ROUTER_ALIASES           name=target,name=target

  Server:
    MINI_ROUTER_LISTEN            0.0.0.0:8080
    MINI_ROUTER_WORKER_THREADS    2
    MINI_ROUTER_REQUIRE_AUTH      true | false
    MINI_ROUTER_API_KEYS          keys your own clients present
    MINI_ROUTER_API_KEY_ENVS      vars holding those keys
    MINI_ROUTER_ADMIN             false disables GET /admin/upstreams
    MINI_ROUTER_METRICS           false disables GET /metrics
    MINI_ROUTER_LOG               error | warn | info | debug | trace
    MINI_ROUTER_LOG_LEVEL         same, from config rather than runtime
    MINI_ROUTER_MAX_BODY_BYTES, MINI_ROUTER_MAX_TRANSLATE_BYTES,
    MINI_ROUTER_HEADER_TIMEOUT_SECS, MINI_ROUTER_QUEUE_TIMEOUT_SECS,
    MINI_ROUTER_POOL_IDLE_TIMEOUT_SECS

  Health and translation:
    MINI_ROUTER_HEALTH_INTERVAL_SECS, MINI_ROUTER_HEALTH_TIMEOUT_SECS,
    MINI_ROUTER_HEALTH_PATH, MINI_ROUTER_FAILURE_THRESHOLD,
    MINI_ROUTER_SUCCESS_THRESHOLD, MINI_ROUTER_COOLDOWN_SECS,
    MINI_ROUTER_MAX_COOLDOWN_SECS, MINI_ROUTER_DEFAULT_MAX_TOKENS,
    MINI_ROUTER_ANTHROPIC_VERSION

EXAMPLE (docker compose)
    environment:
      - OPENAI_API_KEY=sk-...
      - ANTHROPIC_API_KEY=sk-ant-...
      - MINI_ROUTER_POOL_FAST=openai:gpt-4o-mini,anthropic:claude-haiku-4-5
";

struct Args {
    check: bool,
    listen: Option<String>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut check = false;
    let mut listen = None;

    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("{}", version_line());
                return Ok(None);
            }
            "--check" => check = true,
            "-c" | "--config" => {
                return Err(
                    "mini-router has no configuration file: every setting is an environment \
                     variable. Run `mini-router --help` for the list."
                        .to_string(),
                );
            }
            "-l" | "--listen" => {
                listen = Some(
                    argv.next()
                        .ok_or_else(|| "--listen needs an address".to_string())?,
                );
            }
            other => return Err(format!("unknown argument {other:?}\n\n{USAGE}")),
        }
    }
    Ok(Some(Args { check, listen }))
}

fn print_check(cfg: &Config, sources: &BTreeMap<String, Source>) {
    println!(
        "configuration ok: {} provider(s), {} pool(s), strategy {}, spillover {}",
        cfg.upstreams.len(),
        cfg.pools.len(),
        cfg.balance.strategy,
        match cfg.balance.spillover {
            mini_router::config::Spillover::AnyError => "any-error",
            mini_router::config::Spillover::StatusList => "status-list",
        }
    );
    for up in &cfg.upstreams {
        let source = sources
            .get(&up.name)
            .map(Source::to_string)
            .unwrap_or_else(|| "set".into());
        println!(
            "  provider  {:<14} {:<10} [{:<4}] {}{}",
            up.name,
            up.protocol.to_string(),
            source,
            up.url,
            if up.resolve_key().is_some() {
                ""
            } else {
                "   !! no api key resolved"
            }
        );
    }
    for (name, pool) in &cfg.pools {
        let members: Vec<String> = pool
            .members
            .iter()
            .map(|m| format!("{}:{}", m.upstream, m.model))
            .collect();
        println!("  pool      {:<14} {}", name, members.join("  ->  "));
    }
    for (from, to) in &cfg.alias {
        println!("  alias     {:<14} ->  {}", from, to);
    }
    println!(
        "  endpoints /v1/chat/completions  /v1/messages  /v1/models  /healthz  /readyz{}{}",
        if cfg.server.metrics { "  /metrics" } else { "" },
        if cfg.server.admin {
            "  /admin/upstreams"
        } else {
            ""
        }
    );
    if cfg.server.auth.require_auth {
        println!("  auth      required ({} key(s))", cfg.client_keys().len());
    } else {
        println!("  auth      OPEN -- anyone who can reach the port can spend your credits");
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mini-router: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (mut cfg, sources) = match env::load(&env::vars()) {
        Ok(r) => (r.config, r.sources),
        Err(e) => {
            eprintln!("mini-router: configuration error: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(listen) = &args.listen {
        match listen.parse() {
            Ok(addr) => cfg.server.listen = addr,
            Err(e) => {
                eprintln!("mini-router: --listen {listen:?}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if args.check {
        print_check(&cfg, &sources);
        return ExitCode::SUCCESS;
    }

    init_logging(&cfg);
    init_crypto();

    let workers = if cfg.server.worker_threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
    } else {
        cfg.server.worker_threads
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        // 512 KiB is plenty for a proxy and saves real memory on a board where
        // the default 2 MiB per thread is a measurable fraction of RAM.
        .thread_stack_size(512 * 1024)
        .thread_name("mini-router")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("mini-router: could not start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(cfg, workers)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cfg: Config, workers: usize) -> Result<(), Box<dyn std::error::Error>> {
    let listen = cfg.server.listen;
    let state = Arc::new(AppState::new(cfg));

    tracing::info!(
        "{} listening on {listen} | {} provider(s) | {} pool(s) | strategy {} | {workers} worker thread(s)",
        version_line(),
        state.upstreams.len(),
        state.cfg.pools.len(),
        state.balancer.strategy(),
    );
    for up in &state.upstreams {
        tracing::info!(
            upstream = %up.name,
            protocol = %up.cfg.protocol,
            url = %up.cfg.url,
            max_concurrency = up.cfg.max_concurrency,
            has_key = up.api_key.is_some(),
            "provider registered"
        );
    }
    for (name, pool) in &state.cfg.pools {
        tracing::info!(
            pool = %name,
            members = pool.members.len(),
            "pool registered"
        );
    }

    if !state.cfg.server.auth.require_auth {
        tracing::warn!(
            "auth is off: anyone who can reach {listen} can spend your provider credits. \
             Set MINI_ROUTER_REQUIRE_AUTH=true and MINI_ROUTER_API_KEYS=..."
        );
    }

    let probes = health::spawn_probes(state.clone());
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind(listen).await?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    for p in probes {
        p.abort();
    }
    tracing::info!("shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => tracing::warn!("cannot listen for SIGTERM: {e}"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT received, draining"),
        _ = terminate => tracing::info!("SIGTERM received, draining"),
    }
}

fn init_logging(cfg: &Config) {
    let level = std::env::var("MINI_ROUTER_LOG").unwrap_or_else(|_| cfg.server.log_level.clone());
    let level = match level.to_ascii_lowercase().as_str() {
        "error" => tracing::Level::ERROR,
        "warn" => tracing::Level::WARN,
        "debug" => tracing::Level::DEBUG,
        "trace" => tracing::Level::TRACE,
        _ => tracing::Level::INFO,
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
}
