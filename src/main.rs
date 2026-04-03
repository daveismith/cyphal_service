use clap::Parser;
use std::path::PathBuf;
use tracing_subscriber::{EnvFilter, fmt};

mod cli;
mod commands;
mod config;
mod daemon;
mod transport;

/// Cyphal Linux Service
///
/// Runs as a systemd daemon by default, or in foreground REPL mode with `--foreground`.
#[derive(Parser, Debug)]
#[command(name = "cyphal_service", version, about, long_about = None)]
struct Args {
    /// Run in foreground with an interactive REPL instead of daemonising.
    #[arg(short, long, default_value_t = false)]
    foreground: bool,

    /// Path to a TOML configuration file for Cyphal transports.
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Initialise logging.  When running as a systemd service the output is
    // captured by journald.  The log level can be overridden via the
    // `RUST_LOG` environment variable.
    tracing_log::LogTracer::init().ok();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();

    // Load the configuration file (if provided).
    let app_config = match config::load_config(args.config.as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Configuration error: {e}");
            std::process::exit(1);
        }
    };

    if args.foreground {
        // Foreground / REPL mode – background tasks run via the same code
        // path as daemon mode; the REPL is layered on top.
        cli::run(app_config).await;
    } else {
        // Daemon mode – driven by the tokio async runtime.
        daemon::run(app_config).await;
    }
}
