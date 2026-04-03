use std::time::Duration;
use tokio::time;
use tracing::{info, warn};

/// Interval between "hello" ticks.
const TICK_INTERVAL: Duration = Duration::from_secs(60);

/// Run the service in daemon mode.
///
/// Outputs "hello" every [`TICK_INTERVAL`] and exits cleanly on SIGTERM or
/// SIGINT.  When running under systemd the process notifies readiness via
/// `sd_notify`.
pub async fn run() {
    // Notify systemd that we are ready (no-op when not running under systemd).
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        warn!("sd_notify ready failed (not running under systemd?): {e}");
    }

    info!(
        "Daemon started – ticking every {} seconds",
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
            _ = shutdown_signal() => {
                info!("Shutdown signal received – stopping daemon");
                break;
            }
        }
    }

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
