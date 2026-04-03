use std::collections::HashMap;

pub mod get_info;
pub mod hello;
pub mod help;
pub mod nodes;

/// A command that can be registered with and executed via the [`CommandRegistry`].
pub trait Command: Send + Sync {
    /// The primary name used to invoke this command.
    fn name(&self) -> &'static str;
    /// A short human-readable description shown in help output.
    fn description(&self) -> &'static str;
    /// Execute the command with the given arguments and return output text.
    fn execute(&self, args: &[&str]) -> String;
}

/// Registry that holds all registered commands and dispatches input lines to them.
pub struct CommandRegistry {
    commands: HashMap<String, Box<dyn Command>>,
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRegistry {
    /// Create a new, empty registry.
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
        }
    }

    /// Register a command.  The command is keyed by [`Command::name`].
    pub fn register(&mut self, cmd: Box<dyn Command>) {
        self.commands.insert(cmd.name().to_string(), cmd);
    }

    /// Dispatch a raw input line.  Returns `None` when the line is empty.
    pub fn dispatch(&self, line: &str) -> Option<String> {
        let mut parts = line.split_whitespace();
        let cmd_name = parts.next()?;
        let args: Vec<&str> = parts.collect();

        match self.commands.get(cmd_name) {
            Some(cmd) => Some(cmd.execute(&args)),
            None => Some(format!(
                "Unknown command: '{cmd_name}'.  Type 'help' for available commands."
            )),
        }
    }

    /// Return a sorted list of (name, description) pairs for every registered command.
    pub fn list(&self) -> Vec<(&str, &str)> {
        let mut entries: Vec<(&str, &str)> = self
            .commands
            .values()
            .map(|c| (c.name(), c.description()))
            .collect();
        entries.sort_by_key(|(name, _)| *name);
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoCommand;

    impl Command for EchoCommand {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "Echo arguments back"
        }
        fn execute(&self, args: &[&str]) -> String {
            args.join(" ")
        }
    }

    #[test]
    fn test_register_and_dispatch() {
        let mut registry = CommandRegistry::new();
        registry.register(Box::new(EchoCommand));

        let result = registry.dispatch("echo foo bar");
        assert_eq!(result, Some("foo bar".to_string()));
    }

    #[test]
    fn test_dispatch_unknown_command() {
        let registry = CommandRegistry::new();
        let result = registry.dispatch("nonexistent");
        assert!(result.unwrap().contains("Unknown command"));
    }

    #[test]
    fn test_dispatch_empty_line() {
        let registry = CommandRegistry::new();
        assert_eq!(registry.dispatch(""), None);
        assert_eq!(registry.dispatch("   "), None);
    }

    #[test]
    fn test_list_commands() {
        let mut registry = CommandRegistry::new();
        registry.register(Box::new(EchoCommand));
        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "echo");
    }
}
