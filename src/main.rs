//! `wise-wren`: the Wise Wren server. `wise-wren --help` lists the flags and
//! their `WW_` environment variables.

use std::io::IsTerminal as _;

use clap::Parser as _;
use tracing_subscriber::EnvFilter;
use wise_wren::config::Config;

#[tokio::main]
async fn main() {
    // vole-kit needs to know which product it runs in before anything else.
    wise_wren::install();
    let config = Config::parse();

    // Colours only on a terminal: `docker logs` and log files get plain text.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    // Every TLS stack in the binary uses rustls; pin its crypto provider so a
    // dependency enabling a second one can't make rustls panic at runtime.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    if let Err(err) = wise_wren::server::run(config).await {
        tracing::error!("{err}");
        std::process::exit(1);
    }
}
