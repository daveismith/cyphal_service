use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, Result as RlResult};
use tracing::info;

use crate::commands::{CommandRegistry, hello::HelloCommand, help::HelpCommand, help::format_help};

/// Run the service in foreground / REPL mode.
///
/// A background thread ticks every 60 seconds printing "hello" to stdout.
/// The foreground thread runs a readline REPL allowing the operator to
/// interact with the service via the [`CommandRegistry`].
pub fn run() {
    let running = Arc::new(AtomicBool::new(true));

    // Background tick thread.
    let running_bg = running.clone();
    let tick_handle = std::thread::spawn(move || {
        while running_bg.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(60));
            if running_bg.load(Ordering::Relaxed) {
                info!("hello");
                println!("[tick] hello");
            }
        }
    });

    // Build the command registry with all built-in commands.
    let mut registry = CommandRegistry::new();
    register_defaults(&mut registry);

    println!("cyphal_service – foreground mode");
    println!("Type 'help' for available commands, 'quit' or 'exit' to stop.");

    if let Err(e) = repl_loop(&registry, &running) {
        eprintln!("REPL error: {e}");
    }

    running.store(false, Ordering::Relaxed);
    tick_handle.join().ok();
}

/// Register all built-in commands into the registry.
pub fn register_defaults(registry: &mut CommandRegistry) {
    registry.register(Box::new(HelloCommand));
    registry.register(Box::new(HelpCommand));
}

fn repl_loop(registry: &CommandRegistry, running: &Arc<AtomicBool>) -> RlResult<()> {
    let mut rl = DefaultEditor::new()?;

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
                        running.store(false, Ordering::Relaxed);
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
                running.store(false, Ordering::Relaxed);
                break;
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}
