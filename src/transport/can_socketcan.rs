//! Cyphal/CAN transport via Linux SocketCAN.
//!
//! This module is only compiled on Linux (`#[cfg(target_os = "linux")]`).
//! It wraps the `canadensis_linux::LinuxCan` adapter around a `socketcan` socket.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use canadensis::core::transfer::{MessageTransfer, ServiceTransfer};
use canadensis::node::{BasicNode, CoreNode};
use canadensis::requester::TransferIdFixedMap;
use canadensis::{Node, ServiceToken, TransferHandler};
use canadensis_can::queue::{ArrayQueue, SingleQueueDriver};
use canadensis_can::{
    CanNodeId, CanReceiver, CanTransmitter, CanTransport, Error as CanError, Mtu,
};
use canadensis_core::Priority;
use canadensis_core::time::milliseconds;
use canadensis_data_types::uavcan::node::get_info_1_0::{
    GetInfoRequest, GetInfoResponse, SERVICE as GET_INFO_SERVICE,
};
use canadensis_data_types::uavcan::node::heartbeat_1_0::{Heartbeat, SUBJECT as HEARTBEAT_SUBJECT};
use canadensis_encoding::Deserialize;
use canadensis_linux::{LinuxCan, SystemClock};
use socketcan::{CanSocket, Socket};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::config::CanConfig;
use crate::transport::{
    GetInfoResult, PendingGetInfo, PnpGetInfoPending, TransportCommand, TransportHandle,
    WorkerState,
};

use canadensis_data_types::uavcan::pnp::node_id_allocation_data_1_0::{
    NodeIDAllocationData as AllocationDataV1, SUBJECT as PNP_V1_SUBJECT,
};
use canadensis_data_types::uavcan::pnp::node_id_allocation_data_2_0::{
    NodeIDAllocationData as AllocationDataV2, SUBJECT as PNP_V2_SUBJECT,
};

use crate::config::effective_pnp_db_path;
use crate::pnp::PnpAllocator;

// ─── Canadensis node type aliases ─────────────────────────────────────────────

const QUEUE_CAPACITY: usize = 1210;
const NUM_PUBLISHERS: usize = 8;
const NUM_REQUESTERS: usize = 4;
const NUM_TRANSFER_IDS: usize = 4;

type SocketCanQueue =
    SingleQueueDriver<SystemClock, ArrayQueue<QUEUE_CAPACITY>, LinuxCan<CanSocket>>;

type SocketCanNode = BasicNode<
    CoreNode<
        SystemClock,
        CanTransmitter<SystemClock, SocketCanQueue>,
        CanReceiver<SystemClock, SocketCanQueue>,
        TransferIdFixedMap<CanTransport, NUM_TRANSFER_IDS>,
        SocketCanQueue,
        NUM_PUBLISHERS,
        NUM_REQUESTERS,
    >,
>;

// ─── Worker ───────────────────────────────────────────────────────────────────

/// Start a SocketCAN transport worker and return its handle.
pub fn start(cfg: CanConfig, shutdown: watch::Receiver<bool>) -> TransportHandle {
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<TransportCommand>(16);

    let name = cfg.name.clone();
    let name_clone = name.clone();
    let local_node_id = u16::from(cfg.node_id);

    tokio::task::spawn_blocking(move || {
        run_worker(cfg, cmd_rx, shutdown);
    });

    TransportHandle {
        name: name_clone,
        local_node_id: Some(local_node_id),
        cmd_tx,
    }
}

fn run_worker(
    cfg: CanConfig,
    cmd_rx: mpsc::Receiver<TransportCommand>,
    shutdown: watch::Receiver<bool>,
) {
    let name = cfg.name.clone();

    let iface = match cfg.interface.as_deref() {
        Some(i) => i.to_string(),
        None => {
            error!(transport = %name, "SocketCAN transport requires 'interface' to be set");
            return;
        }
    };

    let socket = match CanSocket::open(&iface) {
        Ok(s) => s,
        Err(e) => {
            error!(transport = %name, "Failed to open SocketCAN interface '{iface}': {e}");
            return;
        }
    };
    if let Err(e) = socket.set_read_timeout(Duration::from_millis(1)) {
        warn!(transport = %name, "Could not set read timeout: {e}");
    }
    if let Err(e) = socket.set_write_timeout(Duration::from_millis(100)) {
        warn!(transport = %name, "Could not set write timeout: {e}");
    }
    let can = LinuxCan::new(socket);

    let node_id = match CanNodeId::try_from(cfg.node_id) {
        Ok(id) => id,
        Err(_) => {
            error!(transport = %name, "Invalid node ID {}", cfg.node_id);
            return;
        }
    };

    let queue_driver = SocketCanQueue::new(ArrayQueue::new(), can);
    let transmitter = CanTransmitter::new(Mtu::Can8);
    let receiver = CanReceiver::new(node_id);
    let core_node = CoreNode::new(
        SystemClock::new(),
        node_id,
        transmitter,
        receiver,
        queue_driver,
    );

    let node_info = crate::transport::make_node_info(&name);
    let mut node: SocketCanNode = match BasicNode::new(core_node, node_info) {
        Ok(n) => n,
        Err(e) => {
            error!(transport = %name, "Failed to create Cyphal node: {e:?}");
            return;
        }
    };

    if let Err(e) = node.subscribe_message(HEARTBEAT_SUBJECT, 7, milliseconds(2000)) {
        warn!(transport = %name, "Failed to subscribe to Heartbeat: {e:?}");
    }

    let get_info_token: Option<ServiceToken<GetInfoRequest>> = node
        .start_sending_requests::<GetInfoRequest>(
            GET_INFO_SERVICE,
            milliseconds(3000),
            432,
            Priority::Nominal,
        )
        .map_err(|e| warn!(transport = %name, "Failed to start GetInfo requester: {e:?}"))
        .ok();

    let mut state = WorkerState::default();
    state.register_local_node(u16::from(cfg.node_id));
    let mut last_second = Instant::now();

    // ─── PNP allocator ────────────────────────────────────────────────────────
    let mut pnp_allocator: Option<PnpAllocator> = if cfg.pnp_enabled {
        let db_path = effective_pnp_db_path(&name, cfg.pnp_db_path.as_deref());
        match crate::pnp::AllocationDb::open(&db_path) {
            Ok(db) => {
                if let Err(e) = node.subscribe_message(PNP_V2_SUBJECT, 18, milliseconds(5000)) {
                    warn!(transport = %name, "Failed to subscribe to PNP v2: {e:?}");
                }
                if let Err(e) = node.subscribe_message(PNP_V1_SUBJECT, 9, milliseconds(5000)) {
                    warn!(transport = %name, "Failed to subscribe to PNP v1: {e:?}");
                }
                if let Err(e) =
                    node.start_publishing(PNP_V2_SUBJECT, milliseconds(500), Priority::Nominal)
                {
                    warn!(transport = %name, "Failed to start PNP v2 publisher: {e:?}");
                }
                if let Err(e) =
                    node.start_publishing(PNP_V1_SUBJECT, milliseconds(500), Priority::Nominal)
                {
                    warn!(transport = %name, "Failed to start PNP v1 publisher: {e:?}");
                }
                info!(transport = %name, db = %db_path.display(), "PNP allocator started");
                Some(PnpAllocator::new(db, u16::from(cfg.node_id), 127))
            }
            Err(e) => {
                warn!(transport = %name, "Failed to open PNP DB {}: {e}", db_path.display());
                None
            }
        }
    } else {
        None
    };

    info!(transport = %name, "SocketCAN worker started on '{iface}'");

    loop {
        if shutdown.has_changed().unwrap_or(true) && *shutdown.borrow() {
            break;
        }

        while let Ok(cmd) = cmd_rx.try_recv() {
            handle_command(
                cmd,
                &mut node,
                &get_info_token,
                &mut state,
                &name,
                cfg.node_id,
                pnp_allocator.as_ref(),
            );
        }

        state.check_get_info_timeout();

        // Dispatch next PNP-triggered GetInfo if none is in flight.
        if state.pnp_pending_get_info.is_none()
            && let Some(pnp) = pnp_allocator.as_mut()
            && let Some(node_id) = pnp.pop_get_info_request()
        {
            if node_id <= 127 {
                if let (Some(token), Ok(target)) =
                    (&get_info_token, CanNodeId::try_from(node_id as u8))
                {
                    match node.send_request(token, &GetInfoRequest {}, target) {
                        Ok(_) => {
                            state.on_get_info_request_sent();
                            state.pnp_pending_get_info = Some(PnpGetInfoPending {
                                node_id,
                                deadline: Instant::now() + Duration::from_secs(5),
                            });
                            debug!(transport = %name, node_id, "PNP GetInfo request sent");
                        }
                        Err(e) => {
                            debug!(transport = %name, node_id, "PNP GetInfo send failed: {e:?}");
                            pnp.on_get_info_result(node_id, None);
                        }
                    }
                } else {
                    pnp.on_get_info_result(node_id, None);
                }
            } else {
                pnp.on_get_info_result(node_id, None);
            }
        }

        // Report PNP GetInfo timeouts back to the allocator.
        if let Some(timed_out) = state.check_pnp_get_info_timeout()
            && let Some(pnp) = pnp_allocator.as_mut()
        {
            pnp.on_get_info_result(timed_out, None);
        }

        let mut handler = SocketCanHandler {
            state: &mut state,
            pnp: pnp_allocator.as_mut(),
        };
        match node.receive(&mut handler) {
            Ok(_) => {}
            Err(CanError::Driver(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(CanError::Driver(e)) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                state.record_error(format!("Receive error: {e:?}"));
                warn!(transport = %name, "Receive error: {e:?}");
            }
        }

        if last_second.elapsed() >= Duration::from_secs(1) {
            last_second = Instant::now();
            state.tick_local_node(u16::from(cfg.node_id));
            if let Err(e) = node.run_per_second_tasks() {
                debug!(transport = %name, "Per-second task error: {e:?}");
            }
            if let Err(e) = node.flush() {
                debug!(transport = %name, "Flush error: {e:?}");
            }
        }
    }

    info!(transport = %name, "SocketCAN worker stopped");
}

fn handle_command(
    cmd: TransportCommand,
    node: &mut SocketCanNode,
    token: &Option<ServiceToken<GetInfoRequest>>,
    state: &mut WorkerState,
    _name: &str,
    local_node_id: u8,
    pnp: Option<&PnpAllocator>,
) {
    match cmd {
        TransportCommand::ListNodes { reply } => {
            let _ = reply.send(state.node_list());
        }
        TransportCommand::Diagnostics { reply } => {
            let mut diag = state.diagnostics(Some(u16::from(local_node_id)));
            diag.pnp_enabled = pnp.is_some();
            let _ = reply.send(diag);
        }
        TransportCommand::GetInfo { node_id, reply } => {
            if node_id > 127 {
                let _ = reply.send(Err(format!(
                    "CAN node ID {node_id} is out of range (0–127)"
                )));
                return;
            }
            let target = match CanNodeId::try_from(node_id as u8) {
                Ok(id) => id,
                Err(_) => {
                    let _ = reply.send(Err("Invalid node ID".into()));
                    return;
                }
            };
            if let Some(token) = token {
                match node.send_request(token, &GetInfoRequest {}, target) {
                    Ok(_) => {
                        state.on_get_info_request_sent();
                        state.pending_get_info = Some(PendingGetInfo {
                            node_id,
                            reply,
                            deadline: Instant::now() + Duration::from_secs(3),
                        });
                    }
                    Err(e) => {
                        state.record_error(format!("GetInfo request send failed: {e:?}"));
                        let _ = reply.send(Err(format!("Failed to send GetInfo request: {e:?}")));
                    }
                }
            } else {
                state.record_error("GetInfo requester not available");
                let _ = reply.send(Err("GetInfo requester not available".into()));
            }
        }
        TransportCommand::ListAllocations { reply } => {
            let entries = pnp.map(|a| a.list_allocations()).unwrap_or_default();
            let _ = reply.send(entries);
        }
    }
}

// ─── Transfer handler ─────────────────────────────────────────────────────────

struct SocketCanHandler<'a> {
    state: &'a mut WorkerState,
    pnp: Option<&'a mut PnpAllocator>,
}

impl<'a> TransferHandler<CanTransport> for SocketCanHandler<'a> {
    fn handle_message<N: Node<Transport = CanTransport>>(
        &mut self,
        node: &mut N,
        transfer: &MessageTransfer<Vec<u8>, CanTransport>,
    ) -> bool {
        let source = transfer.header.source.map(u16::from);

        if transfer.header.subject == HEARTBEAT_SUBJECT {
            let Ok(hb) = Heartbeat::deserialize_from_bytes(&transfer.payload) else {
                return false;
            };
            let node_id: u16 = source.unwrap_or(0xFFFF);
            if let Some(pnp) = &mut self.pnp {
                pnp.on_heartbeat(node_id);
            }
            self.state
                .on_heartbeat(node_id, hb.uptime, hb.health.value, hb.mode.value);
            return true;
        }

        if let Some(pnp) = &mut self.pnp {
            if transfer.header.subject == PNP_V2_SUBJECT {
                if let Ok(msg) = AllocationDataV2::deserialize_from_bytes(&transfer.payload)
                    && let Some(response) = pnp.on_allocation_v2(&msg, source)
                {
                    let _ = node.publish(PNP_V2_SUBJECT, &response);
                    self.state.on_pnp_allocated();
                }
                return true;
            }

            if transfer.header.subject == PNP_V1_SUBJECT {
                if let Ok(msg) = AllocationDataV1::deserialize_from_bytes(&transfer.payload)
                    && let Some(response) = pnp.on_allocation_v1(&msg, source)
                {
                    let _ = node.publish(PNP_V1_SUBJECT, &response);
                    self.state.on_pnp_allocated();
                }
                return true;
            }
        }

        false
    }

    fn handle_response<N: Node<Transport = CanTransport>>(
        &mut self,
        _node: &mut N,
        transfer: &ServiceTransfer<Vec<u8>, CanTransport>,
    ) -> bool {
        if transfer.header.service != GET_INFO_SERVICE {
            return false;
        }

        let source_node_id = u16::from(transfer.header.source);

        let matches_cli = self
            .state
            .pending_get_info
            .as_ref()
            .is_some_and(|p| p.node_id == source_node_id);
        let matches_pnp = self
            .state
            .pnp_pending_get_info
            .as_ref()
            .is_some_and(|p| p.node_id == source_node_id);

        if !matches_cli && !matches_pnp {
            return false;
        }

        let parsed = GetInfoResponse::deserialize_from_bytes(&transfer.payload);

        if matches_cli {
            let result = match &parsed {
                Ok(resp) => Ok(GetInfoResult {
                    node_id: source_node_id,
                    name: String::from_utf8_lossy(&resp.name).into_owned(),
                    hardware_version: (resp.hardware_version.major, resp.hardware_version.minor),
                    software_version: (resp.software_version.major, resp.software_version.minor),
                    unique_id: resp.unique_id,
                }),
                Err(_) => Err("Failed to parse GetInfo response".into()),
            };
            self.state.on_get_info_response(result);
        }

        if matches_pnp {
            self.state.pnp_pending_get_info = None;
            self.state.get_info_responses_received =
                self.state.get_info_responses_received.saturating_add(1);
            let uid = parsed.as_ref().map(|r| r.unique_id).ok();
            if let Some(pnp) = &mut self.pnp {
                pnp.on_get_info_result(source_node_id, uid);
            }
        }

        self.state.on_message_received();
        true
    }

    fn handle_request<N: Node<Transport = CanTransport>>(
        &mut self,
        _node: &mut N,
        _token: canadensis::ResponseToken<CanTransport>,
        _transfer: &ServiceTransfer<Vec<u8>, CanTransport>,
    ) -> bool {
        false
    }
}
