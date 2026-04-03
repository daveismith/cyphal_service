use super::Command;

/// Prints a friendly "hello" message.
pub struct HelloCommand;

impl Command for HelloCommand {
    fn name(&self) -> &'static str {
        "hello"
    }

    fn description(&self) -> &'static str {
        "Print a greeting"
    }

    fn execute(&self, _args: &[&str]) -> String {
        "hello".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hello_execute() {
        let cmd = HelloCommand;
        assert_eq!(cmd.execute(&[]), "hello");
    }
}
