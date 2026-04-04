//! Plug-and-Play node-ID allocation.
//!
//! Implements the single-allocator PNP algorithm for Cyphal/UAVCAN.
//! Each transport interface maintains its own allocation database in a SQLite file.
//!
//! # Supported message versions
//! - `uavcan.pnp.NodeIDAllocationData.1.0` (subject 8166) – for Classic CAN
//! - `uavcan.pnp.NodeIDAllocationData.2.0` (subject 8165) – for CAN FD, UDP, Serial

pub mod allocator;
pub mod db;

#[allow(unused_imports)]
pub use allocator::PnpAllocator;
#[allow(unused_imports)]
pub use db::{AllocationDb, AllocationEntry};
