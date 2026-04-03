use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio::sync::watch;
use tracing::info;

use crate::commands::{CommandRegistry, hello::HelloCommand, help::HelpCommand, help::format_help};
use crate::daemon;

/// Run the service in foreground / REPL mode.
///
/// The same background tasks that run in daemon mode (via [`daemon::run_tasks`])
/// are started on the tokio runtime.  The REPL is then layered on top in a
/// dedicated OS thread (via [`tokio::task::spawn_blocking`]) so that blocking
/// readline calls do not stall the async runtime.  When the REPL exits the
/// background tasks are signalled to stop through the shared shutdown channel.
pub async fn run() {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Start background tasks – identical to daemon mode.
    let tasks = tokio::spawn(daemon::run_tasks(shutdown_rx));

    // Build the command registry with all built-in commands.
    let mut registry = CommandRegistry::new();
    register_defaults(&mut registry);

    println!("cyphal_service – foreground mode");
    println!("Type 'help' for available commands, 'quit' or 'exit' to stop.");

    // Run the blocking REPL in a dedicated OS thread so it doesn't stall the
    // tokio runtime.  When the closure returns the shutdown signal is sent.
    tokio::task::spawn_blocking(move || repl_loop(registry, shutdown_tx))
        .await
        .ok();

    tasks.await.ok();
}

/// Register all built-in commands into the registry.
pub fn register_defaults(registry: &mut CommandRegistry) {
    registry.register(Box::new(HelloCommand));
    registry.register(Box::new(HelpCommand));
}

fn repl_loop(registry: CommandRegistry, shutdown_tx: watch::Sender<bool>) {
    let mut rl = match DefaultEditor::new() {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("REPL initialisation error: {e}");
            return;
        }
    };

    loop {
        match rl.readline("cyphal> ") {
            Ok(line) => {
                let trimmed = line.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }

                rl.add_history_entry(&trimmed).ok();

                match trimmed.as_str() {
                    "quit" | "exit" => {
                        println!("Goodbye.");
                        break;
                    }
                    "help" => {
                        println!("{}", format_help(&registry.list()));
                    }
                    _ => {
                        if let Some(output) = registry.dispatch(&trimmed) {
                            println!("{output}");
                        }
                    }
                }
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                println!("Goodbye.");
                break;
            }
            Err(e) => {
                eprintln!("REPL error: {e}");
                break;
            }
        }
    }

    // Signal background tasks to stop regardless of how the REPL exited.
    let _ = shutdown_tx.send(true);
    info!("Foreground REPL exited – stopping background tasks");
}
