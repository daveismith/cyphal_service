use std::sync::Arc;
use std::time::Duration;

use super::Command;
use crate::transport::TransportHandle;

/// Lists PNP node-ID allocations for one or all configured transports.
///
/// Usage: `pnp-list [<transport-name>]`
pub struct PnpListCommand {
    handles: Arc<Vec<TransportHandle>>,
}

impl PnpListCommand {
    pub fn new(handles: Arc<Vec<TransportHandle>>) -> Self {
        Self { handles }
    }
}

impl Command for PnpListCommand {
    fn name(&self) -> &'static str {
        "pnp-list"
    }

    fn description(&self) -> &'static str {
        "List PNP node-ID allocations. Usage: pnp-list [<transport>]"
    }

    fn execute(&self, args: &[&str]) -> String {
        if self.handles.is_empty() {
            return "No transports configured.".to_string();
        }

        let timeout = Duration::from_secs(2);

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

        let mut output = String::new();

        for handle in &selected {
            let entries = handle.list_allocations(timeout);
            output.push_str(&format!("Transport: {}\n", handle.name));
            if entries.is_empty() {
                output.push_str("  (no allocations)\n");
            } else {
                output.push_str(&format!(
                    "  {:<8} {:<34} {:<20} {}\n",
                    "Node ID", "Unique ID (hex)", "Pseudo UID", "Timestamp"
                ));
                for entry in &entries {
                    let pseudo = entry
                        .pseudo_unique_id
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "NULL".to_string());
                    output.push_str(&format!(
                        "  {:<8} {:<34} {:<20} {}\n",
                        entry.node_id, entry.unique_id_hex, pseudo, entry.ts
                    ));
                }
            }
        }

        output.trim_end().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pnp_list_name() {
        let cmd = PnpListCommand::new(Arc::new(vec![]));
        assert_eq!(cmd.name(), "pnp-list");
    }

    #[test]
    fn test_pnp_list_no_transports() {
        let cmd = PnpListCommand::new(Arc::new(vec![]));
        assert_eq!(cmd.execute(&[]), "No transports configured.");
    }

    #[test]
    fn test_pnp_list_description() {
        let cmd = PnpListCommand::new(Arc::new(vec![]));
        assert!(cmd.description().contains("pnp-list"));
    }
}
