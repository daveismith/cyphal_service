use std::sync::Arc;
use std::time::Duration;

use super::Command;
use crate::transport::TransportHandle;

/// Issues a `uavcan.node.GetInfo` request to a specific node.
///
/// Usage: `get-info <transport> <node-id>`
pub struct GetInfoCommand {
    handles: Arc<Vec<TransportHandle>>,
}

impl GetInfoCommand {
    pub fn new(handles: Arc<Vec<TransportHandle>>) -> Self {
        GetInfoCommand { handles }
    }
}

impl Command for GetInfoCommand {
    fn name(&self) -> &'static str {
        "get-info"
    }

    fn description(&self) -> &'static str {
        "Query a node's GetInfo. Usage: get-info <transport> <node-id>"
    }

    fn execute(&self, args: &[&str]) -> String {
        if args.len() < 2 {
            return "Usage: get-info <transport> <node-id>".to_string();
        }

        let transport_name = args[0];
        let node_id_str = args[1];

        let node_id: u16 = match node_id_str.parse() {
            Ok(id) => id,
            Err(_) => return format!("Invalid node ID '{node_id_str}': must be a number"),
        };

        let handle = match self.handles.iter().find(|h| h.name == transport_name) {
            Some(h) => h,
            None => {
                return format!(
                    "Unknown transport '{transport_name}'. Available: {}",
                    self.handles
                        .iter()
                        .map(|h| h.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        };

        match handle.get_info(node_id, Duration::from_secs(5)) {
            Ok(info) => {
                let uid_hex: String = info
                    .unique_id
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(":");
                format!(
                    "Node {}  ({})\n\
                     Hardware : {}.{}\n\
                     Software : {}.{}\n\
                     Unique ID: {uid_hex}",
                    info.node_id,
                    info.name,
                    info.hardware_version.0,
                    info.hardware_version.1,
                    info.software_version.0,
                    info.software_version.1,
                )
            }
            Err(e) => format!("GetInfo failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_info_name() {
        let cmd = GetInfoCommand::new(Arc::new(vec![]));
        assert_eq!(cmd.name(), "get-info");
    }

    #[test]
    fn test_get_info_missing_args() {
        let cmd = GetInfoCommand::new(Arc::new(vec![]));
        let result = cmd.execute(&[]);
        assert!(result.contains("Usage:"));
    }

    #[test]
    fn test_get_info_bad_node_id() {
        let cmd = GetInfoCommand::new(Arc::new(vec![]));
        let result = cmd.execute(&["my-transport", "not-a-number"]);
        assert!(result.contains("Invalid node ID"));
    }

    #[test]
    fn test_get_info_unknown_transport() {
        let cmd = GetInfoCommand::new(Arc::new(vec![]));
        let result = cmd.execute(&["nonexistent", "42"]);
        assert!(result.contains("Unknown transport"));
    }
}
