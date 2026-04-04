use std::time::Duration;
use tokio::sync::watch;
use tokio::time;
use tracing::{info, warn};

use crate::config::AppConfig;
use crate::transport;

/// Interval between "hello" ticks.
const TICK_INTERVAL: Duration = Duration::from_secs(60);

/// Run all background service tasks.
///
/// This is the single source of truth for every background task in the
/// service.  Both daemon mode and foreground mode call this function, so a new
/// background task only ever needs to be added here.
///
/// The function runs until `shutdown` receives `true`, then returns.
pub async fn run_tasks(mut shutdown: watch::Receiver<bool>) {
    info!(
        "Background tasks started – ticking every {} seconds",
        TICK_INTERVAL.as_secs()
    );

    let mut interval = time::interval(TICK_INTERVAL);
    // The first tick fires immediately; consume it so the first "hello" comes
    // after a full minute.
    interval.tick().await;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                info!("hello");
                println!("hello");
            }
            _ = shutdown.changed() => {
                info!("Background tasks stopping");
                break;
            }
        }
    }
}

/// Run the service in daemon mode.
///
/// Starts [`run_tasks`] and exits cleanly on SIGTERM or SIGINT.  When running
/// under systemd the process notifies readiness via `sd_notify`.
pub async fn run(config: AppConfig) {
    // Notify systemd that we are ready (no-op when not running under systemd).
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        warn!("sd_notify ready failed (not running under systemd?): {e}");
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Start transport workers.
    let _handles = transport::start_all(&config, shutdown_rx.clone()).await;

    let tasks = tokio::spawn(run_tasks(shutdown_rx));

    shutdown_signal().await;
    info!("Shutdown signal received – stopping daemon");

    let _ = shutdown_tx.send(true);
    tasks.await.ok();

    // Notify systemd that we are stopping.
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]) {
        warn!("sd_notify stopping failed: {e}");
    }
}

/// Wait for SIGTERM or SIGINT.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl_c");
    }
}
