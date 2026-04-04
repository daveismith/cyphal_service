use std::sync::Arc;
use std::time::Duration;

use super::Command;
use crate::transport::{TransportDiagnostics, TransportHandle};

/// Displays transport diagnostics for one or all configured transports.
///
/// Usage: `diag [<transport-name>]`
pub struct DiagCommand {
    handles: Arc<Vec<TransportHandle>>,
}

impl DiagCommand {
    pub fn new(handles: Arc<Vec<TransportHandle>>) -> Self {
        Self { handles }
    }
}

impl Command for DiagCommand {
    fn name(&self) -> &'static str {
        "diag"
    }

    fn description(&self) -> &'static str {
        "Show transport diagnostics. Usage: diag [<transport>]"
    }

    fn execute(&self, args: &[&str]) -> String {
        if self.handles.is_empty() {
            return "No transports configured.".to_string();
        }

        let selected: Vec<&TransportHandle> = if let Some(name) = args.first() {
            let found: Vec<_> = self.handles.iter().filter(|h| h.name == *name).collect();
            if found.is_empty() {
                return format!(
                    "Unknown transport '{name}'. Available: {}",
                    self.handles
                        .iter()
                        .map(|h| h.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            found
        } else {
            self.handles.iter().collect()
        };

        let mut sections = Vec::new();
        for handle in selected {
            match handle.diagnostics(Duration::from_secs(2)) {
                Ok(diagnostics) => sections.push(format_diagnostics(&handle.name, &diagnostics)),
                Err(error) => sections.push(format!(
                    "Transport: {}\n  Diagnostics error: {error}",
                    handle.name
                )),
            }
        }

        sections.join("\n\n")
    }
}

fn format_diagnostics(name: &str, diagnostics: &TransportDiagnostics) -> String {
    let recent_heartbeats = if diagnostics.recent_heartbeat_sources.is_empty() {
        "none".to_string()
    } else {
        diagnostics
            .recent_heartbeat_sources
            .iter()
            .map(|node_id| node_id.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };

    format!(
        "Transport: {name}\n  Local node ID        {}\n  Observed nodes        {}\n  Messages received     {}\n  Heartbeats received   {}\n  GetInfo req/res       {} / {}\n  Last message age      {}\n  Last heartbeat age    {}\n  Recent heartbeat IDs  {}\n  Last error            {}",
        diagnostics
            .local_node_id
            .map(|node_id| node_id.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        diagnostics.observed_node_count,
        diagnostics.total_messages_received,
        diagnostics.heartbeat_messages_received,
        diagnostics.get_info_requests_sent,
        diagnostics.get_info_responses_received,
        format_duration(diagnostics.last_message_age),
        format_duration(diagnostics.last_heartbeat_age),
        recent_heartbeats,
        diagnostics.last_error.as_deref().unwrap_or("none"),
    )
}

fn format_duration(duration: Option<Duration>) -> String {
    let Some(duration) = duration else {
        return "none".to_string();
    };

    if duration.as_secs() > 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diag_command_name() {
        let cmd = DiagCommand::new(Arc::new(vec![]));
        assert_eq!(cmd.name(), "diag");
    }

    #[test]
    fn test_diag_no_transports() {
        let cmd = DiagCommand::new(Arc::new(vec![]));
        assert_eq!(cmd.execute(&[]), "No transports configured.");
    }

    #[test]
    fn test_format_duration_none() {
        assert_eq!(format_duration(None), "none");
    }
}
