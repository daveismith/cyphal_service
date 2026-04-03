use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt};

mod cli;
mod commands;
mod daemon;

/// Cyphal Linux Service
///
/// Runs as a systemd daemon by default, or in foreground REPL mode with `--foreground`.
#[derive(Parser, Debug)]
#[command(name = "cyphal_service", version, about, long_about = None)]
struct Args {
    /// Run in foreground with an interactive REPL instead of daemonising.
    #[arg(short, long, default_value_t = false)]
    foreground: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Initialise logging.  When running as a systemd service the output is
    // captured by journald.  The log level can be overridden via the
    // `RUST_LOG` environment variable.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();

    if args.foreground {
        // Foreground / REPL mode – background tasks run via the same code
        // path as daemon mode; the REPL is layered on top.
        cli::run().await;
    } else {
        // Daemon mode – driven by the tokio async runtime.
        daemon::run().await;
    }
}
