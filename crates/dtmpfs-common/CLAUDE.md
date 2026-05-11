# dtmpfs-common — Agent Brief

## Role

Pure library crate providing shared types, HRW block placement, config deserialization, and error
types. Has **no** runtime I/O dependencies — no tokio, no gRPC servers, no FUSE. Unit tests
compile in under a second.

## Crate boundaries

- You own everything under `crates/dtmpfs-common/`.
- Do NOT touch `proto/`, other crates, or the workspace `Cargo.toml`.
- Do NOT add tonic-build or fuser as dependencies.

## Files to create

1. `Cargo.toml`
2. `src/lib.rs`
3. `src/id.rs`   — ID newtypes + BlockKey + BlockPlacement + InodeKind
4. `src/error.rs` — DtmpfsError enum + libc errno + tonic::Status conversions
5. `src/config.rs` — TOML-deserialized config types
6. `src/hash.rs`  — HRW block placement using xxhash

---

## 1. `Cargo.toml`

```toml
[package]
name = "dtmpfs-common"
version.workspace = true
edition.workspace = true

[dependencies]
serde       = { workspace = true }
toml        = { workspace = true }
thiserror   = { workspace = true }
anyhow      = { workspace = true }
bytes       = { workspace = true }
xxhash-rust = { workspace = true }
libc        = { workspace = true }
tonic       = { workspace = true }
tracing     = { workspace = true }
```

---

## 2. `src/lib.rs`

```rust
pub mod config;
pub mod error;
pub mod hash;
pub mod id;

pub use error::{DtmpfsError, Result};
```

---

## 3. `src/id.rs`

```rust
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
```

---

## 4. `src/error.rs`

```rust
use thiserror::Error;
use crate::id::NodeId;

#[derive(Debug, Error)]
pub enum DtmpfsError {
    #[error("meta server unavailable")]
    MetaUnavailable,
    #[error("store node {0:?} unavailable")]
    StoreUnavailable(NodeId),
    #[error("block generation mismatch (write rejected as stale)")]
    BlockGenerationMismatch,
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    AlreadyExists,
    #[error("not a directory")]
    NotADirectory,
    #[error("is a directory")]
    IsADirectory,
    #[error("directory not empty")]
    NotEmpty,
    #[error("permission denied")]
    PermissionDenied,
    #[error("resource exhausted")]
    ResourceExhausted,
    #[error("invalid argument")]
    InvalidArgument,
    #[error("unauthenticated")]
    Unauthenticated,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rpc: {0}")]
    Rpc(#[from] tonic::Status),
}

impl From<DtmpfsError> for libc::c_int {
    fn from(e: DtmpfsError) -> libc::c_int {
        use DtmpfsError::*;
        match e {
            NotFound                          => libc::ENOENT,
            AlreadyExists                     => libc::EEXIST,
            NotADirectory                     => libc::ENOTDIR,
            IsADirectory                      => libc::EISDIR,
            NotEmpty                          => libc::ENOTEMPTY,
            PermissionDenied | Unauthenticated => libc::EACCES,
            ResourceExhausted                 => libc::ENOSPC,
            InvalidArgument                   => libc::EINVAL,
            MetaUnavailable | StoreUnavailable(_)
            | BlockGenerationMismatch | Io(_) | Rpc(_) => libc::EIO,
        }
    }
}

impl DtmpfsError {
    pub fn from_status(s: tonic::Status, node: Option<NodeId>) -> DtmpfsError {
        use tonic::Code::*;
        match s.code() {
            InvalidArgument | OutOfRange => DtmpfsError::InvalidArgument,
            NotFound                     => DtmpfsError::NotFound,
            AlreadyExists                => DtmpfsError::AlreadyExists,
            PermissionDenied             => DtmpfsError::PermissionDenied,
            Unauthenticated              => DtmpfsError::Unauthenticated,
            ResourceExhausted            => DtmpfsError::ResourceExhausted,
            FailedPrecondition           => DtmpfsError::BlockGenerationMismatch,
            Unavailable => match node {
                Some(n) => DtmpfsError::StoreUnavailable(n),
                None    => DtmpfsError::MetaUnavailable,
            },
            _ => DtmpfsError::Rpc(s),
        }
    }
}

pub type Result<T> = std::result::Result<T, DtmpfsError>;
```

---

## 5. `src/config.rs`

```rust
use serde::{Deserialize, Serialize};
use crate::id::NodeId;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Config {
    Meta(MetaConfig),
    Store(StoreConfig),
    Client(ClientConfig),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MetaConfig {
    pub node_id:              NodeId,
    pub listen:               String,
    pub cluster_token:        String,
    #[serde(default = "d_replication")]
    pub replication_factor:   usize,
    #[serde(default = "d_block_size")]
    pub block_size:           usize,
    #[serde(default = "d_heartbeat_timeout_ms")]
    pub heartbeat_timeout_ms: u64,
    #[serde(default = "d_max_open_handles")]
    pub max_open_handles:     u64,
    #[serde(default)]
    pub gc_interval_ms:       Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct StoreConfig {
    pub node_id:           NodeId,
    pub listen:            String,
    pub advertise_addr:    String,
    pub meta_addr:         String,
    pub cluster_token:     String,
    pub ram_budget_bytes:  u64,
    #[serde(default)]
    pub debug_http_listen: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ClientConfig {
    pub node_id:                 NodeId,
    pub mount_point:             String,
    pub meta_addr:               String,
    pub cluster_token:           String,
    #[serde(default = "d_block_size")]
    pub block_size:              usize,
    #[serde(default = "d_replication")]
    pub replication_factor:      usize,
    #[serde(default = "d_attr_ttl_ms")]
    pub attr_cache_ttl_ms:       u64,
    #[serde(default = "d_block_cache_mb")]
    pub block_cache_capacity_mb: u64,
    #[serde(default = "d_fuse_threads")]
    pub fuse_threads:            usize,
    #[serde(default)]
    pub tokio_worker_threads:    Option<u32>,
    #[serde(default = "d_keepalive_secs")]
    pub keepalive_interval_secs: u64,
    #[serde(default = "d_rpc_timeout_ms")]
    pub rpc_timeout_ms:          u64,
    #[serde(default = "d_write_rpc_timeout_ms")]
    pub write_rpc_timeout_ms:    u64,
    #[serde(default)]
    pub mount_options:           ClientMountOptions,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct ClientMountOptions {
    #[serde(default = "d_true")] pub allow_other:         bool,
    #[serde(default = "d_true")] pub default_permissions: bool,
    #[serde(default = "d_true")] pub auto_unmount:        bool,
    #[serde(default = "d_true")] pub no_atime:            bool,
}

fn d_replication() -> usize { 1 }
fn d_block_size() -> usize { 1 << 20 }
fn d_heartbeat_timeout_ms() -> u64 { 5000 }
fn d_attr_ttl_ms() -> u64 { 1000 }
fn d_block_cache_mb() -> u64 { 1024 }
fn d_fuse_threads() -> usize { 4 }
fn d_max_open_handles() -> u64 { 100_000 }
fn d_keepalive_secs() -> u64 { 30 }
fn d_rpc_timeout_ms() -> u64 { 5000 }
fn d_write_rpc_timeout_ms() -> u64 { 30_000 }
fn d_true() -> bool { true }

pub fn load(path: &str) -> anyhow::Result<Config> {
    let s = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&s)?)
}
```

---

## 6. `src/hash.rs`

HRW (Highest Random Weight / Rendezvous) hashing. Given a `BlockKey` and a list of `NodeId`s,
returns up to R nodes ranked by score — element 0 is the primary.

```rust
use crate::id::{BlockKey, NodeId};
use xxhash_rust::xxh3::xxh3_64_with_seed;

/// Pick up to `r` nodes for a block. Returns nodes in score order (index 0 = primary).
/// Generation is deliberately excluded from the hash — placement is per-(ino, idx)
/// and must be stable across rewrites of the same block.
pub fn pick_nodes(key: &BlockKey, nodes: &[NodeId], r: usize) -> Vec<NodeId> {
    if nodes.is_empty() { return Vec::new(); }
    let r = r.min(nodes.len());
    let pk = key.placement_key();    // u128: ino<<64 | block_idx
    let lo = pk as u64;
    let hi = (pk >> 64) as u64;

    let mut scored: Vec<(u64, &NodeId)> = nodes.iter()
        .map(|n| {
            let mut h = xxh3_64_with_seed(n.as_str().as_bytes(), lo);
            h ^= xxh3_64_with_seed(n.as_str().as_bytes(), hi);
            (h, n)
        })
        .collect();
    scored.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().take(r).map(|(_, n)| n.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{BlockIdx, BlockKey, Generation, InodeId, NodeId};

    fn nodes(n: usize) -> Vec<NodeId> {
        (0..n).map(|i| NodeId::new(format!("store-{i}"))).collect()
    }

    #[test]
    fn deterministic() {
        let ns = nodes(8);
        let k = BlockKey { ino: InodeId(42), block_idx: BlockIdx(7), generation: Generation(0) };
        assert_eq!(pick_nodes(&k, &ns, 2), pick_nodes(&k, &ns, 2));
    }

    #[test]
    fn empty_nodes() {
        let k = BlockKey { ino: InodeId(1), block_idx: BlockIdx(0), generation: Generation(0) };
        assert!(pick_nodes(&k, &[], 3).is_empty());
    }

    #[test]
    fn r_capped() {
        let ns = nodes(2);
        let k = BlockKey { ino: InodeId(1), block_idx: BlockIdx(0), generation: Generation(0) };
        assert_eq!(pick_nodes(&k, &ns, 5).len(), 2);
    }

    #[test]
    fn minimal_disruption() {
        let n8 = nodes(8);
        let n7: Vec<_> = n8.iter().cloned().filter(|n| n.as_str() != "store-3").collect();
        let mut moved = 0usize;
        for i in 0..1024 {
            let k = BlockKey { ino: InodeId(1), block_idx: BlockIdx(i), generation: Generation(0) };
            if pick_nodes(&k, &n8, 1)[0] != pick_nodes(&k, &n7, 1)[0] { moved += 1; }
        }
        assert!(moved < 1024 * 2 / 8, "moved={moved}");
    }
}
```

---

## Build / test command

```bash
cargo build -p dtmpfs-common
cargo test -p dtmpfs-common
```

## Done when

`cargo test -p dtmpfs-common` passes (including the `hash::tests::minimal_disruption` test).
