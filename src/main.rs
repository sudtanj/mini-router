//! mini-router entry point.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use mini_router::config::Config;
use mini_router::state::{init_crypto, AppState};
use mini_router::{health, router, version_line};

const USAGE: &str = "\
mini-router -- one OpenAI- and Anthropic-compatible endpoint over every
              LLM provider you use

USAGE:
    mini-router [OPTIONS]

OPTIONS:
    -c, --config <PATH>    Configuration file (default: ./mini-router.toml,
                           or $MINI_ROUTER_CONFIG)
        --check            Validate the configuration and exit
    -l, --listen <ADDR>    Override server.listen, e.g. 0.0.0.0:8080
    -h, --help             Print this help
    -V, --version          Print the version

ENVIRONMENT:
    MINI_ROUTER_CONFIG     Default configuration path
    MINI_ROUTER_LOG        Log level (error|warn|info|debug|trace), overrides
                           server.log_level
";

struct Args {
    config: PathBuf,
    check: bool,
    listen: Option<String>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut config = std::env::var("MINI_ROUTER_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("mini-router.toml"));
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
                config = argv
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| "--config needs a path".to_string())?;
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
    Ok(Some(Args {
        config,
        check,
        listen,
    }))
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

    let mut cfg = match Config::load(&args.config) {
        Ok(c) => c,
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
        println!(
            "configuration ok: {} provider(s), {} pool(s), strategy {}",
            cfg.upstreams.len(),
            cfg.pools.len(),
            cfg.balance.strategy
        );
        for up in &cfg.upstreams {
            println!(
                "  provider {:<16} {:<10} {}{}",
                up.name,
                up.protocol.to_string(),
                up.url,
                if up.resolve_key().is_some() {
                    ""
                } else {
                    "  (no api key resolved)"
                }
            );
        }
        for (name, pool) in &cfg.pools {
            let members: Vec<String> = pool
                .members
                .iter()
                .map(|m| format!("{}:{}", m.upstream, m.model))
                .collect();
            println!("  pool     {:<16} {}", name, members.join(" -> "));
        }
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
