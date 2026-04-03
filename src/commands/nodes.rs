use std::sync::Arc;
use std::time::Duration;

use super::Command;
use crate::transport::TransportHandle;

/// Lists Cyphal nodes observed on one or all transports.
///
/// Usage: `nodes [<transport-name>]`
pub struct NodesCommand {
    handles: Arc<Vec<TransportHandle>>,
}

impl NodesCommand {
    pub fn new(handles: Arc<Vec<TransportHandle>>) -> Self {
        NodesCommand { handles }
    }
}

impl Command for NodesCommand {
    fn name(&self) -> &'static str {
        "nodes"
    }

    fn description(&self) -> &'static str {
        "List Cyphal nodes observed on transports. Usage: nodes [<transport>]"
    }

    fn execute(&self, args: &[&str]) -> String {
        if self.handles.is_empty() {
            return "No transports configured.".to_string();
        }

        let timeout = Duration::from_secs(2);

        // Filter to a specific transport if requested.
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
            let nodes = handle.list_nodes(timeout);
            output.push_str(&format!("Transport: {}\n", handle.name));
            if nodes.is_empty() {
                output.push_str("  (no nodes observed)\n");
            } else {
                output.push_str(&format!(
                    "  {:<8} {:<10} {:<10} {:<8}\n",
                    "Node ID", "Uptime(s)", "Health", "Mode"
                ));
                let mut sorted = nodes;
                sorted.sort_by_key(|n| n.node_id);
                for node in &sorted {
                    output.push_str(&format!(
                        "  {:<8} {:<10} {:<10} {:<8}\n",
                        node.node_id, node.uptime, node.health, node.mode
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
    fn test_nodes_no_transports() {
        let cmd = NodesCommand::new(Arc::new(vec![]));
        let result = cmd.execute(&[]);
        assert!(result.contains("No transports configured"));
    }

    #[test]
    fn test_nodes_name() {
        let cmd = NodesCommand::new(Arc::new(vec![]));
        assert_eq!(cmd.name(), "nodes");
    }
}
