use bytes::Bytes;
use dashmap::DashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use dtmpfs_common::id::NodeId;

pub use dtmpfs_common::id::BlockKey;

pub struct StoreState {
    pub blocks:     DashMap<BlockKey, Bytes>,
    pub node_id:    NodeId,
    pub meta_addr:  String,
    pub ram_budget: u64,
    pub ram_used:   AtomicU64,
}

impl StoreState {
    pub fn new(node_id: NodeId, meta_addr: String, ram_budget: u64) -> Arc<Self> {
        Arc::new(StoreState {
            blocks:   DashMap::new(),
            node_id,
            meta_addr,
            ram_budget,
            ram_used: AtomicU64::new(0),
        })
    }
}
