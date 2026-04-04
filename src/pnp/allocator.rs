//! PNP node-ID allocator state machine.
//!
//! Implements the single-allocator (non-redundant) PNP allocation algorithm
//! described in UAVCAN/Cyphal specification sections on plug-and-play nodes.
//!
//! Supports both `NodeIDAllocationData.1.0` (for low-MTU transports such as
//! Classic CAN) and `NodeIDAllocationData.2.0` (for high-MTU transports).
//!
//! # Cluster mode
//! This implementation is non-redundant (single allocator).  The architecture
//! does not preclude adding cluster/Raft support in the future: incoming
//! messages from other allocators (identified by a non-anonymous source) are
//! simply ignored here, leaving room for a future cluster implementation.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use canadensis_data_types::uavcan::node::id_1_0::ID as NodeId;
use canadensis_data_types::uavcan::pnp::node_id_allocation_data_1_0::NodeIDAllocationData as AllocationDataV1;
use canadensis_data_types::uavcan::pnp::node_id_allocation_data_2_0::NodeIDAllocationData as AllocationDataV2;
use tracing::{debug, info, warn};

use crate::pnp::db::{AllocationDb, AllocationEntry};

/// Minimum time that must elapse between successive GetInfo enqueues for the
/// same node. Prevents re-queuing a node that simply does not implement GetInfo.
const GET_INFO_RETRY_COOLDOWN: Duration = Duration::from_secs(60);

/// Single-allocator PNP node-ID assignment service.
pub struct PnpAllocator {
    db: AllocationDb,
    local_node_id: u16,
    max_node_id: u16,
    /// Queue of node IDs that need a GetInfo service request to retrieve their
    /// full 128-bit unique ID.  Drained by the transport worker.
    get_info_queue: VecDeque<u16>,
    /// Tracks when a GetInfo was last enqueued for a given node ID, to prevent
    /// retrying too frequently for nodes that do not implement GetInfo.
    recently_queried: HashMap<u16, Instant>,
}

impl PnpAllocator {
    /// Create a new allocator.
    ///
    /// * `db` – the opened allocation database.
    /// * `local_node_id` – the node ID of the allocator itself (reserved, never assigned).
    /// * `max_node_id` – highest assignable node ID (127 for CAN, 65534 for UDP/Serial).
    pub fn new(db: AllocationDb, local_node_id: u16, max_node_id: u16) -> Self {
        if let Err(e) = db.record_static_node(local_node_id) {
            warn!(
                local_node_id,
                "PNP: failed to reserve local node in allocation DB: {e}"
            );
        }
        PnpAllocator {
            db,
            local_node_id,
            max_node_id,
            get_info_queue: VecDeque::new(),
            recently_queried: HashMap::new(),
        }
    }

    /// Process a v2 (`NodeIDAllocationData.2.0`) allocation message.
    ///
    /// Returns `Some(response)` when an allocation was performed. Returns `None`
    /// for allocator-to-allocator messages or when allocation fails.
    pub fn on_allocation_v2(
        &mut self,
        msg: &AllocationDataV2,
        source: Option<u16>,
    ) -> Option<AllocationDataV2> {
        if source.is_some() {
            return None;
        }

        let uid = &msg.unique_id;

        let node_id = if let Some(existing) = self.db.find_by_unique_id(uid) {
            debug!(node_id = existing, "PNP v2: re-using existing allocation");
            existing
        } else {
            let new_id = self
                .db
                .next_free_id(&[self.local_node_id], self.max_node_id)?;
            if let Err(e) = self.db.insert_v2(new_id, uid) {
                warn!(node_id = new_id, "PNP v2: DB insert failed: {e}");
                return None;
            }
            info!(node_id = new_id, "PNP v2: allocated node ID");
            new_id
        };

        Some(AllocationDataV2 {
            node_id: NodeId { value: node_id },
            unique_id: *uid,
        })
    }

    /// Process a v1 (`NodeIDAllocationData.1.0`) allocation message.
    ///
    /// Returns `Some(response)` when an allocation was performed.
    pub fn on_allocation_v1(
        &mut self,
        msg: &AllocationDataV1,
        source: Option<u16>,
    ) -> Option<AllocationDataV1> {
        if source.is_some() {
            return None;
        }

        if !msg.allocated_node_id.is_empty() {
            return None;
        }

        let hash = msg.unique_id_hash;

        let node_id = if let Some(existing) = self.db.find_by_hash(hash) {
            debug!(node_id = existing, "PNP v1: re-using existing allocation");
            existing
        } else {
            let new_id = self
                .db
                .next_free_id(&[self.local_node_id], self.max_node_id)?;
            if let Err(e) = self.db.insert_v1(new_id, hash) {
                warn!(node_id = new_id, "PNP v1: DB insert failed: {e}");
                return None;
            }
            info!(node_id = new_id, hash, "PNP v1: allocated node ID");
            new_id
        };

        let mut allocated_node_id = heapless::Vec::new();
        let _ = allocated_node_id.push(NodeId { value: node_id });

        Some(AllocationDataV1 {
            unique_id_hash: hash,
            allocated_node_id,
        })
    }

    /// Notify the allocator that a node has been observed via heartbeat.
    ///
    /// Records the node as a static node if not already in the allocation table,
    /// preventing its ID from being assigned to a PNP node.
    ///
    /// If the stored unique ID is still a placeholder (all-zeros for a static
    /// node, or a 48-bit hash for a v1 PNP node), this method enqueues a
    /// GetInfo request so the transport worker can retrieve the real 128-bit
    /// unique ID.  Re-enqueueing is rate-limited by `GET_INFO_RETRY_COOLDOWN`.
    pub fn on_heartbeat(&mut self, node_id: u16) {
        if let Err(e) = self.db.record_static_node(node_id) {
            warn!(node_id, "PNP: failed to record static node: {e}");
        }

        // Enqueue a GetInfo for static and v1 nodes whose unique ID is still a
        // placeholder, unless we queried this node recently.
        let needs_query = self
            .db
            .find_by_node_id(node_id)
            .is_some_and(|e| e.needs_get_info());

        if needs_query {
            let cooldown_elapsed = self
                .recently_queried
                .get(&node_id)
                .is_none_or(|t| t.elapsed() >= GET_INFO_RETRY_COOLDOWN);

            if cooldown_elapsed {
                debug!(node_id, "PNP: queuing GetInfo for node");
                self.get_info_queue.push_back(node_id);
                self.recently_queried.insert(node_id, Instant::now());
            }
        }
    }

    /// Pop the next node ID that needs a GetInfo service request, if any.
    ///
    /// The transport worker calls this once per loop iteration when no
    /// PNP-triggered GetInfo is already in flight.
    pub fn pop_get_info_request(&mut self) -> Option<u16> {
        self.get_info_queue.pop_front()
    }

    /// Report the result of a PNP-triggered GetInfo request.
    ///
    /// On success (`uid = Some(…)`) the full unique ID is persisted in the
    /// allocation database. On failure / timeout (`uid = None`) the
    /// `recently_queried` entry is cleared immediately so the node can be
    /// retried on its next heartbeat rather than waiting the full cooldown.
    pub fn on_get_info_result(&mut self, node_id: u16, uid: Option<[u8; 16]>) {
        match uid {
            Some(uid) => {
                if let Err(e) = self.db.update_unique_id(node_id, &uid) {
                    warn!(node_id, "PNP: failed to update unique ID: {e}");
                } else {
                    info!(node_id, "PNP: unique ID updated from GetInfo response");
                }
            }
            None => {
                // Remove the cooldown so the next heartbeat can retry.
                self.recently_queried.remove(&node_id);
                debug!(
                    node_id,
                    "PNP: GetInfo timed out, will retry on next heartbeat"
                );
            }
        }
    }

    /// Return all entries in the allocation table.
    pub fn list_allocations(&self) -> Vec<AllocationEntry> {
        self.db.list_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pnp::db::AllocationDb;

    fn temp_allocator(local_id: u16, max_id: u16) -> (PnpAllocator, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "pnp_alloc_test_{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let db = AllocationDb::open(&path).expect("open temp db");
        let alloc = PnpAllocator::new(db, local_id, max_id);
        (alloc, path)
    }

    fn make_v2_request(uid: [u8; 16]) -> AllocationDataV2 {
        AllocationDataV2 {
            node_id: NodeId { value: 0 },
            unique_id: uid,
        }
    }

    fn make_v1_request(hash: u64) -> AllocationDataV1 {
        AllocationDataV1 {
            unique_id_hash: hash,
            allocated_node_id: heapless::Vec::new(),
        }
    }

    #[test]
    fn test_v2_first_allocation() {
        let (mut alloc, path) = temp_allocator(100, 127);
        let uid: [u8; 16] = [0xAA; 16];
        let resp = alloc.on_allocation_v2(&make_v2_request(uid), None).unwrap();
        assert_eq!(resp.unique_id, uid);
        let id = resp.node_id.value;
        assert!((1..=127).contains(&id));
        assert_ne!(id, 100);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_v2_idempotent_same_uid() {
        let (mut alloc, path) = temp_allocator(100, 127);
        let uid: [u8; 16] = [0xBB; 16];
        let resp1 = alloc.on_allocation_v2(&make_v2_request(uid), None).unwrap();
        let resp2 = alloc.on_allocation_v2(&make_v2_request(uid), None).unwrap();
        let id1 = resp1.node_id.value;
        let id2 = resp2.node_id.value;
        assert_eq!(id1, id2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_v2_different_uids_get_different_ids() {
        let (mut alloc, path) = temp_allocator(100, 127);
        let uid1: [u8; 16] = [0x01; 16];
        let uid2: [u8; 16] = [0x02; 16];
        let resp1 = alloc
            .on_allocation_v2(&make_v2_request(uid1), None)
            .unwrap();
        let resp2 = alloc
            .on_allocation_v2(&make_v2_request(uid2), None)
            .unwrap();
        let id1 = resp1.node_id.value;
        let id2 = resp2.node_id.value;
        assert_ne!(id1, id2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_v2_ignores_non_anonymous_source() {
        let (mut alloc, path) = temp_allocator(100, 127);
        let uid: [u8; 16] = [0xCC; 16];
        let resp = alloc.on_allocation_v2(&make_v2_request(uid), Some(42));
        assert!(resp.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_v2_local_id_not_assigned() {
        let (mut alloc, path) = temp_allocator(1, 5);
        let uid: [u8; 16] = [0x11; 16];
        let resp = alloc.on_allocation_v2(&make_v2_request(uid), None).unwrap();
        let id = resp.node_id.value;
        assert_ne!(id, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_v2_table_full_returns_none() {
        let (mut alloc, path) = temp_allocator(1, 2);
        let uid1: [u8; 16] = [0x01; 16];
        let uid2: [u8; 16] = [0x02; 16];
        let resp1 = alloc.on_allocation_v2(&make_v2_request(uid1), None);
        assert!(resp1.is_some());
        let resp2 = alloc.on_allocation_v2(&make_v2_request(uid2), None);
        assert!(resp2.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_v1_first_allocation() {
        let (mut alloc, path) = temp_allocator(100, 127);
        let hash: u64 = 0xABCDEF123456;
        let resp = alloc
            .on_allocation_v1(&make_v1_request(hash), None)
            .unwrap();
        assert_eq!(resp.unique_id_hash, hash);
        assert_eq!(resp.allocated_node_id.len(), 1);
        let id = resp.allocated_node_id[0].value;
        assert!((1..=127).contains(&id));
        assert_ne!(id, 100);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_v1_idempotent_same_hash() {
        let (mut alloc, path) = temp_allocator(100, 127);
        let hash: u64 = 0x1234567890AB;
        let resp1 = alloc
            .on_allocation_v1(&make_v1_request(hash), None)
            .unwrap();
        let resp2 = alloc
            .on_allocation_v1(&make_v1_request(hash), None)
            .unwrap();
        let id1 = resp1.allocated_node_id[0].value;
        let id2 = resp2.allocated_node_id[0].value;
        assert_eq!(id1, id2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_v1_ignores_non_anonymous_source() {
        let (mut alloc, path) = temp_allocator(100, 127);
        let hash: u64 = 0xDEADBEEFCAFE;
        let resp = alloc.on_allocation_v1(&make_v1_request(hash), Some(5));
        assert!(resp.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_v1_ignores_response_messages() {
        let (mut alloc, path) = temp_allocator(100, 127);
        let mut msg = make_v1_request(0x123456789ABC);
        let _ = msg.allocated_node_id.push(NodeId { value: 7 });
        let resp = alloc.on_allocation_v1(&msg, None);
        assert!(resp.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_heartbeat_reserves_node_id() {
        let (mut alloc, path) = temp_allocator(100, 3);
        alloc.on_heartbeat(1);
        alloc.on_heartbeat(2);
        let uid: [u8; 16] = [0xFF; 16];
        let resp = alloc.on_allocation_v2(&make_v2_request(uid), None).unwrap();
        let id = resp.node_id.value;
        assert_eq!(id, 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_list_allocations() {
        let (mut alloc, path) = temp_allocator(100, 127);
        let uid: [u8; 16] = [0x42; 16];
        alloc.on_allocation_v2(&make_v2_request(uid), None);
        let entries = alloc.list_allocations();
        assert!(entries.len() >= 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_heartbeat_enqueues_get_info_for_static_node() {
        let (mut alloc, path) = temp_allocator(100, 127);
        alloc.on_heartbeat(5);
        assert_eq!(
            alloc.pop_get_info_request(),
            Some(5),
            "static node should be enqueued for GetInfo"
        );
        assert!(
            alloc.pop_get_info_request().is_none(),
            "queue should be empty after pop"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_heartbeat_enqueues_get_info_for_v1_node() {
        let (mut alloc, path) = temp_allocator(100, 127);
        alloc
            .on_allocation_v1(&make_v1_request(0xDEAD_BEEF_CAFE), None)
            .unwrap();
        let entries = alloc.list_allocations();
        let v1_entry = entries
            .iter()
            .find(|e| e.pseudo_unique_id.is_some())
            .unwrap();
        let v1_node_id = v1_entry.node_id;

        alloc.on_heartbeat(v1_node_id);
        assert_eq!(
            alloc.pop_get_info_request(),
            Some(v1_node_id),
            "v1 node should be enqueued for GetInfo after heartbeat"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_heartbeat_cooldown_prevents_duplicate_enqueue() {
        let (mut alloc, path) = temp_allocator(100, 127);
        alloc.on_heartbeat(5);
        alloc.pop_get_info_request();

        alloc.on_heartbeat(5);
        assert!(
            alloc.pop_get_info_request().is_none(),
            "within cooldown, node must not be re-enqueued"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_on_get_info_result_success_stops_reenqueue() {
        let (mut alloc, path) = temp_allocator(100, 127);
        alloc.on_heartbeat(5);
        alloc.pop_get_info_request();

        let uid: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10,
        ];
        alloc.on_get_info_result(5, Some(uid));

        let entries = alloc.list_allocations();
        let entry = entries.iter().find(|e| e.node_id == 5).unwrap();
        assert!(
            !entry.needs_get_info(),
            "node should no longer need GetInfo"
        );

        alloc.on_heartbeat(5);
        assert!(
            alloc.pop_get_info_request().is_none(),
            "node with real unique ID must not be re-enqueued"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_on_get_info_result_timeout_allows_retry() {
        let (mut alloc, path) = temp_allocator(100, 127);
        alloc.on_heartbeat(5);
        alloc.pop_get_info_request();

        alloc.on_get_info_result(5, None);

        alloc.on_heartbeat(5);
        assert_eq!(
            alloc.pop_get_info_request(),
            Some(5),
            "after timeout, next heartbeat must re-enqueue"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_v2_node_not_enqueued_for_get_info() {
        let (mut alloc, path) = temp_allocator(100, 127);
        let uid: [u8; 16] = [0xAA; 16];
        let resp = alloc.on_allocation_v2(&make_v2_request(uid), None).unwrap();
        let assigned_id = resp.node_id.value;

        alloc.on_heartbeat(assigned_id);
        assert!(
            alloc.pop_get_info_request().is_none(),
            "v2 node already has a real unique ID and must not be enqueued"
        );
        let _ = std::fs::remove_file(&path);
    }
}
