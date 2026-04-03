use super::Command;

#[cfg(test)]
use super::CommandRegistry;

/// Displays the list of available commands.
pub struct HelpCommand;

impl Command for HelpCommand {
    fn name(&self) -> &'static str {
        "help"
    }

    fn description(&self) -> &'static str {
        "List available commands"
    }

    /// This implementation is a placeholder; the REPL handles `help` specially
    /// so that it can pass the live registry listing.
    fn execute(&self, _args: &[&str]) -> String {
        "Use the REPL to see available commands.".to_string()
    }
}

/// Generate the help text from a registry listing.
pub fn format_help(entries: &[(&str, &str)]) -> String {
    let mut output = String::from("Available commands:\n");
    for (name, desc) in entries {
        output.push_str(&format!("  {name:<16} {desc}\n"));
    }
    output.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_help_empty() {
        let help = format_help(&[]);
        assert_eq!(help, "Available commands:");
    }

    #[test]
    fn test_format_help_single() {
        let entries = vec![("hello", "Print a greeting")];
        let help = format_help(&entries);
        assert!(help.contains("hello"));
        assert!(help.contains("Print a greeting"));
    }

    #[test]
    fn test_help_command_name() {
        let cmd = HelpCommand;
        assert_eq!(cmd.name(), "help");
    }

    #[test]
    fn test_help_registered_in_registry() {
        let mut registry = CommandRegistry::new();
        registry.register(Box::new(HelpCommand));
        let list = registry.list();
        assert!(list.iter().any(|(name, _)| *name == "help"));
    }
}
