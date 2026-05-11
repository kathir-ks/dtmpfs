use bytes::Bytes;
use dashmap::DashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use dtmpfs_common::id::{BlockIdx, Generation, InodeId, NodeId};

pub use dtmpfs_common::id::BlockKey;

pub struct StoreState {
    pub blocks:      DashMap<BlockKey, Bytes>,
    /// High-water generation per logical block (ino, block_idx).
    /// WriteBlock is rejected when its generation < the stored high-water.
    pub high_water:  DashMap<(InodeId, BlockIdx), Generation>,
    pub node_id:     NodeId,
    pub meta_addr:   String,
    pub ram_budget:  u64,
    pub ram_used:    AtomicU64,
}

impl StoreState {
    pub fn new(node_id: NodeId, meta_addr: String, ram_budget: u64) -> Arc<Self> {
        Arc::new(StoreState {
            blocks:     DashMap::new(),
            high_water: DashMap::new(),
            node_id,
            meta_addr,
            ram_budget,
            ram_used:   AtomicU64::new(0),
        })
    }
}
