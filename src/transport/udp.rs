//! Cyphal/UDP transport.

use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use canadensis::core::transfer::{MessageTransfer, ServiceTransfer};
use canadensis::node::{BasicNode, CoreNode};
use canadensis::requester::TransferIdFixedMap;
use canadensis::{Node, ServiceToken, TransferHandler};
use canadensis_core::Priority;
use canadensis_core::session::SessionDynamicMap;
use canadensis_core::time::milliseconds;
use canadensis_data_types::uavcan::node::get_info_1_0::{
    GetInfoRequest, GetInfoResponse, SERVICE as GET_INFO_SERVICE,
};
use canadensis_data_types::uavcan::node::heartbeat_1_0::{Heartbeat, SUBJECT as HEARTBEAT_SUBJECT};
use canadensis_encoding::Deserialize;
use canadensis_udp::driver::StdUdpSocket;
use canadensis_udp::{
    DEFAULT_PORT, UdpNodeId, UdpReceiver, UdpSessionData, UdpTransferId, UdpTransmitter,
    UdpTransport,
};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::config::UdpConfig;
use crate::transport::{
    GetInfoResult, PendingGetInfo, SystemClock, TransportCommand, TransportHandle, WorkerState,
};

// ─── Canadensis node type aliases ─────────────────────────────────────────────

const MTU: usize = 1200;
const NUM_PUBLISHERS: usize = 4;
const NUM_REQUESTERS: usize = 4;
const NUM_TRANSFER_IDS: usize = 4;

type UdpNode = BasicNode<
    CoreNode<
        SystemClock,
        UdpTransmitter<StdUdpSocket, MTU>,
        UdpReceiver<
            SystemClock,
            SessionDynamicMap<UdpNodeId, UdpTransferId, UdpSessionData>,
            StdUdpSocket,
            MTU,
        >,
        TransferIdFixedMap<UdpTransport, NUM_TRANSFER_IDS>,
        StdUdpSocket,
        NUM_PUBLISHERS,
        NUM_REQUESTERS,
    >,
>;

// ─── Worker ───────────────────────────────────────────────────────────────────

/// Start a Cyphal/UDP transport worker and return its handle.
pub fn start(cfg: UdpConfig, shutdown: watch::Receiver<bool>) -> TransportHandle {
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<TransportCommand>(16);

    let name = cfg.name.clone();
    let name_clone = name.clone();

    tokio::task::spawn_blocking(move || {
        run_worker(cfg, cmd_rx, shutdown);
    });

    TransportHandle {
        name: name_clone,
        cmd_tx,
    }
}

fn run_worker(
    cfg: UdpConfig,
    cmd_rx: mpsc::Receiver<TransportCommand>,
    shutdown: watch::Receiver<bool>,
) {
    let name = cfg.name.clone();

    let iface: Ipv4Addr = match Ipv4Addr::from_str(&cfg.interface) {
        Ok(a) => a,
        Err(e) => {
            error!(transport = %name, "Invalid interface address '{}': {e}", cfg.interface);
            return;
        }
    };

    let port = cfg.port.unwrap_or(DEFAULT_PORT);

    let node_id: UdpNodeId = match UdpNodeId::try_from(cfg.node_id) {
        Ok(id) => id,
        Err(_) => {
            error!(transport = %name, "Invalid UDP node ID {}", cfg.node_id);
            return;
        }
    };

    let socket = match StdUdpSocket::bind(Ipv4Addr::UNSPECIFIED, port) {
        Ok(s) => s,
        Err(e) => {
            error!(transport = %name, "Failed to bind UDP socket on port {port}: {e:?}");
            return;
        }
    };

    let transmitter = UdpTransmitter::<StdUdpSocket, MTU>::new(port);
    let receiver = UdpReceiver::new(Some(node_id), iface);
    let core_node = CoreNode::<
        SystemClock,
        UdpTransmitter<StdUdpSocket, MTU>,
        UdpReceiver<
            SystemClock,
            SessionDynamicMap<UdpNodeId, UdpTransferId, UdpSessionData>,
            StdUdpSocket,
            MTU,
        >,
        TransferIdFixedMap<UdpTransport, NUM_TRANSFER_IDS>,
        StdUdpSocket,
        NUM_PUBLISHERS,
        NUM_REQUESTERS,
    >::new(SystemClock::new(), node_id, transmitter, receiver, socket);

    let node_info = crate::transport::make_node_info(&name);
    let mut node: UdpNode = match BasicNode::new(core_node, node_info) {
        Ok(n) => n,
        Err(e) => {
            error!(transport = %name, "Failed to create Cyphal UDP node: {e:?}");
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
    let mut last_second = Instant::now();

    info!(transport = %name, "UDP worker started on {iface}:{port}");

    loop {
        if shutdown.has_changed().unwrap_or(true) && *shutdown.borrow() {
            break;
        }

        while let Ok(cmd) = cmd_rx.try_recv() {
            handle_command(cmd, &mut node, &get_info_token, &mut state, &name);
        }

        state.check_get_info_timeout();

        let mut handler = UdpHandler { state: &mut state };
        match node.receive(&mut handler) {
            Ok(_) => {}
            Err(e) => debug!(transport = %name, "Receive: {e:?}"),
        }

        if last_second.elapsed() >= Duration::from_secs(1) {
            last_second = Instant::now();
            if let Err(e) = node.run_per_second_tasks() {
                debug!(transport = %name, "Per-second task error: {e:?}");
            }
            if let Err(e) = node.flush() {
                debug!(transport = %name, "Flush error: {e:?}");
            }
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    info!(transport = %name, "UDP worker stopped");
}

fn handle_command(
    cmd: TransportCommand,
    node: &mut UdpNode,
    token: &Option<ServiceToken<GetInfoRequest>>,
    state: &mut WorkerState,
    _name: &str,
) {
    match cmd {
        TransportCommand::ListNodes { reply } => {
            let _ = reply.send(state.node_list());
        }
        TransportCommand::GetInfo { node_id, reply } => {
            let target = match UdpNodeId::try_from(node_id) {
                Ok(id) => id,
                Err(_) => {
                    let _ = reply.send(Err(format!("Invalid UDP node ID: {node_id}")));
                    return;
                }
            };
            if let Some(token) = token {
                match node.send_request(token, &GetInfoRequest {}, target) {
                    Ok(_) => {
                        state.pending_get_info = Some(PendingGetInfo {
                            node_id,
                            reply,
                            deadline: Instant::now() + Duration::from_secs(3),
                        });
                    }
                    Err(e) => {
                        let _ = reply.send(Err(format!("Failed to send GetInfo request: {e:?}")));
                    }
                }
            } else {
                let _ = reply.send(Err("GetInfo requester not available".into()));
            }
        }
    }
}

// ─── Transfer handler ─────────────────────────────────────────────────────────

struct UdpHandler<'a> {
    state: &'a mut WorkerState,
}

impl<'a> TransferHandler<UdpTransport> for UdpHandler<'a> {
    fn handle_message<N: Node<Transport = UdpTransport>>(
        &mut self,
        _node: &mut N,
        transfer: &MessageTransfer<Vec<u8>, UdpTransport>,
    ) -> bool {
        if transfer.header.subject != HEARTBEAT_SUBJECT {
            return false;
        }
        let Ok(hb) = Heartbeat::deserialize_from_bytes(&transfer.payload) else {
            return false;
        };
        let node_id: u16 = transfer.header.source.map(u16::from).unwrap_or(0xFFFF);
        self.state
            .on_heartbeat(node_id, hb.uptime, hb.health.value, hb.mode.value);
        true
    }

    fn handle_response<N: Node<Transport = UdpTransport>>(
        &mut self,
        _node: &mut N,
        transfer: &ServiceTransfer<Vec<u8>, UdpTransport>,
    ) -> bool {
        if transfer.header.service != GET_INFO_SERVICE {
            return false;
        }
        let Some(ref pending) = self.state.pending_get_info else {
            return false;
        };
        let expected_node_id = pending.node_id;
        let result = match GetInfoResponse::deserialize_from_bytes(&transfer.payload) {
            Ok(resp) => Ok(GetInfoResult {
                node_id: expected_node_id,
                name: String::from_utf8_lossy(&resp.name).into_owned(),
                hardware_version: (resp.hardware_version.major, resp.hardware_version.minor),
                software_version: (resp.software_version.major, resp.software_version.minor),
                unique_id: resp.unique_id,
            }),
            Err(_) => Err("Failed to parse GetInfo response".into()),
        };
        self.state.on_get_info_response(result);
        true
    }

    fn handle_request<N: Node<Transport = UdpTransport>>(
        &mut self,
        _node: &mut N,
        _token: canadensis::ResponseToken<UdpTransport>,
        _transfer: &ServiceTransfer<Vec<u8>, UdpTransport>,
    ) -> bool {
        false
    }
}
