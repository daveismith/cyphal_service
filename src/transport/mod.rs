//! Cyphal transport layer.
//!
//! This module manages one or more Cyphal transports, each running in a
//! dedicated background thread.  The REPL (and other callers) interact with
//! them through [`TransportHandle`] values that hold the command channel
//! sender.

pub mod can_gsusb;
#[cfg(target_os = "linux")]
pub mod can_socketcan;
pub mod serial;
pub mod udp;

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use canadensis_core::time::{Clock, Microseconds32};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::{AppConfig, CanDriver, TransportConfig};

// ─── public shared types ─────────────────────────────────────────────────────

/// A node observed on the network via heartbeat.
#[derive(Debug, Clone)]
pub struct NodeEntry {
    /// Cyphal node ID.
    pub node_id: u16,
    /// Node uptime in seconds (from the Heartbeat message).
    pub uptime: u32,
    /// Raw health value from the Heartbeat message.
    pub health: u8,
    /// Raw mode value from the Heartbeat message.
    pub mode: u8,
}

/// Information returned by a successful GetInfo request.
#[derive(Debug, Clone)]
pub struct GetInfoResult {
    pub node_id: u16,
    pub name: String,
    pub hardware_version: (u8, u8),
    pub software_version: (u8, u8),
    pub unique_id: [u8; 16],
}

/// Snapshot of per-transport diagnostic state.
#[derive(Debug, Clone)]
pub struct TransportDiagnostics {
    pub local_node_id: Option<u16>,
    pub observed_node_count: usize,
    pub total_messages_received: u64,
    pub heartbeat_messages_received: u64,
    pub get_info_requests_sent: u64,
    pub get_info_responses_received: u64,
    pub recent_heartbeat_sources: Vec<u16>,
    pub last_message_age: Option<Duration>,
    pub last_heartbeat_age: Option<Duration>,
    pub last_error: Option<String>,
    pub pnp_enabled: bool,
    #[allow(dead_code)]
    pub pnp_allocations_served: u64,
}

/// Commands sent from the REPL to a transport worker thread.
pub enum TransportCommand {
    /// Request the list of nodes observed via heartbeats.
    ListNodes {
        reply: mpsc::SyncSender<Vec<NodeEntry>>,
    },
    /// Request `uavcan.node.GetInfo` from a specific node.
    GetInfo {
        node_id: u16,
        reply: mpsc::SyncSender<Result<GetInfoResult, String>>,
    },
    /// Request diagnostic information about the transport worker.
    Diagnostics {
        reply: mpsc::SyncSender<TransportDiagnostics>,
    },
    /// Request all PNP allocations for this transport.
    #[allow(dead_code)]
    ListAllocations {
        reply: mpsc::SyncSender<Vec<crate::pnp::AllocationEntry>>,
    },
}

/// Handle to a running transport worker.
///
/// Clone it freely; all clones share the same command channel.
#[derive(Clone)]
pub struct TransportHandle {
    /// Human-readable name matching the config `name` field.
    pub name: String,
    local_node_id: Option<u16>,
    cmd_tx: mpsc::SyncSender<TransportCommand>,
}

impl TransportHandle {
    /// List all nodes observed on this transport (blocks up to `timeout`).
    pub fn list_nodes(&self, timeout: Duration) -> Vec<NodeEntry> {
        let (tx, rx) = mpsc::sync_channel(1);
        if self
            .cmd_tx
            .send(TransportCommand::ListNodes { reply: tx })
            .is_err()
        {
            return Vec::new();
        }
        let mut nodes = rx.recv_timeout(timeout).unwrap_or_default();
        if let Some(local_node_id) = self.local_node_id
            && !nodes.iter().any(|node| node.node_id == local_node_id)
        {
            nodes.push(NodeEntry {
                node_id: local_node_id,
                uptime: 0,
                health: 0,
                mode: 0,
            });
        }
        nodes
    }

    /// Send a GetInfo request to `node_id` (blocks up to `timeout`).
    pub fn get_info(&self, node_id: u16, timeout: Duration) -> Result<GetInfoResult, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        if self
            .cmd_tx
            .send(TransportCommand::GetInfo { node_id, reply: tx })
            .is_err()
        {
            return Err("transport worker has stopped".into());
        }
        rx.recv_timeout(timeout)
            .unwrap_or_else(|_| Err("timed out waiting for GetInfo response".into()))
    }

    /// Collect transport diagnostics (blocks up to `timeout`).
    pub fn diagnostics(&self, timeout: Duration) -> Result<TransportDiagnostics, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        if self
            .cmd_tx
            .send(TransportCommand::Diagnostics { reply: tx })
            .is_err()
        {
            return Err("transport worker has stopped".into());
        }

        let mut diagnostics = rx
            .recv_timeout(timeout)
            .map_err(|_| "timed out waiting for diagnostics".to_string())?;
        if diagnostics.local_node_id.is_none() {
            diagnostics.local_node_id = self.local_node_id;
        }
        Ok(diagnostics)
    }

    /// List all PNP allocations for this transport (blocks up to `timeout`).
    #[allow(dead_code)]
    pub fn list_allocations(&self, timeout: Duration) -> Vec<crate::pnp::AllocationEntry> {
        let (tx, rx) = mpsc::sync_channel(1);
        if self
            .cmd_tx
            .send(TransportCommand::ListAllocations { reply: tx })
            .is_err()
        {
            return Vec::new();
        }
        rx.recv_timeout(timeout).unwrap_or_default()
    }
}

// ─── cross-platform system clock ─────────────────────────────────────────────

/// A wall-clock that wraps elapsed microseconds into a `u32`.
///
/// Compatible with all platforms (does not depend on `canadensis_linux`).
#[derive(Debug, Clone)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&mut self) -> Microseconds32 {
        let elapsed = self.start.elapsed().as_micros() as u32;
        Microseconds32::from_ticks(elapsed)
    }
}

// ─── node discovery state shared by all worker types ─────────────────────────

/// Per-transport state shared between the poll loop and the command handler.
#[derive(Default)]
pub struct WorkerState {
    /// Nodes observed via heartbeat messages, keyed by node ID.
    pub observed_nodes: HashMap<u16, NodeEntry>,
    /// Pending GetInfo request, if any.
    pub pending_get_info: Option<PendingGetInfo>,
    /// Total successfully decoded Cyphal messages seen on the transport.
    pub total_messages_received: u64,
    /// Number of successfully decoded Heartbeat messages seen on the transport.
    pub heartbeat_messages_received: u64,
    /// Number of GetInfo requests issued by this transport worker.
    pub get_info_requests_sent: u64,
    /// Number of GetInfo responses received by this transport worker.
    pub get_info_responses_received: u64,
    /// Recent heartbeat source node IDs in order of arrival.
    pub recent_heartbeat_sources: VecDeque<u16>,
    /// Last time any Cyphal message was successfully decoded.
    pub last_message_at: Option<Instant>,
    /// Last time a heartbeat was received.
    pub last_heartbeat_at: Option<Instant>,
    /// Most recent worker error message, if any.
    pub last_error: Option<String>,
    /// Number of PNP node-ID allocations served by this transport.
    pub pnp_allocations_served: u64,
}

/// An in-progress GetInfo request.
pub struct PendingGetInfo {
    /// Target node ID.
    pub node_id: u16,
    /// Channel to send the result back on.
    pub reply: mpsc::SyncSender<Result<GetInfoResult, String>>,
    /// Request deadline.
    pub deadline: Instant,
}

impl WorkerState {
    /// Register the local node so it appears in the node list even when the
    /// transport does not loop back its own heartbeat frames.
    pub fn register_local_node(&mut self, node_id: u16) {
        self.observed_nodes.entry(node_id).or_insert(NodeEntry {
            node_id,
            uptime: 0,
            health: 0,
            mode: 0,
        });
    }

    /// Advance the local node uptime once per second.
    pub fn tick_local_node(&mut self, node_id: u16) {
        self.register_local_node(node_id);
        if let Some(node) = self.observed_nodes.get_mut(&node_id) {
            node.uptime = node.uptime.saturating_add(1);
        }
    }

    /// Process an incoming heartbeat from `node_id`.
    pub fn on_heartbeat(&mut self, node_id: u16, uptime: u32, health: u8, mode: u8) {
        let now = Instant::now();
        self.total_messages_received = self.total_messages_received.saturating_add(1);
        self.heartbeat_messages_received = self.heartbeat_messages_received.saturating_add(1);
        self.last_message_at = Some(now);
        self.last_heartbeat_at = Some(now);
        self.recent_heartbeat_sources.push_back(node_id);
        while self.recent_heartbeat_sources.len() > 8 {
            self.recent_heartbeat_sources.pop_front();
        }
        self.observed_nodes.insert(
            node_id,
            NodeEntry {
                node_id,
                uptime,
                health,
                mode,
            },
        );
    }

    /// Provide a completed GetInfo result.
    pub fn on_get_info_response(&mut self, result: Result<GetInfoResult, String>) {
        self.get_info_responses_received = self.get_info_responses_received.saturating_add(1);
        if let Some(pending) = self.pending_get_info.take() {
            let _ = pending.reply.send(result);
        }
    }

    /// Record a successful outbound GetInfo request.
    pub fn on_get_info_request_sent(&mut self) {
        self.get_info_requests_sent = self.get_info_requests_sent.saturating_add(1);
    }

    /// Record that a non-heartbeat Cyphal message was successfully received.
    pub fn on_message_received(&mut self) {
        self.total_messages_received = self.total_messages_received.saturating_add(1);
        self.last_message_at = Some(Instant::now());
    }

    /// Record a transport-worker error for later diagnostics.
    pub fn record_error(&mut self, error: impl Into<String>) {
        self.last_error = Some(error.into());
    }

    /// Increment the PNP allocation counter.
    pub fn on_pnp_allocated(&mut self) {
        self.pnp_allocations_served = self.pnp_allocations_served.saturating_add(1);
    }

    /// Time out the pending GetInfo if the deadline has passed.
    pub fn check_get_info_timeout(&mut self) {
        if self
            .pending_get_info
            .as_ref()
            .is_some_and(|p| Instant::now() > p.deadline)
        {
            let reply = self.pending_get_info.take().unwrap().reply;
            let _ = reply.send(Err("GetInfo request timed out".into()));
        }
    }

    /// Return a snapshot of the current node list.
    pub fn node_list(&self) -> Vec<NodeEntry> {
        self.observed_nodes.values().cloned().collect()
    }

    /// Build a diagnostics snapshot from the current worker state.
    pub fn diagnostics(&self, local_node_id: Option<u16>) -> TransportDiagnostics {
        TransportDiagnostics {
            local_node_id,
            observed_node_count: self.observed_nodes.len(),
            total_messages_received: self.total_messages_received,
            heartbeat_messages_received: self.heartbeat_messages_received,
            get_info_requests_sent: self.get_info_requests_sent,
            get_info_responses_received: self.get_info_responses_received,
            recent_heartbeat_sources: self.recent_heartbeat_sources.iter().copied().collect(),
            last_message_age: self.last_message_at.map(|at| at.elapsed()),
            last_heartbeat_age: self.last_heartbeat_at.map(|at| at.elapsed()),
            last_error: self.last_error.clone(),
            pnp_enabled: false, // caller sets this
            pnp_allocations_served: self.pnp_allocations_served,
        }
    }
}

// ─── transport manager ────────────────────────────────────────────────────────

/// Start all configured transports and return their handles.
///
/// Each transport runs in its own `tokio::task::spawn_blocking` thread.
/// When `shutdown` fires, all workers stop.
pub async fn start_all(
    config: &AppConfig,
    shutdown: watch::Receiver<bool>,
) -> Vec<TransportHandle> {
    let mut handles = Vec::new();

    for tc in &config.transports {
        match tc {
            TransportConfig::Can(cfg) => match cfg.driver {
                CanDriver::Gsusb => {
                    let handle = can_gsusb::start(cfg.clone(), shutdown.clone());
                    handles.push(handle);
                }
                CanDriver::Socketcan => {
                    #[cfg(target_os = "linux")]
                    {
                        let handle = can_socketcan::start(cfg.clone(), shutdown.clone());
                        handles.push(handle);
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        warn!(
                            transport = %cfg.name,
                            "SocketCAN is not supported on this platform – skipping transport"
                        );
                    }
                }
            },
            TransportConfig::Udp(cfg) => {
                let handle = udp::start(cfg.clone(), shutdown.clone());
                handles.push(handle);
            }
            TransportConfig::Serial(cfg) => {
                let handle = serial::start(cfg.clone(), shutdown.clone());
                handles.push(handle);
            }
        }
    }

    if handles.is_empty() {
        info!("No transports configured – running with REPL only");
    } else {
        info!("Started {} transport(s)", handles.len());
    }

    handles
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Send a log warning if `warn_on_err` is true and return `true` if there is
/// an error condition, for use in poll loops.
#[allow(dead_code)]
pub(crate) fn log_err_and_continue<E: std::fmt::Debug>(
    result: &Result<(), E>,
    transport: &str,
) -> bool {
    if let Err(e) = result {
        warn!(transport, "Transport error: {e:?}");
        true
    } else {
        false
    }
}

/// Build the standard `GetInfoResponse` describing this cyphal_service node.
pub(crate) fn make_node_info(
    transport_name: &str,
) -> canadensis_data_types::uavcan::node::get_info_1_0::GetInfoResponse {
    use canadensis_data_types::uavcan::node::get_info_1_0::GetInfoResponse;
    use canadensis_data_types::uavcan::node::version_1_0::Version;

    let mut name_bytes = heapless::Vec::new();
    let node_name = format!("org.cyphal_service.{transport_name}");
    for b in node_name.bytes().take(50) {
        let _ = name_bytes.push(b);
    }

    GetInfoResponse {
        protocol_version: Version { major: 1, minor: 0 },
        hardware_version: Version { major: 0, minor: 0 },
        software_version: Version { major: 0, minor: 1 },
        software_vcs_revision_id: 0,
        unique_id: rand::random(),
        name: name_bytes,
        software_image_crc: Default::default(),
        certificate_of_authenticity: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::WorkerState;

    #[test]
    fn local_node_is_listed_immediately() {
        let mut state = WorkerState::default();
        state.register_local_node(43);

        let nodes = state.node_list();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_id, 43);
        assert_eq!(nodes[0].uptime, 0);
    }

    #[test]
    fn local_node_uptime_advances() {
        let mut state = WorkerState::default();
        state.register_local_node(43);
        state.tick_local_node(43);
        state.tick_local_node(43);

        let nodes = state.node_list();
        assert_eq!(nodes[0].uptime, 2);
    }
}
