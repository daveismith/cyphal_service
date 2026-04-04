//! PNP allocation database.
//!
//! Wraps a SQLite connection and provides the allocation table operations needed
//! by the single-allocator PNP node-ID assignment algorithm.
//!
//! The schema is deliberately compatible with the yakut allocation table so
//! that databases can be shared between tools.

#![allow(dead_code)]

use std::path::Path;

use rusqlite::{Connection, params};

/// A single row from the allocation table.
#[derive(Debug, Clone)]
pub struct AllocationEntry {
    /// Assigned Cyphal node ID.
    pub node_id: u16,
    /// Hex-encoded 128-bit unique ID (32 lower-case hex characters).
    pub unique_id_hex: String,
    /// 48-bit hash value stored for v1 allocations; `None` for v2 and static nodes.
    pub pseudo_unique_id: Option<i64>,
    /// Timestamp string as stored by SQLite (`current_timestamp`).
    pub ts: String,
}

/// Thin wrapper around a SQLite connection providing the allocation table operations.
pub struct AllocationDb {
    conn: Connection,
}

impl AllocationDb {
    /// Open (or create) the SQLite allocation database at `path`.
    ///
    /// Parent directories are created as needed.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Cannot create DB directory {}: {e}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .map_err(|e| format!("Cannot open DB at {}: {e}", path.display()))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS `allocation` (
                `node_id`          int not null unique check(node_id >= 0),
                `unique_id_hex`    varchar(32),
                `pseudo_unique_id` bigint,
                `ts`               time not null default current_timestamp,
                primary key(node_id)
            );",
        )
        .map_err(|e| format!("Cannot create allocation table: {e}"))?;

        Ok(AllocationDb { conn })
    }

    /// Look up a node_id by the full 128-bit unique ID (v2 allocation).
    ///
    /// Returns `Some(node_id)` if a prior allocation exists for this unique ID.
    pub fn find_by_unique_id(&self, uid: &[u8; 16]) -> Option<u16> {
        let hex = bytes_to_hex(uid);
        self.conn
            .query_row(
                "SELECT node_id FROM allocation WHERE unique_id_hex = ?1 LIMIT 1",
                params![hex],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .map(|id| id as u16)
    }

    /// Look up a node_id by the 48-bit unique-ID hash (v1 allocation).
    ///
    /// Returns `Some(node_id)` if a prior v1 allocation exists for this hash.
    pub fn find_by_hash(&self, hash: u64) -> Option<u16> {
        let hash_signed = hash as i64;
        self.conn
            .query_row(
                "SELECT node_id FROM allocation WHERE pseudo_unique_id = ?1 LIMIT 1",
                params![hash_signed],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .map(|id| id as u16)
    }

    /// Find the lowest available node ID from 1 to `max_id` (inclusive).
    ///
    /// IDs already in the allocation table or in the `reserved` slice are skipped.
    /// Returns `None` if no free ID is available.
    pub fn next_free_id(&self, reserved: &[u16], max_id: u16) -> Option<u16> {
        for candidate in 1..=max_id {
            if reserved.contains(&candidate) {
                continue;
            }
            let count: i64 = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM allocation WHERE node_id = ?1",
                    params![candidate as i64],
                    |row| row.get(0),
                )
                .unwrap_or(1);
            if count == 0 {
                return Some(candidate);
            }
        }
        None
    }

    /// Record a v2 (full unique-ID) PNP allocation.
    pub fn insert_v2(&self, node_id: u16, uid: &[u8; 16]) -> Result<(), String> {
        let hex = bytes_to_hex(uid);
        self.conn
            .execute(
                "INSERT INTO allocation (node_id, unique_id_hex, pseudo_unique_id) VALUES (?1, ?2, NULL)",
                params![node_id as i64, hex],
            )
            .map(|_| ())
            .map_err(|e| format!("DB insert_v2 failed: {e}"))
    }

    /// Record a v1 (48-bit hash) PNP allocation.
    ///
    /// The `unique_id_hex` is left-zero-padded to 32 chars (10 zero bytes then
    /// 6 hash bytes), matching yakut's behaviour.
    pub fn insert_v1(&self, node_id: u16, hash: u64) -> Result<(), String> {
        // Build the padded unique_id_hex: 10 zero bytes + 6 bytes from the hash.
        // The hash is 48 bits stored as u64 (8 bytes); we take the lower 6 bytes.
        let hash_bytes = hash.to_be_bytes(); // big-endian: [0, 0, b5, b4, b3, b2, b1, b0]
        let mut padded = [0u8; 16];
        padded[10..16].copy_from_slice(&hash_bytes[2..8]);
        let hex = bytes_to_hex(&padded);
        let hash_signed = hash as i64;
        self.conn
            .execute(
                "INSERT INTO allocation (node_id, unique_id_hex, pseudo_unique_id) VALUES (?1, ?2, ?3)",
                params![node_id as i64, hex, hash_signed],
            )
            .map(|_| ())
            .map_err(|e| format!("DB insert_v1 failed: {e}"))
    }

    /// Record a static node observed via heartbeat.
    ///
    /// Uses INSERT OR IGNORE so an existing allocation entry is never overwritten.
    pub fn record_static_node(&self, node_id: u16) -> Result<(), String> {
        let zero_hex = "0".repeat(32);
        self.conn
            .execute(
                "INSERT OR IGNORE INTO allocation (node_id, unique_id_hex, pseudo_unique_id) VALUES (?1, ?2, NULL)",
                params![node_id as i64, zero_hex],
            )
            .map(|_| ())
            .map_err(|e| format!("DB record_static_node failed: {e}"))
    }

    /// Return all rows in the allocation table, ordered by node_id.
    pub fn list_all(&self) -> Vec<AllocationEntry> {
        let mut stmt = match self.conn.prepare(
            "SELECT node_id, unique_id_hex, pseudo_unique_id, ts FROM allocation ORDER BY node_id",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        stmt.query_map([], |row| {
            Ok(AllocationEntry {
                node_id: row.get::<_, i64>(0)? as u16,
                unique_id_hex: row.get::<_, String>(1).unwrap_or_default(),
                pseudo_unique_id: row.get::<_, Option<i64>>(2)?,
                ts: row.get::<_, String>(3).unwrap_or_default(),
            })
        })
        .unwrap_or_else(|_| {
            // Safety: this branch is only taken when query_map returns Err,
            // which should not happen for a valid prepared statement.
            panic!("unexpected query_map error")
        })
        .filter_map(|r| r.ok())
        .collect()
    }
}

/// Encode `bytes` as a lower-case hex string.
pub(crate) fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_db() -> (AllocationDb, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "pnp_test_{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let db = AllocationDb::open(&path).expect("open temp db");
        (db, path)
    }

    #[test]
    fn test_open_creates_table() {
        let (db, path) = temp_db();
        let entries = db.list_all();
        assert!(entries.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_insert_and_find_v2() {
        let (db, path) = temp_db();
        let uid: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        db.insert_v2(42, &uid).unwrap();
        assert_eq!(db.find_by_unique_id(&uid), Some(42));
        let uid2: [u8; 16] = [0; 16];
        assert_eq!(db.find_by_unique_id(&uid2), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_insert_and_find_v1() {
        let (db, path) = temp_db();
        let hash: u64 = 0xABCDEF_123456;
        db.insert_v1(99, hash).unwrap();
        assert_eq!(db.find_by_hash(hash), Some(99));
        assert_eq!(db.find_by_hash(0xDEADBEEF), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_v1_unique_id_hex_format() {
        let (db, path) = temp_db();
        let hash: u64 = 0x0000_AABBCCDDEEFF;
        db.insert_v1(10, hash).unwrap();
        let entries = db.list_all();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].unique_id_hex, "00000000000000000000aabbccddeeff");
        assert_eq!(
            entries[0].pseudo_unique_id,
            Some(0x0000_AABBCCDDEEFF_u64 as i64)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_next_free_id_basic() {
        let (db, path) = temp_db();
        assert_eq!(db.next_free_id(&[], 127), Some(1));
        let uid: [u8; 16] = [1; 16];
        db.insert_v2(1, &uid).unwrap();
        assert_eq!(db.next_free_id(&[], 127), Some(2));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_next_free_id_with_reserved() {
        let (db, path) = temp_db();
        assert_eq!(db.next_free_id(&[1, 2], 127), Some(3));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_next_free_id_full_table() {
        let (db, path) = temp_db();
        for id in 1u16..=3 {
            let uid: [u8; 16] = [id as u8; 16];
            db.insert_v2(id, &uid).unwrap();
        }
        assert_eq!(db.next_free_id(&[], 3), None);
        assert_eq!(db.next_free_id(&[], 4), Some(4));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_record_static_node() {
        let (db, path) = temp_db();
        db.record_static_node(50).unwrap();
        let entries = db.list_all();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].node_id, 50);
        assert_eq!(entries[0].unique_id_hex, "0".repeat(32));
        assert_eq!(entries[0].pseudo_unique_id, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_record_static_node_no_overwrite() {
        let (db, path) = temp_db();
        let uid: [u8; 16] = [0xAB; 16];
        db.insert_v2(50, &uid).unwrap();
        db.record_static_node(50).unwrap();
        let entries = db.list_all();
        assert_eq!(entries.len(), 1);
        // Unique ID should still be the v2 one
        assert_eq!(
            entries[0].unique_id_hex,
            "abababababababababababababababababab"
                .chars()
                .take(32)
                .collect::<String>()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_next_free_id_skips_static_nodes() {
        let (db, path) = temp_db();
        for id in 1u16..=5 {
            db.record_static_node(id).unwrap();
        }
        assert_eq!(db.next_free_id(&[], 127), Some(6));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_list_all_ordered() {
        let (db, path) = temp_db();
        let uid1: [u8; 16] = [1; 16];
        let uid2: [u8; 16] = [2; 16];
        db.insert_v2(20, &uid1).unwrap();
        db.insert_v2(10, &uid2).unwrap();
        let entries = db.list_all();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].node_id, 10);
        assert_eq!(entries[1].node_id, 20);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_bytes_to_hex() {
        assert_eq!(bytes_to_hex(&[0x00, 0xAB, 0xCD]), "00abcd");
        assert_eq!(bytes_to_hex(&[0; 16]), "0".repeat(32));
    }
}
