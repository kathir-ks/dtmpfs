use std::collections::BTreeMap;
use bytes::BytesMut;
use dtmpfs_common::id::{BlockIdx, BlockPlacement, Generation, InodeId};

pub struct OpenFile {
    pub ino:        InodeId,
    pub generation: Generation,
    pub block_map:  BTreeMap<BlockIdx, BlockPlacement>,
    pub dirty:      BTreeMap<BlockIdx, BytesMut>,
    pub size_hint:  u64,
    pub flags:      i32,
}
