//! Runs the web server until Ctrl-C or SIGTERM, then lets requests in
//! flight finish.

use std::net::SocketAddr;

use crate::config::Config;

/// Starts Wren with `config` and serves until it is told to stop.
pub async fn run(config: Config) -> Result<(), crate::StartError> {
    let wren = crate::start(&config.data_dir, config.public_url.as_deref()).await?;
    if config.allow_private_version_urls {
        wren.allow_private_fetch
            .store(true, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            "WW_ALLOW_PRIVATE_VERSION_URLS is on: version URLs may reach private and local addresses"
        );
    }
    // Notifications and other work outside a request use the site language.
    vole_kit::i18n::site_language(&wren.core).await;
    // Turns silent agents offline on the live pages.
    crate::servers::spawn_watcher(wren.clone());
    // Version checks on each target's interval, and on "Check now".
    crate::checks::spawn_scheduler(wren.clone());
    // Closes runs whose agent went silent.
    crate::runs::spawn_watchdog(wren.clone());
    // Auto runs, only inside each server's auto window.
    crate::schedule::spawn_picker(wren.clone());
    // Vigilant Vole: planned windows ahead of scheduled runs, closing them.
    crate::vv::spawn_sync(wren.clone());

    let addr = config.bind_addr();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("can't listen on {addr}: {e}"))?;
    tracing::info!(
        version = crate::VERSION,
        data_dir = %config.data_dir.display(),
        "Wise Wren listening on http://{addr}"
    );
    // Connect info lets the rate limiters key on the peer address.
    axum::serve(
        listener,
        crate::router(wren).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    tracing::info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(%err, "can't listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::warn!(%err, "can't listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}
