use std::time::{Duration, Instant};
use bytes::Bytes;
use moka::sync::Cache;
use dtmpfs_common::id::{BlockIdx, Generation, InodeId};

#[derive(Clone)]
pub struct CachedAttr {
    pub attr:    fuser::FileAttr,
    pub fetched: Instant,
}

pub fn build_attr_cache(ttl_ms: u64) -> Cache<InodeId, CachedAttr> {
    Cache::builder()
        .max_capacity(100_000)
        .time_to_live(Duration::from_millis(ttl_ms))
        .build()
}

pub fn build_block_cache(capacity_mb: u64) -> Cache<(InodeId, Generation, BlockIdx), Bytes> {
    let cap_bytes = capacity_mb * 1024 * 1024;
    Cache::builder()
        .max_capacity(cap_bytes)
        .weigher(|_k, v: &Bytes| v.len() as u32)
        .build()
}
