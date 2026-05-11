use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord,
         Serialize, Deserialize)]
#[serde(transparent)]
pub struct InodeId(pub u64);

impl InodeId {
    pub const ROOT: InodeId = InodeId(1);
    pub fn raw(self) -> u64 { self.0 }
}

impl Default for InodeId {
    fn default() -> Self { InodeId(0) }
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord,
         Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Generation(pub u64);

impl Generation {
    pub fn bump(self) -> Generation { Generation(self.0 + 1) }
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord,
         Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlockIdx(pub u64);

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord,
         Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub String);

impl NodeId {
    pub fn new(s: impl Into<String>) -> Self { NodeId(s.into()) }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl Default for NodeId {
    fn default() -> Self { NodeId(String::new()) }
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockKey {
    pub ino:        InodeId,
    pub block_idx:  BlockIdx,
    pub generation: Generation,
}

impl BlockKey {
    /// Placement key omits generation — placement is per-(ino, idx) and stable across rewrites.
    pub fn placement_key(&self) -> u128 {
        ((self.ino.0 as u128) << 64) | (self.block_idx.0 as u128)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockPlacement {
    pub primary:  NodeId,
    pub replicas: Vec<NodeId>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InodeKind { File, Dir, Symlink }
