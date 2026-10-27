//! `wren-agent [--config PATH] [--check]`
//!
//! Reads `agent.toml` (`--config`, `WW_AGENT_CONFIG`, else
//! `/config/agent.toml`; `WW_SERVER_URL` and `WW_AGENT_TOKEN` override the
//! file's `server` and `token`), then reports to Wren until stopped.
//! `--check` only checks the configuration (and says which managed settings
//! from Wren are in force). A configuration problem exits
//! with code 2, naming the field.

use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::process::ExitCode;

use tracing_subscriber::EnvFilter;
use wren_agent::config::{self, Config};
use wren_agent::ledger::Ledger;
use wren_agent::managed::{self, Live};

const USAGE: &str = "usage: wren-agent [--config PATH] [--check] [--version]";

fn main() -> ExitCode {
    let mut path: Option<PathBuf> = std::env::var_os("WW_AGENT_CONFIG").map(PathBuf::from);
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--config" => match args.next() {
                Some(p) => path = Some(PathBuf::from(p)),
                None => {
                    eprintln!("{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "--version" | "-V" => {
                println!("wren-agent {}", wren_agent::heartbeat::VERSION);
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument {other}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let explicit = path.is_some();
    let path = path.unwrap_or_else(|| PathBuf::from(config::DEFAULT_PATH));
    let config = match Config::load(&path, explicit) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("wren-agent: the configuration has problems:\n{err}");
            return ExitCode::from(2);
        }
    };
    if check {
        // What is in force: agent.toml, with the settings Wren sent on top.
        let live = Live::new(
            config.clone(),
            managed::locked_by(|k| std::env::var(k).ok()),
        );
        println!("{}", live.describe());
        let config = live.config();
        let updates = if config.updates.configured() {
            format!(
                "updates only for {}",
                config
                    .updates
                    .allow
                    .as_deref()
                    .unwrap_or_default()
                    .join(", ")
            )
        } else {
            "NO [updates] ALLOW LIST: the agent refuses every update and rollback until agent.toml has [updates] allow = [\"*\"] (or a list of target names)".to_string()
        };
        println!(
            "{}: OK ({} roots, {} script stacks; {updates}{}; old images of the last {} update(s) kept per target)",
            path.display(),
            config.roots.len(),
            config.script_stacks.len(),
            if config.updates.deny.is_empty() {
                String::new()
            } else {
                format!(", never for {}", config.updates.deny.join(", "))
            },
            config.retention.rollback_images
        );
        println!(
            "health period at least {} s; snapshot {}",
            config.updates.min_health_seconds,
            if config.updates.require_snapshot {
                "required for every update (an update with no data folder to snapshot stops before anything is touched)"
            } else {
                "required only when Wren asks"
            }
        );
        println!(
            "run ledger {}: {}",
            config.ledger.display(),
            Ledger::status(&config.ledger)
        );
        let exists = std::path::Path::new(config::COMPOSE_MANAGER).is_dir();
        if let Some(hint) = config.compose_manager_hint(exists) {
            println!("{hint}");
        }
        return ExitCode::SUCCESS;
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("wren-agent: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(wren_agent::run(config, shutdown_signal())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err}");
            ExitCode::FAILURE
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
