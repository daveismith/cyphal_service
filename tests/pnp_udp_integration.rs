//! Integration tests for PNP node-ID allocation.
//!
//! These tests exercise the full allocation flow through `PnpAllocator` and
//! `AllocationDb` together, covering both v1 and v2 allocation protocols.
//! They are written as integration tests (in the `tests/` directory) so that
//! the public API surface is validated just as external callers would use it.

use canadensis_data_types::uavcan::node::id_1_0::ID as NodeId;
use canadensis_data_types::uavcan::pnp::node_id_allocation_data_1_0::NodeIDAllocationData as AllocationDataV1;
use canadensis_data_types::uavcan::pnp::node_id_allocation_data_2_0::NodeIDAllocationData as AllocationDataV2;
use cyphal_service::pnp::{AllocationDb, AllocationEntry, PnpAllocator};
use std::path::PathBuf;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn temp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pnp_integ_{}_{}.db",
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
    ))
}

fn open_allocator(tag: &str, local_id: u16, max_id: u16) -> (PnpAllocator, PathBuf) {
    let path = temp_path(tag);
    let db = AllocationDb::open(&path).expect("open integration test DB");
    let alloc = PnpAllocator::new(db, local_id, max_id);
    (alloc, path)
}

fn v2_request(uid: [u8; 16]) -> AllocationDataV2 {
    AllocationDataV2 {
        node_id: NodeId { value: 0 },
        unique_id: uid,
    }
}

fn v1_request(hash: u64) -> AllocationDataV1 {
    AllocationDataV1 {
        unique_id_hash: hash,
        allocated_node_id: heapless::Vec::new(),
    }
}

/// Extract the node_id value from a v2 allocation response.
///
/// `AllocationDataV2` is `#[repr(C, packed)]`; its `node_id.value` field must
/// be read via `read_unaligned` to avoid creating a misaligned reference.
fn v2_node_id(resp: &AllocationDataV2) -> u16 {
    // SAFETY: addr_of! avoids creating a reference; read_unaligned handles
    //         the 1-byte alignment of the packed struct field.
    unsafe { std::ptr::addr_of!(resp.node_id.value).read_unaligned() }
}

/// Extract the allocated node_id from a v1 allocation response.
///
/// `ID` inside the heapless Vec is `#[repr(C, packed)]`; the field is accessed
/// via a raw pointer to avoid creating a misaligned reference.
fn v1_allocated_id(resp: &AllocationDataV1) -> u16 {
    // SAFETY: The heapless::Vec element is valid; read_unaligned handles
    //         the alignment of the packed ID struct.
    unsafe { std::ptr::addr_of!(resp.allocated_node_id[0].value).read_unaligned() }
}

// ─── v2 allocation flow ───────────────────────────────────────────────────────

#[test]
fn test_v2_allocation_assigns_valid_id() {
    let (mut alloc, path) = open_allocator("v2_valid", 100, 127);
    let uid: [u8; 16] = [0xDE, 0xAD, 0xBE, 0xEF, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

    let resp = alloc.on_allocation_v2(&v2_request(uid), None).unwrap();

    assert_eq!(resp.unique_id, uid, "response must echo back the unique ID");
    let id = v2_node_id(&resp);
    assert!(
        (1..=127).contains(&id),
        "allocated ID must be in range 1..=127"
    );
    assert_ne!(id, 100, "local node ID must not be assigned");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_v2_repeated_request_returns_same_id() {
    let (mut alloc, path) = open_allocator("v2_repeat", 100, 127);
    let uid: [u8; 16] = [0x11; 16];

    let first = alloc.on_allocation_v2(&v2_request(uid), None).unwrap();
    let second = alloc.on_allocation_v2(&v2_request(uid), None).unwrap();
    let third = alloc.on_allocation_v2(&v2_request(uid), None).unwrap();

    let id1 = v2_node_id(&first);
    let id2 = v2_node_id(&second);
    let id3 = v2_node_id(&third);

    assert_eq!(id1, id2, "repeated request must return the same ID");
    assert_eq!(id1, id3, "third request must also return the same ID");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_v2_different_nodes_get_different_ids() {
    let (mut alloc, path) = open_allocator("v2_different", 100, 127);

    let uids: [[u8; 16]; 5] = [[0x01; 16], [0x02; 16], [0x03; 16], [0x04; 16], [0x05; 16]];

    let mut assigned: Vec<u16> = Vec::new();
    for uid in &uids {
        let resp = alloc.on_allocation_v2(&v2_request(*uid), None).unwrap();
        let id = v2_node_id(&resp);
        assert!(!assigned.contains(&id), "duplicate node ID {id} assigned");
        assigned.push(id);
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_v2_db_persists_allocation() {
    let path = temp_path("v2_persist");
    let uid: [u8; 16] = [0xAB; 16];
    let assigned_id;

    // First allocator instance
    {
        let db = AllocationDb::open(&path).unwrap();
        let mut alloc = PnpAllocator::new(db, 100, 127);
        let resp = alloc.on_allocation_v2(&v2_request(uid), None).unwrap();
        assigned_id = v2_node_id(&resp);
    }

    // Second allocator instance on the same DB file
    {
        let db = AllocationDb::open(&path).unwrap();
        let mut alloc = PnpAllocator::new(db, 100, 127);
        let resp = alloc.on_allocation_v2(&v2_request(uid), None).unwrap();
        let id = v2_node_id(&resp);
        assert_eq!(id, assigned_id, "allocation must persist across DB reopens");
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_v2_ignores_non_anonymous_source() {
    let (mut alloc, path) = open_allocator("v2_nonano", 100, 127);
    let uid: [u8; 16] = [0xCC; 16];

    // source = Some(42) simulates a message from another allocator
    let resp = alloc.on_allocation_v2(&v2_request(uid), Some(42));
    assert!(
        resp.is_none(),
        "non-anonymous source must be ignored (cluster isolation)"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_v2_table_full_returns_none() {
    // local = 1, max = 2 → only ID 2 available
    let (mut alloc, path) = open_allocator("v2_full", 1, 2);

    let resp1 = alloc.on_allocation_v2(&v2_request([0x01; 16]), None);
    assert!(resp1.is_some(), "first allocation should succeed");

    let resp2 = alloc.on_allocation_v2(&v2_request([0x02; 16]), None);
    assert!(resp2.is_none(), "table full – no more IDs available");

    let _ = std::fs::remove_file(&path);
}

// ─── v1 allocation flow ───────────────────────────────────────────────────────

#[test]
fn test_v1_allocation_assigns_valid_id() {
    let (mut alloc, path) = open_allocator("v1_valid", 100, 127);
    let hash: u64 = 0x00ABCDEF012345;

    let resp = alloc.on_allocation_v1(&v1_request(hash), None).unwrap();

    assert_eq!(resp.unique_id_hash, hash, "response must echo the hash");
    assert_eq!(
        resp.allocated_node_id.len(),
        1,
        "response must contain one node ID"
    );
    let id = v1_allocated_id(&resp);
    assert!(
        (1..=127).contains(&id),
        "allocated ID must be in range 1..=127"
    );
    assert_ne!(id, 100, "local node ID must not be assigned");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_v1_repeated_hash_returns_same_id() {
    let (mut alloc, path) = open_allocator("v1_repeat", 100, 127);
    let hash: u64 = 0x1234_5678_9ABC;

    let first = alloc.on_allocation_v1(&v1_request(hash), None).unwrap();
    let second = alloc.on_allocation_v1(&v1_request(hash), None).unwrap();

    let id1 = v1_allocated_id(&first);
    let id2 = v1_allocated_id(&second);
    assert_eq!(id1, id2, "repeated v1 request must return the same node ID");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_v1_ignores_response_messages() {
    let (mut alloc, path) = open_allocator("v1_resp", 100, 127);

    let mut msg = v1_request(0xDEAD_BEEF_CAFE);
    // populated allocated_node_id signals a response from another allocator
    let _ = msg.allocated_node_id.push(NodeId { value: 7 });

    let resp = alloc.on_allocation_v1(&msg, None);
    assert!(resp.is_none(), "allocation responses must be ignored");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_v1_db_hex_format() {
    let (mut alloc, path) = open_allocator("v1_hex", 100, 127);
    let hash: u64 = 0x0000_AABBCCDDEEFF;

    alloc.on_allocation_v1(&v1_request(hash), None).unwrap();

    let entries: Vec<AllocationEntry> = alloc.list_allocations();
    let entry = entries
        .iter()
        .find(|e| e.pseudo_unique_id.is_some())
        .unwrap();

    // The hex must be 20 zeros + 12 hex chars for the 48-bit hash
    assert_eq!(
        entry.unique_id_hex, "00000000000000000000aabbccddeeff",
        "v1 unique_id_hex must be left-zero-padded hash"
    );
    assert_eq!(entry.pseudo_unique_id, Some(0x0000_AABBCCDDEEFF_u64 as i64));

    let _ = std::fs::remove_file(&path);
}

// ─── mixed v1 + v2 ───────────────────────────────────────────────────────────

#[test]
fn test_v1_and_v2_share_id_space() {
    let (mut alloc, path) = open_allocator("mixed", 100, 5);

    // Allocate two v2 nodes
    let uid1: [u8; 16] = [0x01; 16];
    let uid2: [u8; 16] = [0x02; 16];
    let r1 = alloc.on_allocation_v2(&v2_request(uid1), None).unwrap();
    let r2 = alloc.on_allocation_v2(&v2_request(uid2), None).unwrap();

    // Allocate a v1 node
    let hash: u64 = 0xCCDD_EEFF_1122;
    let r3 = alloc.on_allocation_v1(&v1_request(hash), None).unwrap();

    let id1 = v2_node_id(&r1);
    let id2 = v2_node_id(&r2);
    let id3 = v1_allocated_id(&r3);

    assert_ne!(id1, id2, "v2 nodes must get different IDs");
    assert_ne!(id1, id3, "v2 and v1 nodes must not share IDs");
    assert_ne!(id2, id3, "v2 and v1 nodes must not share IDs");

    let _ = std::fs::remove_file(&path);
}

// ─── static node tracking ────────────────────────────────────────────────────

#[test]
fn test_heartbeat_reserves_ids() {
    // max = 4, local = 1 → IDs 2, 3, 4 are candidates
    // Observe 2 and 3 via heartbeat → only 4 should be assigned
    let (mut alloc, path) = open_allocator("hb_reserve", 1, 4);

    alloc.on_heartbeat(2);
    alloc.on_heartbeat(3);

    let uid: [u8; 16] = [0xEE; 16];
    let resp = alloc.on_allocation_v2(&v2_request(uid), None).unwrap();
    let id = v2_node_id(&resp);

    assert_eq!(id, 4, "only ID 4 should remain after heartbeat reservation");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_heartbeat_does_not_overwrite_pnp_allocation() {
    let (mut alloc, path) = open_allocator("hb_no_overwrite", 100, 127);

    // Allocate node via v2 PNP
    let uid: [u8; 16] = [0x01; 16];
    let resp = alloc.on_allocation_v2(&v2_request(uid), None).unwrap();
    let assigned = v2_node_id(&resp);

    // Then observe the same node ID via heartbeat
    alloc.on_heartbeat(assigned);

    // The entry must still have the real unique_id, not the zero-padded static marker
    let entries = alloc.list_allocations();
    let entry = entries.iter().find(|e| e.node_id == assigned).unwrap();
    let expected_hex: String = uid.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        entry.unique_id_hex, expected_hex,
        "heartbeat must not overwrite an existing PNP allocation"
    );

    let _ = std::fs::remove_file(&path);
}

// ─── list_allocations ────────────────────────────────────────────────────────

#[test]
fn test_list_allocations_contains_all_entries() {
    let (mut alloc, path) = open_allocator("list", 100, 127);

    alloc.on_allocation_v2(&v2_request([0x01; 16]), None);
    alloc.on_allocation_v2(&v2_request([0x02; 16]), None);
    alloc.on_allocation_v1(&v1_request(0xABCD_EF12_3456), None);

    let entries = alloc.list_allocations();
    // local node (100) + 3 allocations
    assert!(
        entries.len() >= 4,
        "expected at least 4 entries, got {}",
        entries.len()
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_list_allocations_ordered_by_node_id() {
    let (mut alloc, path) = open_allocator("list_order", 100, 127);

    alloc.on_allocation_v2(&v2_request([0x01; 16]), None);
    alloc.on_allocation_v2(&v2_request([0x02; 16]), None);

    let entries = alloc.list_allocations();
    let node_ids: Vec<u16> = entries.iter().map(|e| e.node_id).collect();
    let mut sorted = node_ids.clone();
    sorted.sort_unstable();
    assert_eq!(node_ids, sorted, "entries must be ordered by node_id");

    let _ = std::fs::remove_file(&path);
}
