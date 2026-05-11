# dtmpfs — Low-Level Design

This document describes how each component of dtmpfs is implemented. It is the
implementer's reference: every public type, every algorithm, every threading
decision is named here. Anyone holding this document plus the proto files
should be able to write the system without further design work.

The design assumes one Rust workspace containing six crates. It assumes the
constraints fixed in `docs/HLD.md`: gRPC (tonic 0.12) on the wire, FUSE 3 via
`fuser` 0.14 in the client, RAM-only storage on the stores, NFS-style
close-to-open consistency, single centralized meta server, replication
factor R configurable per cluster.

For higher-level rationale see:

- `docs/HLD.md` — high-level design and architecture rationale.
- `docs/architecture.md` — diagrams, role boundaries.
- `docs/protocol.md` — wire format reference for `proto/meta.proto` and
  `proto/store.proto`.
- `docs/consistency.md` — the close-to-open contract and its limitations.
- `docs/failure-model.md` — what failures are tolerated and how.
- `docs/operations.md` — how to run a cluster.
- `docs/configuration.md` — TOML schema, defaults, tunables.
- `docs/testing.md` and `docs/acceptance-tests.md` — how the system is verified.

This file does not duplicate those — it links to them.

---

## 1. Workspace layout & crate dependencies

### 1.1 Top-level `Cargo.toml`

```toml
[workspace]
resolver = "2"
members = [
    "crates/dtmpfs-proto",
    "crates/dtmpfs-common",
    "crates/dtmpfs-meta",
    "crates/dtmpfs-store",
    "crates/dtmpfs-client",
]

[workspace.package]
version      = "0.1.0"
edition      = "2021"
rust-version = "1.94"
license      = "Apache-2.0"
repository   = "https://github.com/kathir-ks/dtmpfs"

# Pin once, reuse via `workspace = true` in member crates.
[workspace.dependencies]
tonic            = { version = "0.12", features = ["transport"] }
tonic-build      = { version = "0.12" }
prost            = { version = "0.13" }
prost-types      = { version = "0.13" }
tokio            = { version = "1", features = ["full"] }
tokio-stream     = { version = "0.1" }
futures          = { version = "0.3" }
bytes            = { version = "1" }
dashmap          = { version = "6" }
moka             = { version = "0.12", features = ["sync"] }
arc-swap         = { version = "1" }
serde            = { version = "1", features = ["derive"] }
toml             = { version = "0.8" }
bincode          = { version = "1.3" }
thiserror        = { version = "1" }
anyhow           = { version = "1" }
tracing          = { version = "0.1" }
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
clap             = { version = "4", features = ["derive"] }
fuser            = { version = "0.14" }
libc             = { version = "0.2" }
xxhash-rust      = { version = "0.8", features = ["xxh3"] }
hyper            = { version = "1" }
axum             = { version = "0.7" }
rand             = { version = "0.8" }
```

### 1.2 Per-crate dependency manifests

#### `crates/dtmpfs-proto/Cargo.toml`

Sole purpose: invoke `tonic_build` and re-export the generated Rust types so
client/server crates do not each pay the codegen cost.

```toml
[package]
name = "dtmpfs-proto"
version.workspace = true
edition.workspace = true

[dependencies]
tonic = { workspace = true }
prost = { workspace = true }
bytes = { workspace = true }

[build-dependencies]
tonic-build = { workspace = true }
```

`build.rs`:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .bytes(["."])           // emit `bytes::Bytes` for `bytes` proto fields
        .build_client(true)
        .build_server(true)
        .compile_protos(
            &["../../proto/meta.proto", "../../proto/store.proto"],
            &["../../proto"],
        )?;
    Ok(())
}
```

`src/lib.rs`:

```rust
pub mod meta {
    tonic::include_proto!("dtmpfs.meta");
}
pub mod store {
    tonic::include_proto!("dtmpfs.store");
}
```

#### `crates/dtmpfs-common/Cargo.toml`

Pure library: IDs, hashing, errors, config. No I/O dependencies — keeps the
unit-test cycle fast.

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
tonic       = { workspace = true }   # for `Status` -> `DtmpfsError` conversion only
tracing     = { workspace = true }
```

#### `crates/dtmpfs-meta/Cargo.toml`

```toml
[package]
name = "dtmpfs-meta"
version.workspace = true
edition.workspace = true

[[bin]]
name = "metasrv"
path = "src/main.rs"

[dependencies]
dtmpfs-proto       = { path = "../dtmpfs-proto" }
dtmpfs-common      = { path = "../dtmpfs-common" }
tonic              = { workspace = true }
prost              = { workspace = true }
tokio              = { workspace = true }
tokio-stream       = { workspace = true }
futures            = { workspace = true }
bytes              = { workspace = true }
serde              = { workspace = true }
toml               = { workspace = true }
thiserror          = { workspace = true }
anyhow             = { workspace = true }
tracing            = { workspace = true }
tracing-subscriber = { workspace = true }
clap               = { workspace = true }
```

#### `crates/dtmpfs-store/Cargo.toml`

```toml
[package]
name = "dtmpfs-store"
version.workspace = true
edition.workspace = true

[[bin]]
name = "storesrv"
path = "src/main.rs"

[dependencies]
dtmpfs-proto       = { path = "../dtmpfs-proto" }
dtmpfs-common      = { path = "../dtmpfs-common" }
tonic              = { workspace = true }
prost              = { workspace = true }
tokio              = { workspace = true }
tokio-stream       = { workspace = true }
futures            = { workspace = true }
bytes              = { workspace = true }
dashmap            = { workspace = true }
serde              = { workspace = true }
toml               = { workspace = true }
thiserror          = { workspace = true }
anyhow             = { workspace = true }
tracing            = { workspace = true }
tracing-subscriber = { workspace = true }
clap               = { workspace = true }
hyper              = { workspace = true }
axum               = { workspace = true }
```

#### `crates/dtmpfs-client/Cargo.toml`

```toml
[package]
name = "dtmpfs-client"
version.workspace = true
edition.workspace = true

[[bin]]
name = "dtmpfs-mount"
path = "src/main.rs"

[dependencies]
dtmpfs-proto       = { path = "../dtmpfs-proto" }
dtmpfs-common      = { path = "../dtmpfs-common" }
tonic              = { workspace = true }
prost              = { workspace = true }
tokio              = { workspace = true }
tokio-stream       = { workspace = true }
futures            = { workspace = true }
bytes              = { workspace = true }
dashmap            = { workspace = true }
moka               = { workspace = true }
arc-swap           = { workspace = true }
fuser              = { workspace = true }
libc               = { workspace = true }
serde              = { workspace = true }
toml               = { workspace = true }
thiserror          = { workspace = true }
anyhow             = { workspace = true }
tracing            = { workspace = true }
tracing-subscriber = { workspace = true }
clap               = { workspace = true }
```

### 1.3 Why each dep is in the tree

- `tonic` 0.12 — gRPC over HTTP/2, integrates with tokio, deadlines, streaming.
- `tonic-build` 0.12 — compile `.proto` to Rust at build time.
- `prost` 0.13 — protobuf encoder/decoder used by tonic-generated code.
- `tokio` 1 (`full`) — async runtime; needed by tonic, hyper, all RPC code.
- `futures` 0.3 — `try_join_all`, `buffer_unordered`, `Stream` combinators.
- `bytes` 1 — `Bytes` for zero-copy slicing in the data path.
- `dashmap` 6 — sharded concurrent map; the store's block table and the
  client's `open_files` table.
- `moka` 0.12 (`sync`) — concurrent LRU with TTL and weigher; AttrCache and
  BlockCache.
- `arc-swap` 1 — lock-free swap of the `node_id -> addr` map on membership
  changes.
- `serde` 1 + `toml` 0.8 — config files and on-the-wire ID representations.
- `bincode` 1.3 — kept for tests that snapshot in-memory state to bytes.
- `thiserror` 1 — derives `Error` for `DtmpfsError`.
- `anyhow` 1 — top-level error type in `main` only; never crosses crate
  boundaries.
- `tracing` + `tracing-subscriber` — logging with structured spans.
- `clap` 4 (`derive`) — CLI parsing in each binary.
- `fuser` 0.14 — FUSE 3 binding; the client's interface to the kernel.
- `libc` 0.2 — `errno` constants for the FUSE error mapping.
- `xxhash-rust` 0.8 (`xxh3`) — fast non-cryptographic hash for HRW.
- `hyper` 1 + `axum` 0.7 — `/debug/blocks` HTTP endpoint on the store.
- `rand` 0.8 — seeded RNG in tests only.

### 1.4 `rust-toolchain.toml`

```toml
[toolchain]
channel    = "1.94.0"
components = ["rustfmt", "clippy"]
profile    = "default"
targets    = ["x86_64-unknown-linux-gnu"]
```

Pinning `1.94.0` matches the available toolchain on the workstation and avoids
churn from stable upgrades. CI re-runs against `stable` weekly to catch drift.

---

## 2. Shared types in `dtmpfs-common`

This crate has zero runtime dependencies (no tokio, no I/O) so its tests
compile in <1 s.

### 2.1 ID newtypes — `crates/dtmpfs-common/src/id.rs`

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

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockKey {
    pub ino: InodeId,
    pub block_idx: BlockIdx,
    pub generation: Generation,
}

impl BlockKey {
    /// Hash value consumed by `pick_nodes`. Includes only `ino` and
    /// `block_idx` because placement is per-(ino, idx) and stable across
    /// rewrites.  See `dtmpfs-common::hash::pick_nodes`.
    pub fn placement_key(&self) -> u128 {
        ((self.ino.0 as u128) << 64) | (self.block_idx.0 as u128)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockPlacement {
    pub primary: NodeId,
    pub replicas: Vec<NodeId>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InodeKind { File, Dir, Symlink }
```

`InodeId` is `Copy` so it travels through hot paths without ref-count traffic.
`NodeId` is a `String` newtype rather than an interned `u32` — at <100 nodes
in v1 the cost is invisible and string-typed errors are easier to read in
logs. If profiling later shows it matters, swap to `Arc<str>` without
breaking serde wire compatibility.

### 2.2 Errors — `crates/dtmpfs-common/src/error.rs`

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

/// Map an internal error to a libc errno that FUSE will surface to userspace.
/// The full canonical mapping lives in `decisions.md` R5.
impl From<DtmpfsError> for libc::c_int {
    fn from(e: DtmpfsError) -> libc::c_int {
        use DtmpfsError::*;
        match e {
            NotFound              => libc::ENOENT,    //  2
            AlreadyExists         => libc::EEXIST,    // 17
            NotADirectory         => libc::ENOTDIR,   // 20
            IsADirectory          => libc::EISDIR,    // 21
            NotEmpty              => libc::ENOTEMPTY, // 39
            PermissionDenied
            | Unauthenticated     => libc::EACCES,    // 13
            ResourceExhausted     => libc::ENOSPC,    // 28
            InvalidArgument       => libc::EINVAL,    // 22
            MetaUnavailable
            | StoreUnavailable(_)
            | BlockGenerationMismatch
            | Io(_)
            | Rpc(_)              => libc::EIO,       //  5
        }
    }
}

/// Convenience: inspect a `tonic::Status` and lift it into the typed error.
/// Per `decisions.md` R5, every `tonic::Code` arm maps to a single errno —
/// see the table there for the full canonical mapping. The arms below
/// preserve typed information where useful and fold the rest into `Rpc`,
/// which the libc-mapping impl above turns into the right errno.
impl DtmpfsError {
    pub fn from_status(s: tonic::Status, node: Option<NodeId>) -> DtmpfsError {
        use tonic::Code::*;
        match s.code() {
            Cancelled          => DtmpfsError::Rpc(s),       // -> EINTR via Rpc fallback below; explicit if desired
            InvalidArgument
            | OutOfRange       => DtmpfsError::InvalidArgument,
            NotFound           => DtmpfsError::NotFound,
            AlreadyExists      => DtmpfsError::AlreadyExists,
            PermissionDenied   => DtmpfsError::PermissionDenied,
            Unauthenticated    => DtmpfsError::Unauthenticated,
            ResourceExhausted  => DtmpfsError::ResourceExhausted,
            FailedPrecondition => DtmpfsError::BlockGenerationMismatch,
            Unimplemented      => DtmpfsError::Rpc(s),       // -> ENOSYS via mapping
            Unavailable => match node {
                Some(n) => DtmpfsError::StoreUnavailable(n),
                None    => DtmpfsError::MetaUnavailable,
            },
            // Aborted / Internal / DataLoss / DeadlineExceeded / Unknown / Ok
            _ => DtmpfsError::Rpc(s),
        }
    }
}

pub type Result<T> = std::result::Result<T, DtmpfsError>;
```

### 2.3 Config — `crates/dtmpfs-common/src/config.rs`

One Rust enum, one TOML schema. The role tag picks the variant.

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
    pub node_id: NodeId,
    pub listen: String,                     // "0.0.0.0:7100"
    pub cluster_token: String,
    #[serde(default = "d_replication")]
    pub replication_factor: usize,          // R; default 1
    #[serde(default = "d_block_size")]
    pub block_size: usize,                  // 1 MiB
    #[serde(default = "d_heartbeat_timeout_ms")]
    pub heartbeat_timeout_ms: u64,          // 5000
    #[serde(default = "d_max_open_handles")]
    pub max_open_handles: u64,              // (v1) soft cap, warn-only
    // (Phase 6+) parsed-and-warned-if-set in v1; orphan sweep is Phase 6 work.
    #[serde(default)]
    pub gc_interval_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct StoreConfig {
    pub node_id: NodeId,
    pub listen: String,                     // "0.0.0.0:7200"
    pub advertise_addr: String,             // "10.0.0.20:7200"
    pub meta_addr: String,                  // "http://10.0.0.10:7100"
    pub cluster_token: String,
    pub ram_budget_bytes: u64,              // hard cap on stored data
    #[serde(default)]
    pub debug_http_listen: Option<String>,  // e.g. "127.0.0.1:7300" or unset
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ClientConfig {
    pub node_id: NodeId,
    pub mount_point: String,                // "/mnt/dtmpfs"
    pub meta_addr: String,
    pub cluster_token: String,
    #[serde(default = "d_block_size")]
    pub block_size: usize,
    #[serde(default = "d_replication")]
    pub replication_factor: usize,
    #[serde(default = "d_attr_ttl_ms")]
    pub attr_cache_ttl_ms: u64,             // 1000
    #[serde(default = "d_block_cache_mb")]
    pub block_cache_capacity_mb: u64,       // 1024 MiB
    #[serde(default = "d_fuse_threads")]
    pub fuse_threads: usize,                // 4
    // (v1) tunables — see decisions.md R17.
    #[serde(default)]
    pub tokio_worker_threads: Option<u32>,  // None => num_cpus
    #[serde(default = "d_keepalive_secs")]
    pub keepalive_interval_secs: u64,       // 30
    #[serde(default = "d_rpc_timeout_ms")]
    pub rpc_timeout_ms: u64,                // 5000
    #[serde(default = "d_write_rpc_timeout_ms")]
    pub write_rpc_timeout_ms: u64,          // 30000
    #[serde(default)]
    pub mount_options: ClientMountOptions,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct ClientMountOptions {
    #[serde(default = "d_true")]  pub allow_other:         bool,
    #[serde(default = "d_true")]  pub default_permissions: bool,
    #[serde(default = "d_true")]  pub auto_unmount:        bool,
    #[serde(default = "d_true")]  pub no_atime:            bool,
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
```

Per `decisions.md` D2, struct field names mirror `configuration.md` (`listen`, `heartbeat_timeout_ms`, `debug_http_listen`). Per R17, `gc_interval_ms` is parsed but only consumed in Phase 6+ (orphan-block sweep); v1 logs a warning if it is set. The other v1-tagged keys (`max_open_handles`, `tokio_worker_threads`, `keepalive_interval_secs`, `rpc_timeout_ms`, `write_rpc_timeout_ms`, `[client.mount_options]`) are honoured immediately.

See `docs/configuration.md` for the full TOML reference.

---

## 3. Hashing — `dtmpfs-common::hash`

Rendezvous (Highest Random Weight) hashing places blocks on nodes. With small
node sets (≤32) HRW is simpler than a hash ring with vnodes and gives nearly
ideal balance. It also has the property that adding/removing one node moves
exactly `1/N` of the keys.

### 3.1 Pseudocode

```
pick_nodes(key, nodes, R):
    if nodes empty: return []
    R = min(R, len(nodes))
    scored = [(xxh3(node_id ++ key_bytes), node) for node in nodes]
    sort scored descending by score
    return [node for (_, node) in scored[:R]]
```

### 3.2 Implementation — `crates/dtmpfs-common/src/hash.rs`

```rust
use crate::id::{BlockKey, NodeId};
use xxhash_rust::xxh3::xxh3_64_with_seed;

/// Deterministically pick R nodes for a block. Returns nodes in score order
/// — element 0 is the primary, the rest are replicas.
pub fn pick_nodes(key: &BlockKey, nodes: &[NodeId], r: usize) -> Vec<NodeId> {
    if nodes.is_empty() {
        return Vec::new();
    }
    let r = r.min(nodes.len());
    let pk = key.placement_key();        // u128: ino<<64 | block_idx
    let lo = pk as u64;
    let hi = (pk >> 64) as u64;

    let mut scored: Vec<(u64, &NodeId)> = nodes
        .iter()
        .map(|n| {
            // Mix the node id bytes with the (ino, block_idx) seed.
            let mut h = xxh3_64_with_seed(n.as_str().as_bytes(), lo);
            h ^= xxh3_64_with_seed(n.as_str().as_bytes(), hi);
            (h, n)
        })
        .collect();
    scored.sort_unstable_by(|a, b| b.0.cmp(&a.0));      // descending
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
        let k = BlockKey {
            ino: InodeId(42),
            block_idx: BlockIdx(7),
            generation: Generation(0),
        };
        let a = pick_nodes(&k, &ns, 2);
        let b = pick_nodes(&k, &ns, 2);
        assert_eq!(a, b);
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn empty_nodes_returns_empty() {
        let k = BlockKey { ino: InodeId(1), block_idx: BlockIdx(0), generation: Generation(0) };
        assert!(pick_nodes(&k, &[], 3).is_empty());
    }

    #[test]
    fn r_capped_to_node_count() {
        let ns = nodes(2);
        let k = BlockKey { ino: InodeId(1), block_idx: BlockIdx(0), generation: Generation(0) };
        assert_eq!(pick_nodes(&k, &ns, 5).len(), 2);
    }

    #[test]
    fn minimal_disruption_when_one_node_removed() {
        // With 8 nodes and 1024 keys, removing one node should reassign
        // roughly 1/8 of the keys; the remaining 7/8 stay put.
        let n8 = nodes(8);
        let n7: Vec<_> = n8.iter().cloned().filter(|n| n.as_str() != "store-3").collect();
        let mut moved = 0usize;
        for i in 0..1024 {
            let k = BlockKey { ino: InodeId(1), block_idx: BlockIdx(i), generation: Generation(0) };
            let p8 = pick_nodes(&k, &n8, 1)[0].clone();
            let p7 = pick_nodes(&k, &n7, 1)[0].clone();
            if p8 != p7 { moved += 1; }
        }
        assert!(moved < 1024 * 2 / 8, "moved={moved}");   // < 2/8 with slack
    }
}
```

### 3.3 Properties

- **Deterministic** for any (key, node-set): every client and the meta server
  compute the same placement without coordination.
- **Minimal disruption**: removing one node reassigns only the keys that had
  that node as primary — empirically `1/N`.
- **No vnodes** at v1 scale (≤32 nodes). With 8 nodes the standard deviation
  of block count per node is well under 5%, verified by the smoke test.
- **Generation-independent**: the hash deliberately ignores `generation`.
  Placement is per-(ino, idx) and stable across rewrites; this lets a client
  with a cached `block_map` keep using it even if the file's generation has
  bumped, until it next opens.

### 3.4 Edge cases

- `nodes.is_empty()` → returns `Vec::new()`. The client treats this as
  `MetaUnavailable` because meta is the only source of truth for the live
  set; if meta returned an empty set the cluster is unconfigured.
- `r > nodes.len()` → silently capped at `nodes.len()`. Logged at warn
  level inside `MetaState::allocate_blocks` because it indicates an
  operator misconfig (e.g., `R=3` on a 2-store cluster).

---

## 4. `dtmpfs-meta` internals

### 4.1 State

```rust
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use tokio::sync::RwLock;
use dtmpfs_common::id::{BlockIdx, BlockPlacement, Generation, InodeId, InodeKind, NodeId};

pub struct MetaState {
    pub inodes:         HashMap<InodeId, Inode>,
    pub dirs:           HashMap<InodeId, BTreeMap<String, InodeId>>,
    pub next_ino:       AtomicU64,
    pub open_handles:   HashMap<u64, OpenHandleSt>,
    pub next_fh:        AtomicU64,
    pub nodes:          HashMap<NodeId, NodeInfo>,
    pub last_heartbeat: HashMap<NodeId, Instant>,
}

pub struct Inode {
    pub ino:            InodeId,
    pub kind:           InodeKind,
    pub mode:           u32,
    pub uid:            u32,
    pub gid:            u32,
    pub size:           u64,
    pub nlink:          u32,
    pub atime:          SystemTime,
    pub mtime:          SystemTime,
    pub ctime:          SystemTime,
    pub generation:     Generation,
    pub blocks:         BTreeMap<BlockIdx, BlockPlacement>,    // file only
    pub symlink_target: Option<String>,
}

pub struct OpenHandleSt {
    pub fh:            u64,
    pub ino:           InodeId,
    pub flags:         i32,
    pub generation_at_open: Generation,
    pub opener_node:   NodeId,
}

#[derive(Clone, Debug)]
pub struct NodeInfo {
    pub node_id:    NodeId,
    pub addr:       String,
    pub ram_used:   u64,
    pub ram_total:  u64,
    pub status:     NodeStatus,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NodeStatus { Up, Down }
```

### 4.1.1 Locking strategy

A single `Arc<RwLock<MetaState>>` for v1. Every read RPC takes a read lock,
every mutating RPC takes a write lock. Justification:

- The hot path under the lock is microseconds (HashMap lookup, a couple of
  Vec inserts). A single lock is cheap at the v1 scale of ≤8 clients × ≤8
  stores producing perhaps 10k metadata ops/sec.
- A `tokio::sync::RwLock` (not the std one) is used because handlers `.await`
  while holding the lock only across pure CPU work — no I/O — but the futures
  passed to tonic must remain `Send`, which `tokio::sync` guarantees and
  `std::sync` does not after `.read().await`.
- When profiling shows lock contention, shard by `ino % N` into N independent
  `RwLock`s. The data structure already partitions cleanly: `inodes` and
  `dirs` are keyed by inode, `open_handles` by fh, `nodes`/`last_heartbeat` by
  node id (a separate lock).

### 4.2 Inode allocation

```rust
impl MetaState {
    pub fn alloc_ino(&self) -> InodeId {
        // 0 is reserved (FUSE convention), 1 is root.
        let next = self.next_ino.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        debug_assert!(next >= 2);
        InodeId(next)
    }
}
```

`next_ino` is initialized to 2 in `MetaState::new()`. `AtomicU64::fetch_add`
returns the previous value, so the first call returns 2. Wraparound at
2^64 / 10^9 ops/sec is ≈585 years — not a concern.

File handles use the same pattern with `next_fh` starting at 1.

### 4.3 Block allocation algorithm

```rust
pub fn allocate_blocks(
    &self,
    _ino: InodeId,
    idxs: &[BlockIdx],
    r: usize,
) -> Vec<(BlockIdx, BlockPlacement)> {
    use dtmpfs_common::hash::pick_nodes;
    use dtmpfs_common::id::{BlockKey, Generation};

    let live: Vec<NodeId> = self.nodes.iter()
        .filter(|(_, ni)| ni.status == NodeStatus::Up)
        .map(|(id, _)| id.clone())
        .collect();

    idxs.iter().map(|&idx| {
        let key = BlockKey {
            ino: _ino,
            block_idx: idx,
            generation: Generation(0),   // see note below
        };
        let chosen = pick_nodes(&key, &live, r);
        let placement = BlockPlacement {
            primary: chosen.first().cloned().expect("live nodes"),
            replicas: chosen.into_iter().skip(1).collect(),
        };
        (idx, placement)
    }).collect()
}
```

**Why `generation: Generation(0)` for placement**: HRW must be stable
across rewrites of the same block. If the hash key included `generation`, a
client that bumped from gen 5 to gen 6 would pick a different primary, and
the cached `block_map` from `Open` would no longer be authoritative. The
generation field of `BlockKey` exists for the **store's** stale-write
rejection (`§5.3`); for **placement**, we zero it.

### 4.4 Generation bumping (Meta.Close)

```rust
pub async fn close_handler(
    state: &Arc<RwLock<MetaState>>,
    req: CloseReq,
) -> Result<CloseResp, DtmpfsError> {
    // CloseReq is the canonical wire shape from protocol.md / decisions.md R6:
    //   { fh, ino, expected_generation, new_size, mtime_s, mtime_ns,
    //     written_block_idxs }
    // Allocations were already committed inside `Meta.AllocateBlocks`; the
    // close path does NOT carry placements. The new generation rides back
    // inside `attr.generation` — no separate `new_generation` field.
    let mut s = state.write().await;
    let oh = s.open_handles.remove(&req.fh).ok_or(DtmpfsError::NotFound)?;
    let inode = s.inodes.get_mut(&oh.ino).ok_or(DtmpfsError::NotFound)?;
    let dirty = !req.written_block_idxs.is_empty();
    if dirty {
        // Stale-close detection: refuse if some other client published a
        // newer generation since this fh was opened, except in the
        // disjoint-blocks case (see consistency.md §5.3).
        if inode.generation != Generation(req.expected_generation) {
            return Err(DtmpfsError::Rpc(
                tonic::Status::failed_precondition("close: stale generation")));
        }
        inode.generation = inode.generation.bump();
        inode.size       = req.new_size;
        inode.mtime      = SystemTime::now();
        inode.ctime      = inode.mtime;
        // Placements installed earlier via AllocateBlocks; nothing to merge here.
    }
    Ok(CloseResp { attr: build_attr(inode) })
}
```

If the client never wrote (read-only `open`), no generation bump — keeps
caches valid for other clients.

### 4.5 Membership / heartbeat

```rust
// In dtmpfs-meta/src/heartbeat.rs

pub fn spawn_watcher(state: Arc<RwLock<MetaState>>, dead_after: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let now = Instant::now();
            let mut s = state.write().await;
            let mut downs = Vec::new();
            for (id, last) in s.last_heartbeat.iter() {
                if now.duration_since(*last) > dead_after {
                    downs.push(id.clone());
                }
            }
            for id in downs {
                if let Some(ni) = s.nodes.get_mut(&id) {
                    if ni.status != NodeStatus::Down {
                        tracing::warn!(?id, "node marked Down (no heartbeat)");
                        ni.status = NodeStatus::Down;
                    }
                }
            }
        }
    });
}
```

`Meta.HeartbeatNode` handler simply updates `last_heartbeat[node] =
Instant::now()`, sets status to `Up`, and records the latest `ram_used` /
`ram_total` from the message body. The watcher demotes silent nodes once a
second.

The block_map served on subsequent `Open` calls reflects only `Up` nodes, so
new files never get placed on a Down node. Existing inodes keep their
recorded placement unchanged; a read against a Down node returns `EIO`
(R=1) or transparently fails over to a replica (R≥2), per
`docs/failure-model.md`.

### 4.6 Operation handlers

Pseudocode for each `Meta` RPC. Real signatures use the tonic-generated
request/response types.

**`Lookup(parent_ino, name) -> Attr`**

```
read lock
let dir = dirs.get(parent_ino).ok_or(NotADirectory)?;
let ino = dir.get(name).ok_or(NotFound)?;
let inode = inodes.get(ino).ok_or(NotFound)?;
return build_attr(inode);
```

**`GetAttr(ino) -> Attr`**

```
read lock; inodes.get(ino) -> build_attr or NotFound.
```

**`SetAttr(ino, mask, ...) -> Attr`**

```
write lock
mutate fields under mask {mode, uid, gid, size, atime, mtime}
if size shrinks:
    last_valid_idx = (new_size - 1) / BLOCK_SIZE   (0 if new_size == 0)
    dropped = inode.blocks.drain(last_valid_idx+1 ..)
    collect node_addrs from state.nodes (still under lock)
    fire-and-forget DeleteBlock for each (node, dropped block)
inode.size = new_size; inode.ctime = now
return build_attr
```

**`Create(parent_ino, name, mode) -> (Attr, fh)`**

```
write lock
let dir = dirs.get_mut(parent_ino).ok_or(NotADirectory)?;
if dir.contains_key(name) { return AlreadyExists; }
let ino = alloc_ino();
let inode = Inode { ino, kind: File, mode, ..now() };
inodes.insert(ino, inode);
dir.insert(name.into(), ino);
let fh = next_fh.fetch_add(1, ...);
open_handles.insert(fh, OpenHandleSt { fh, ino, flags, generation_at_open: 0, opener_node });
return (build_attr, fh);
```

**`Mkdir(parent_ino, name, mode) -> Attr`**

```
write lock
ensure no collision; alloc_ino; insert as Dir;
dirs.insert(ino, BTreeMap::new());          // empty children map
parent's nlink += 1 (".." link from new dir)
```

**`Unlink(parent_ino, name) -> ()`**

```
write lock
let ino = dirs.get_mut(parent_ino).remove(name);
let inode = inodes.remove(ino);
if inode.kind == Dir { return IsADirectory; }
// collect addresses from state.nodes before releasing the lock
let node_addrs = state.nodes.values().map(|n| (n.node_id, "http://"+n.addr)).collect();
// fire-and-forget: one tokio::spawn per (node, block) pair
for (idx, placement) in inode.blocks {
    for node in [primary] + replicas {
        spawn(async { store_client_with_cluster_token.delete_block(ino, idx, gen=0) });
    }
}
```

`DeleteBlock` is fire-and-forget — failure leaks store RAM but does not affect correctness.
`MetaService` carries a `token: String` field so outgoing `DeleteBlock` RPCs can attach the
`cluster-token` header. The same pattern applies to `SetAttr` (truncate-shrink) and to
`Rename` when it atomically overwrites an existing file destination.

**`Rmdir(parent_ino, name) -> ()`**

```
write lock
let ino = dirs.get(parent_ino).ok_or(NotADirectory)?
                .get(name).ok_or(NotFound)?;
let child_dir = dirs.get(ino).ok_or(NotADirectory)?;
if !child_dir.is_empty() { return NotEmpty; }
dirs.remove(ino);
inodes.remove(ino);
dirs.get_mut(parent_ino).unwrap().remove(name);
parent's nlink -= 1.
```

**`Rename(src_parent, src_name, dst_parent, dst_name) -> ()`**

```
write lock
let src_ino = dirs.get(src_parent)?.get(src_name).copied().ok_or(NotFound)?;
let collision = dirs.get(dst_parent)?.get(dst_name).copied();
match collision {
    None => move,
    Some(dst_ino) if dst_ino == src_ino => return Ok(()),       // same file
    Some(dst_ino) => {
        // POSIX rename(2) atomically replaces dst.
        if dst is dir and not empty: return NotEmpty;
        if (src is dir) != (dst is dir): return IsADirectory or NotADirectory;
        // collect dst file blocks and node addrs; fire-and-forget DeleteBlock after move
        unlink dst (same block GC as Unlink);
    }
}
dirs.get_mut(src_parent)?.remove(src_name);
dirs.get_mut(dst_parent)?.insert(dst_name, src_ino);
inodes.get_mut(src_ino)?.ctime = now();
```

Block placements are **not** rewritten on rename — placement is per-(ino, idx)
and the inode is unchanged. See `§12` open question 1.

**`ReadDir(ino, offset, max) -> Vec<DirEnt>`**

```
read lock
let dir = dirs.get(ino).ok_or(NotADirectory)?;
let entries: Vec<_> = dir.iter()
    .skip(offset as usize)
    .take(max.unwrap_or(256) as usize)
    .map(|(name, &child_ino)| build_dirent(name, child_ino, kind))
    .collect();
return ReadDirResp { entries, next_cookie: offset + entries.len() };
```

The cookie is the integer offset into the BTreeMap iteration. BTreeMap
iteration is sorted, so cookies are stable across readdir calls *as long as
the directory is not mutated mid-stream*. Concurrent insert/delete may cause
skip-or-double-list — POSIX `readdir(3)` permits this.

**`Open(ino, flags) -> OpenResp { attr, fh, block_map }`**

```
read lock for inode metadata; write lock for handle insert
let inode = inodes.get(ino).ok_or(NotFound)?;
if inode.kind != File { return IsADirectory; }
let block_map = inode.blocks.clone();   // BTreeMap<BlockIdx, BlockPlacement>
let fh = next_fh.fetch_add(1, ...);
open_handles.insert(fh, OpenHandleSt {
    fh, ino,
    flags,
    generation_at_open: inode.generation,
    opener_node,
});
return OpenResp { attr: build_attr(inode), fh, block_map };
```

**`Close(fh, ino, expected_generation, new_size, mtime_s, mtime_ns, written_block_idxs) -> CloseResp`** —
see §4.4.

**`AllocateBlocks(ino, idxs) -> Vec<BlockLoc>`** — see §4.3.

**`ListNodes() -> NodeList`**

```
read lock
return nodes.values().filter(|ni| ni.status == Up)
    .map(|ni| NodeEntry { id: ni.node_id, addr: ni.addr, ram_used, ram_total })
    .collect()
```

---

## 5. `dtmpfs-store` internals

### 5.1 State

```rust
use bytes::Bytes;
use dashmap::DashMap;
use std::sync::atomic::AtomicU64;
use dtmpfs_common::id::{BlockIdx, Generation, InodeId, NodeId};

pub struct StoreState {
    // Per decisions.md R7: keyed by the 3-tuple BlockKey { ino, block_idx,
    // generation }, NOT by (ino, block_idx). Two open generations of the
    // same logical block can be live at once (writer-on-A still holding
    // gen-N while gen-N+1 has been published); separate keys let the store
    // retain both until GC reaps the older one.
    pub blocks:    DashMap<BlockKey, Bytes>,
    pub node_id:   NodeId,
    pub meta_addr: String,
    pub ram_budget: u64,
    pub ram_used:   AtomicU64,
}
```

#### Why `DashMap`

- Sharded internally (default 64 shards): concurrent readers and writers on
  different keys never contend.
- `entry`, `get`, `remove` all return guards that drop quickly — no awaiting
  while holding shard locks.
- Lock-free reads on the hot path matter: a single store services many
  parallel `ReadBlock` calls from many clients.

#### Why `Bytes`

- `Bytes::clone()` is a refcount bump, not a copy. The store hands out
  clones on every read; the data stays in one allocation until the last
  reader drops it.
- Tonic's prost-generated code natively accepts `bytes::Bytes` for `bytes`
  proto fields when `tonic_build::configure().bytes(["."])` is set, which
  avoids one extra copy on the wire path.
- Read-once-write-once semantics: a block is written by `WriteBlock`, then
  cloned and emitted on every `ReadBlock`. `Bytes` is exactly this pattern.

### 5.2 Read path

```rust
pub async fn read_block(
    state: Arc<StoreState>,
    req: ReadBlockReq,
) -> Result<ReadBlockResp, tonic::Status> {
    let proto = req.key.ok_or_else(|| tonic::Status::invalid_argument("key"))?;
    let key = BlockKey {
        ino:        InodeId(proto.ino),
        block_idx:  BlockIdx(proto.block_idx),
        generation: Generation(proto.generation),
    };
    let g = state.blocks.get(&key)
        .ok_or_else(|| tonic::Status::not_found("block"))?;
    // Bytes::clone is a refcount bump.
    Ok(ReadBlockResp {
        data: g.value().clone().to_vec(),
        len:  g.value().len() as u32,
    })
}
```

The store keys reads/writes by the full 3-tuple `BlockKey`. The client
always issues `ReadBlock` with the generation it observed at `Open` time,
so the lookup is a direct hit on the live entry; older generations of the
same `(ino, block_idx)` may still sit in the map until GC, but they live
under a different key.

### 5.3 Write path

Per `decisions.md` R2, **stale-write rejection on the store is Phase 6 work**.
v1 stores accept `WriteBlock` blindly: each call inserts the payload at its
3-tuple `BlockKey { ino, block_idx, generation }` (R7) and returns success.
The wire field `BlockKey.generation` is preserved on the wire and in the
DashMap key so the field exists for Phase 6 hardening; v1 just does not
consult it for a freshness comparison.

```rust
pub async fn write_block(
    state: Arc<StoreState>,
    req: WriteBlockReq,
) -> Result<WriteBlockResp, tonic::Status> {
    let key_proto = req.key.ok_or_else(|| tonic::Status::invalid_argument("key"))?;
    let key = BlockKey {
        ino:        InodeId(key_proto.ino),
        block_idx:  BlockIdx(key_proto.block_idx),
        generation: Generation(key_proto.generation),
    };
    let data    = Bytes::from(req.data);
    let len_u64 = data.len() as u64;

    // Budget check (still v1).
    if state.ram_used.load(Ordering::Relaxed) + len_u64 > state.ram_budget {
        return Err(tonic::Status::resource_exhausted("ram budget"));
    }

    // v1: blind insert. Phase 6 will add a per-(ino, block_idx) high-water
    // generation check and return FailedPrecondition for stale writes.
    let prev_len = state.blocks
        .insert(key, data)
        .map(|b| b.len() as u64)
        .unwrap_or(0);
    state.ram_used.fetch_add(len_u64, Ordering::Relaxed);
    state.ram_used.fetch_sub(prev_len, Ordering::Relaxed);
    Ok(WriteBlockResp { len: len_u64 as u32 })
}
```

**Phase 6 plan**: maintain a `DashMap<(InodeId, BlockIdx), Generation>` of
the highest-seen generation per logical block; a `WriteBlock` whose
`BlockKey.generation` is strictly less is rejected with
`Status::failed_precondition("stale generation")`. This guards against the
network-stuck-zombie-client scenario from `consistency.md` §5.5; v1 papers
over it with the meta-side stale-close check (`Meta.Close` rejects a stale
`expected_generation`) instead.

### 5.4 Heartbeat client

```rust
pub fn spawn_heartbeat(state: Arc<StoreState>, advertise_addr: String, token: String) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut client = match MetaClient::connect(state.meta_addr.clone()).await {
            Ok(c) => c,
            Err(e) => { tracing::error!(?e, "initial meta connect failed"); return; }
        };
        loop {
            tick.tick().await;
            let req = HeartbeatReq {
                node_id:   state.node_id.0.clone(),
                addr:      advertise_addr.clone(),
                ram_used:  state.ram_used.load(Ordering::Relaxed),
                ram_total: state.ram_budget,
            };
            let mut rpc = tonic::Request::new(req);
            rpc.metadata_mut().insert(
                "cluster-token",
                token.parse().expect("token is valid ascii"),
            );
            if let Err(e) = client.heartbeat_node(rpc).await {
                tracing::warn!(?e, "heartbeat failed");
                // Reconnect lazily — tonic auto-reconnects on next call.
            }
        }
    });
}
```

Never `panic!` on heartbeat failure; the meta is allowed to be transiently
unreachable. Once reachable again, the store re-registers via the next
heartbeat.

### 5.5 RAM budget enforcement

The check-then-insert in §5.3 is racy under concurrent writers (two writers
might each see budget allowing them and then both succeed, busting the
budget by one block). Acceptable for v1: the over-budget is bounded by
`max_concurrent_writers × block_size`. Phase 6 hardens this by moving the
budget check inside the DashMap entry guard.

`DeleteBlock` is the only path that decrements `ram_used`:

```rust
pub async fn delete_block(state: Arc<StoreState>, req: DeleteBlockReq)
    -> Result<Empty, tonic::Status> {
    let proto = req.key.ok_or_else(|| tonic::Status::invalid_argument("key"))?;
    let key = BlockKey {
        ino:        InodeId(proto.ino),
        block_idx:  BlockIdx(proto.block_idx),
        generation: Generation(proto.generation),
    };
    if let Some((_, prev)) = state.blocks.remove(&key) {
        state.ram_used.fetch_sub(prev.len() as u64, Ordering::Relaxed);
    }
    Ok(Empty {})
}
```

### 5.6 `/debug/blocks` HTTP affordance

Mounted via axum on a separate port from the gRPC server. Gated by
`StoreConfig::debug_http_listen = Some("...")`; if `None`, the endpoint is not
spawned.

```rust
pub fn spawn_debug_http(state: Arc<StoreState>, bind: String) {
    use axum::{routing::get, Router, Json};
    use serde_json::json;

    let app = Router::new().route("/debug/blocks", get(move || {
        let state = state.clone();
        async move {
            let mut blocks = Vec::with_capacity(state.blocks.len());
            for entry in state.blocks.iter() {
                let (key, v) = (entry.key(), entry.value());
                blocks.push(json!({
                    "ino": key.ino.0,
                    "idx": key.block_idx.0,
                    "gen": key.generation.0,
                    "len": v.len(),
                }));
            }
            Json(json!({
                "count":    blocks.len(),
                "ram_used": state.ram_used.load(Ordering::Relaxed),
                "blocks":   blocks,
            }))
        }
    }));
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&bind).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });
}
```

Used by `tests/integration/sharding.sh` to confirm blocks split roughly
evenly across stores. **Not** for production — it iterates the entire map.

---

## 6. `dtmpfs-client` internals — the FUSE filesystem

### 6.1 Top-level types

```rust
use std::collections::BTreeMap;
use std::sync::Arc;
use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use moka::sync::Cache;
use arc_swap::ArcSwap;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tonic::transport::Channel;

use dtmpfs_common::id::{BlockIdx, BlockPlacement, Generation, InodeId, NodeId};
use dtmpfs_proto::meta::meta_client::MetaClient;
use dtmpfs_proto::store::store_client::StoreClient;

pub struct DtmpfsFs {
    pub rt:           Handle,
    pub meta:         Mutex<MetaClient<Channel>>,
    pub stores:       Arc<StoreClientPool>,
    pub attr_cache:   Cache<InodeId, CachedAttr>,
    pub block_cache:  Cache<(InodeId, Generation, BlockIdx), Bytes>,
    pub open_files:   DashMap<u64, Arc<Mutex<OpenFile>>>,
    pub block_size:   usize,
    pub replication_factor: usize,
}

pub struct CachedAttr {
    pub attr:    fuser::FileAttr,
    pub fetched: std::time::Instant,
}

pub struct OpenFile {
    pub ino:        InodeId,
    pub generation: Generation,
    pub block_map:  BTreeMap<BlockIdx, BlockPlacement>,
    pub dirty:      BTreeMap<BlockIdx, BytesMut>,
    pub size_hint:  u64,
    pub flags:      i32,
}

pub struct StoreClientPool {
    pub clients: DashMap<NodeId, StoreClient<Channel>>,
    pub addrs:   ArcSwap<std::collections::HashMap<NodeId, String>>,
}

impl StoreClientPool {
    pub async fn get(&self, id: &NodeId) -> Result<StoreClient<Channel>, DtmpfsError> {
        if let Some(c) = self.clients.get(id) {
            return Ok(c.clone());
        }
        let addrs = self.addrs.load();
        let addr  = addrs.get(id).ok_or_else(|| DtmpfsError::StoreUnavailable(id.clone()))?;
        let chan  = Channel::from_shared(addr.clone())
            .map_err(|_| DtmpfsError::StoreUnavailable(id.clone()))?
            .connect().await
            .map_err(|_| DtmpfsError::StoreUnavailable(id.clone()))?;
        let client = StoreClient::new(chan);
        self.clients.insert(id.clone(), client.clone());
        Ok(client)
    }

    pub fn refresh_addrs(&self, m: std::collections::HashMap<NodeId, String>) {
        self.addrs.store(Arc::new(m));
    }
}
```

`StoreClient<Channel>` clones cheaply (it's a thin wrapper around an
`Arc<Channel>`), so the DashMap stores instances rather than connection
configs. The `addrs` swap is lock-free via `arc-swap` — membership churn
does not stall the data path.

### 6.2 The async bridge

The client builds **one** multi-threaded tokio runtime in `main()`:

```rust
fn main() -> anyhow::Result<()> {
    let cfg = load_config()?;
    init_tracing();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num_cpus::get().max(2))
        .enable_all()
        .build()?;
    let handle = rt.handle().clone();
    let fs = rt.block_on(async {
        build_dtmpfs_fs(handle, &cfg).await
    })?;
    let opts = mount_options(&cfg);
    fuser::mount2(fs, &cfg.mount_point, &opts)?;     // blocks here
    Ok(())
}
```

Every `fuser::Filesystem` callback bridges into async via
`self.rt.block_on(async { ... })`. The fuser worker threads are
**not** tokio worker threads — they are libfuse threads dispatched by
`fuser::Session`. Calling `block_on` on a non-tokio thread is supported and
documented (see `tokio::runtime::Handle::block_on`).

### 6.2.1 Worked example: `read`

```rust
impl fuser::Filesystem for DtmpfsFs {
    fn read(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyData,
    ) {
        let span = tracing::info_span!("client.read", ino, fh, offset, size);
        let _e = span.enter();

        let res: Result<Vec<u8>, DtmpfsError> = self.rt.block_on(async {
            let of_arc = self.open_files.get(&fh)
                .ok_or(DtmpfsError::NotFound)?
                .clone();
            let of = of_arc.lock().await;

            let bs = self.block_size as u64;
            let start = offset as u64;
            let end   = start + size as u64;     // exclusive
            let first_idx = BlockIdx(start / bs);
            let last_idx  = BlockIdx((end.saturating_sub(1)) / bs);

            let mut out = Vec::with_capacity(size as usize);
            for i in first_idx.0 ..= last_idx.0 {
                let idx = BlockIdx(i);
                let block = self.fetch_block(&of, idx).await?;
                let block_off = (i * bs) as u64;
                let in_block_start = (start.saturating_sub(block_off)) as usize;
                let in_block_end   = (end.min(block_off + bs) - block_off) as usize;
                out.extend_from_slice(&block[in_block_start..in_block_end]);
            }
            Ok(out)
        });

        match res {
            Ok(buf) => reply.data(&buf),
            Err(e)  => {
                tracing::warn!(?e, "read failed");
                reply.error(libc::c_int::from(e));
            }
        }
    }
}

impl DtmpfsFs {
    async fn fetch_block(&self, of: &OpenFile, idx: BlockIdx)
        -> Result<Bytes, DtmpfsError>
    {
        // 1. Dirty buffer wins.
        if let Some(buf) = of.dirty.get(&idx) {
            return Ok(Bytes::copy_from_slice(buf));
        }
        // 2. Block cache.
        if let Some(b) = self.block_cache.get(&(of.ino, of.generation, idx)) {
            return Ok(b);
        }
        // 3. Remote fetch from primary, fall back to replicas (R>=2 only).
        // Lazily refresh store address pool on first read (e.g. after client restart).
        if self.stores.addrs.load().is_empty() {
            if let Ok(resp) = self.meta.lock().await.list_nodes(self.authed(Empty {})).await {
                let m = resp.into_inner().nodes.iter()
                    .map(|n| (NodeId(n.node_id.clone()), format!("http://{}", n.addr)))
                    .collect();
                self.stores.refresh_addrs(m);
            }
        }
        let placement = of.block_map.get(&idx)
            .ok_or(DtmpfsError::NotFound)?;
        let mut tried: Vec<NodeId> = Vec::new();
        for node in std::iter::once(&placement.primary).chain(placement.replicas.iter()) {
            tried.push(node.clone());
            let mut client = match self.stores.get(node).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            // generation is always 0 on the wire: the store is a latest-overwrite map
            // keyed by (ino, block_idx, 0). Inode generation is used only for the
            // client-side BlockCache key to invalidate stale entries after Meta.Close.
            let req = ReadBlockReq {
                ino: of.ino.0, block_idx: idx.0, generation: 0,
            };
            match client.read_block(req).await {
                Ok(resp) => {
                    let b = Bytes::from(resp.into_inner().data);
                    self.block_cache.insert((of.ino, of.generation, idx), b.clone());
                    return Ok(b);
                }
                Err(s) if s.code() == tonic::Code::NotFound => {
                    // Block was unlinked or never written; treat as zeros
                    // (sparse-file semantics).
                    let z = Bytes::from(vec![0u8; self.block_size]);
                    return Ok(z);
                }
                Err(_) => continue,
            }
        }
        Err(DtmpfsError::StoreUnavailable(tried.last().cloned().unwrap_or_default()))
    }
}
```

### 6.3 BlockCache / AttrCache

```rust
fn build_attr_cache(cfg: &ClientConfig) -> Cache<InodeId, CachedAttr> {
    Cache::builder()
        .max_capacity(100_000)
        .time_to_live(std::time::Duration::from_millis(cfg.attr_cache_ttl_ms))
        .build()
}

fn build_block_cache(cfg: &ClientConfig)
    -> Cache<(InodeId, Generation, BlockIdx), Bytes>
{
    let cap_bytes = cfg.block_cache_capacity_mb * 1024 * 1024;
    Cache::builder()
        .max_capacity(cap_bytes)
        .weigher(|_k, v: &Bytes| v.len() as u32)
        .build()
}
```

Cache key for blocks deliberately includes `Generation`. After a generation
bump, looking up `(ino, gen_old, idx)` continues to return the old data —
which is fine because no live `OpenFile` is using `gen_old` anymore (every
new `open` rebuilds at the new generation). Old entries fall out
naturally as the LRU evicts them; no explicit invalidation is required.

Optional eager-eviction hook (used only at flush time):

```rust
self.block_cache.invalidate_entries_if(move |k, _v| {
    k.0 == ino && k.1 < new_generation
}).ok();
```

Provided to release memory aggressively; correctness does not depend on it.

### 6.4 Read-modify-write for partial writes

```rust
impl DtmpfsFs {
    async fn apply_write(
        &self, of: &mut OpenFile,
        offset: u64, data: &[u8],
    ) -> Result<u32, DtmpfsError> {
        let bs = self.block_size as u64;
        let mut written = 0u32;
        let mut cur = offset;
        let end = offset + data.len() as u64;
        while cur < end {
            let idx = BlockIdx(cur / bs);
            let block_off = idx.0 * bs;
            let in_block = (cur - block_off) as usize;
            let chunk_len = ((block_off + bs).min(end) - cur) as usize;

            // Ensure dirty buffer exists.
            if !of.dirty.contains_key(&idx) {
                let init = if in_block == 0 && chunk_len == self.block_size {
                    // fully overwritten -> fresh zeroed buffer; will be fully
                    // overwritten by the copy below
                    BytesMut::zeroed(self.block_size)
                } else if of.block_map.contains_key(&idx) {
                    // partially overwritten and exists remotely; fetch RMW base
                    let b = self.fetch_block(of, idx).await?;
                    let mut bm = BytesMut::with_capacity(self.block_size);
                    bm.extend_from_slice(&b);
                    if bm.len() < self.block_size { bm.resize(self.block_size, 0); }
                    bm
                } else {
                    // brand-new block (extending file or sparse hole)
                    BytesMut::zeroed(self.block_size)
                };
                of.dirty.insert(idx, init);
            }

            let buf = of.dirty.get_mut(&idx).unwrap();
            let src = &data[written as usize .. written as usize + chunk_len];
            buf[in_block .. in_block + chunk_len].copy_from_slice(src);

            written += chunk_len as u32;
            cur += chunk_len as u64;
        }
        of.size_hint = of.size_hint.max(end);
        Ok(written)
    }
}
```

Newly-allocated blocks (those with `idx >= ceil(old_size/B)`) are tracked
implicitly: at flush time we compare `of.dirty.keys()` against
`of.block_map.keys()` and the difference is the set requiring
`AllocateBlocks`. See §6.5.

**Implementation note**: every dirty block is exactly `block_size` bytes
(zero-padded to the right). The actual file size is recorded on `Close` as
`size_hint`. Storing fixed-size blocks simplifies the RMW logic at the cost
of some wasted bytes for files whose size is not a multiple of `block_size`
— acceptable at 1 MiB blocks, where the worst-case waste is one block per
file.

### 6.5 Flush and fsync algorithm

Per `decisions.md` D3 and R13, the v1 client splits the close-time flush
path from the explicit-`fsync(2)` path on the **replica-wait policy only**:

- `flush_path()` — called from FUSE `flush`/`release`. Waits for **primary**
  acks, then calls `Meta.Close`. Replicas may still be in flight.
- `fsync_path()` — called from FUSE `fsync`/`fsyncdir`. Waits for **all**
  replicas (when `R≥2`), then calls `Meta.Close`. With `R=1` the two paths
  are identical.

Both paths share the per-block write fan-out, the `Meta.AllocateBlocks`
step for new indices, and the close-time generation bump. The difference
is one `WaitPolicy` argument fed to the inner stream.

```rust
#[derive(Copy, Clone, PartialEq, Eq)]
enum WaitPolicy { PrimariesOnly, AllReplicas }

pub async fn flush_path(&self, fh: u64) -> Result<(), DtmpfsError> {
    self.publish(fh, WaitPolicy::PrimariesOnly).await
}
pub async fn fsync_path(&self, fh: u64) -> Result<(), DtmpfsError> {
    self.publish(fh, WaitPolicy::AllReplicas).await
}

async fn publish(&self, fh: u64, wait: WaitPolicy) -> Result<(), DtmpfsError> {
    let of_arc = self.open_files.get(&fh)
        .ok_or(DtmpfsError::NotFound)?.clone();
    let mut of = of_arc.lock().await;
    if of.dirty.is_empty() {
        return Ok(());
    }

    // 1. Find any blocks not yet in block_map (newly-allocated).
    let new_idxs: Vec<BlockIdx> = of.dirty.keys()
        .filter(|i| !of.block_map.contains_key(i))
        .copied().collect();

    if !new_idxs.is_empty() {
        let req = AllocReq { ino: of.ino.0, block_idxs: new_idxs.iter().map(|i| i.0).collect() };
        let resp = self.meta.lock().await.allocate_blocks(req).await
            .map_err(|s| DtmpfsError::from_status(s, None))?;
        for loc in resp.into_inner().placements {
            of.block_map.insert(BlockIdx(loc.block_idx), BlockPlacement {
                primary:  NodeId(loc.primary),
                replicas: loc.replicas.into_iter().map(NodeId).collect(),
            });
        }
    }

    // 2. Drain dirty buffers and write them in parallel.
    use futures::stream::{StreamExt, TryStreamExt};
    let dirty = std::mem::take(&mut of.dirty);
    let ino = of.ino;
    let gen = of.generation;
    let stores = self.stores.clone();
    let block_map = of.block_map.clone();
    let written_idxs: Vec<BlockIdx> = dirty.keys().copied().collect();

    futures::stream::iter(dirty.into_iter())
        .map(move |(idx, buf)| {
            let placement = block_map.get(&idx).cloned()
                .ok_or(DtmpfsError::NotFound);
            let stores = stores.clone();
            async move {
                let placement = placement?;
                let frozen = buf.freeze();
                // generation: 0 — store is a latest-overwrite map; inode generation
                // is used only for client-side cache invalidation, not as a store key.
                let key = BlockKey { ino, block_idx: idx, generation: Generation(0) };
                let make_write = |n: NodeId| {
                    let stores = stores.clone();
                    let frozen = frozen.clone();
                    async move {
                        let mut client = stores.get(&n).await?;
                        let req = WriteBlockReq {
                            key:  Some(BlockKeyProto::from(key)),
                            data: frozen.to_vec(),
                        };
                        client.write_block(req).await
                            .map_err(|s| DtmpfsError::from_status(s, Some(n.clone())))?;
                        Ok::<_, DtmpfsError>(())
                    }
                };
                // Always wait for the primary.
                make_write(placement.primary.clone()).await?;
                // Replica wait depends on the path (decisions.md R13).
                let replica_writes = placement.replicas.iter()
                    .cloned().map(make_write);
                match wait {
                    WaitPolicy::AllReplicas => {
                        futures::future::try_join_all(replica_writes).await?;
                    }
                    WaitPolicy::PrimariesOnly => {
                        // Spawn replicas best-effort; do not block close on them.
                        for w in replica_writes {
                            tokio::spawn(async move { let _ = w.await; });
                        }
                    }
                }
                Ok::<_, DtmpfsError>(())
            }
        })
        .buffer_unordered(16)
        .try_collect::<Vec<()>>()
        .await?;

    // 3. Tell meta we're done; capture the new generation from the returned attr.
    //    Per decisions.md R6: CloseReq carries `expected_generation` for stale-
    //    close detection and `written_block_idxs` (NOT `dirty_block_idxs` and
    //    NOT `allocated_blocks` — the placements were committed inside meta
    //    when the client called `Meta.AllocateBlocks` above). The new
    //    generation is read back from `attr.generation`, not a separate field.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let close_req = CloseReq {
        fh,
        ino: ino.0,
        expected_generation: gen.0,
        new_size: of.size_hint,
        mtime_s: now.as_secs() as i64,
        mtime_ns: now.subsec_nanos(),
        written_block_idxs: written_idxs.iter().map(|i| i.0).collect(),
    };
    let resp = self.meta.lock().await.close(close_req).await
        .map_err(|s| DtmpfsError::from_status(s, None))?
        .into_inner();
    of.generation = Generation(resp.attr.as_ref().map(|a| a.generation).unwrap_or(gen.0));

    // 4. Optional cache pruning.
    let new_gen = of.generation;
    let ino_copy = ino;
    self.block_cache.invalidate_entries_if(move |k, _| k.0 == ino_copy && k.1 < new_gen).ok();
    Ok(())
}
```

`buffer_unordered(16)` caps in-flight writes at 16 blocks (≈16 MiB at 1 MiB
blocks). Higher values saturate the LAN; lower values fail to overlap. Tunable.

Per the `WaitPolicy` split, replica writes either block close (`fsync_path`,
`AllReplicas`) or run best-effort in the background (`flush_path`,
`PrimariesOnly`). With `R=1` no replicas exist and the two paths collapse.
With `R≥2`, `fsync_path` close-time latency is `max(primary, slowest_replica)`;
`flush_path` close-time latency is just primary latency. The FUSE callbacks
route as: `flush`/`release` → `flush_path`, `fsync`/`fsyncdir` →
`fsync_path` (see §6.6).

### 6.6 FUSE method coverage

| Method        | Status      | Notes |
|---------------|-------------|-------|
| `init`        | implemented | Pre-warm meta connection; cache root attr. |
| `destroy`     | implemented | Drain `open_files` (best-effort flush). |
| `lookup`      | implemented | `Meta.Lookup`; AttrCache populate. |
| `forget`      | stub        | No-op; we hold no per-lookup refcounts. |
| `getattr`     | implemented | AttrCache then `Meta.GetAttr` on miss. |
| `setattr`     | implemented | `Meta.SetAttr`; truncate frees blocks via meta-side async DeleteBlock. |
| `readlink`    | implemented | Symlink target stored in `Inode.symlink_target`. |
| `mknod`       | partial     | File and FIFO only; block/char devices return `EPERM`. |
| `mkdir`       | implemented | `Meta.Mkdir`. |
| `unlink`      | implemented | `Meta.Unlink`. |
| `rmdir`       | implemented | `Meta.Rmdir`. |
| `symlink`     | implemented | `Meta.Create` with `kind=Symlink` + target. |
| `rename`      | implemented | `Meta.Rename`. |
| `link`        | not impl    | `EPERM`. Hardlinks are out of v1 scope. |
| `open`        | implemented | `Meta.Open`; populate `OpenFile`. |
| `read`        | implemented | dirty / cache / store; see §6.2.1. |
| `write`       | implemented | RMW into `OpenFile.dirty`. |
| `flush`       | implemented | `flush_path` (waits primaries; replicas best-effort with R≥2). |
| `release`     | implemented | `flush_path` + remove from `open_files`. |
| `fsync`       | implemented | `fsync_path` (waits all replicas with R≥2; otherwise identical). RAM-only — not a durability barrier. |
| `opendir`     | implemented | Cheap; allocates a readdir cookie. |
| `readdir`     | implemented | `Meta.ReadDir` paginated. |
| `releasedir`  | implemented | Drop the cookie. |
| `fsyncdir`    | stub        | Always Ok; metadata is already in meta's RAM. |
| `statfs`      | implemented | Sum `ListNodes` ram_total / ram_used. |
| `setxattr`    | not impl    | `ENOSYS`. |
| `getxattr`    | not impl    | `ENOSYS`. |
| `listxattr`   | not impl    | `ENOSYS`. |
| `removexattr` | not impl    | `ENOSYS`. |
| `access`      | implemented | `Meta.GetAttr` + permission check. (Defaults to relying on `DefaultPermissions` mount option, which makes the kernel do the check using attrs.) |
| `create`      | implemented | `Meta.Create` returns fh+attr. |
| `getlk`/`setlk`| not impl  | `ENOSYS`. POSIX locks are out of v1. |
| `bmap`        | not impl    | `ENOSYS`; we have no block device. |
| `fallocate`   | partial     | `FALLOC_FL_KEEP_SIZE` no-op; otherwise extend size, allocate blocks. |
| `lseek`       | implemented | `SEEK_SET/CUR/END` only; `SEEK_HOLE/DATA` return `ENOSYS`. |
| `copy_file_range` | not impl | Falls back to read+write in kernel. |

### 6.7 Mount setup

```rust
fn mount_options(cfg: &ClientConfig) -> Vec<fuser::MountOption> {
    use fuser::MountOption::*;
    vec![
        FSName("dtmpfs".into()),
        Subtype("dtmpfs".into()),
        DefaultPermissions,        // kernel enforces mode/uid/gid checks
        AllowOther,                // requires `user_allow_other` in /etc/fuse.conf
        AutoUnmount,               // unmount if process dies
        NoAtime,                   // skip atime updates; write-amp goes to zero
    ]
}
```

Justification:

- **`FSName`/`Subtype`** — appears in `mount` output and `df`; lets ops tell
  dtmpfs apart from other FUSE mounts.
- **`DefaultPermissions`** — moves access check into the kernel using the
  `getattr`-supplied mode/uid/gid. Without this we'd have to re-implement
  POSIX permission checks in every handler.
- **`AllowOther`** — the cluster is a shared host; mount as one user, allow
  reads from any. Requires `/etc/fuse.conf` edit (`user_allow_other`),
  documented in `docs/operations.md`.
- **`AutoUnmount`** — if `dtmpfs-mount` crashes, the kernel notices the
  `/dev/fuse` fd close and unmounts. Without this, a stale mount point
  blocks remount until manual `fusermount3 -u`.
- **`NoAtime`** — atime updates would otherwise generate one `Meta.SetAttr`
  per read.

Multithreaded mount: `fuser::MountOption::Threadable` controls
multithreaded dispatch; the actual worker count comes from
`fuser::MountOption::FuseThreadCount`. The default in `MountConfig` is set
to `cfg.fuse_threads` (default 4) which means up to 4 in-flight FUSE
callbacks. They all share one tokio runtime, so concurrent block fetches
parallelize naturally.

---

## 7. Threading model & runtime

```
                         dtmpfs-mount (one OS process)

           main thread                        kernel-side
           ┌──────────────────────┐         ┌────────────────────┐
           │ parse args, load cfg │         │ /dev/fuse fd       │
           │ build tokio runtime  │         │  ▲                 │
           │ build DtmpfsFs       │         │  │ syscalls         │
           │ fuser::mount2 ──────►│ blocks  │  │                 │
           └──────────┬───────────┘         └────────────────────┘
                      │ spawned by fuser
                      ▼
   ┌─────────────────────────────────────────────────────────┐
   │ fuser worker pool (4 threads, default)                  │
   │   each thread loops on /dev/fuse                        │
   │   for each request: dispatch -> Filesystem method       │
   │   method body: self.rt.block_on(async { ... })          │
   │     ───────────────┐                                    │
   │                    │ submit future via Handle           │
   │                    ▼                                    │
   │   ┌──────────────────────────────────────────────────┐  │
   │   │ tokio multi_thread runtime (N=num_cpus, ≥2)      │  │
   │   │   workers run async tonic RPCs over hyper        │  │
   │   │   blocking I/O is hyper's poll-based async       │  │
   │   │ ┌────────────────────────────────────────────┐   │  │
   │   │ │ tonic Channel (HTTP/2) -> meta             │   │  │
   │   │ │ tonic Channels (HTTP/2) -> stores (pool)   │   │  │
   │   │ └────────────────────────────────────────────┘   │  │
   │   └──────────────────────────────────────────────────┘  │
   └─────────────────────────────────────────────────────────┘
```

### 7.1 Why no deadlock

A single-threaded tokio runtime would deadlock here: the FUSE worker calls
`block_on(async { ... .await })` — that future contains awaits which need a
tokio worker thread to make progress. If there is exactly one tokio worker
**and** it is the FUSE worker, the await blocks the only thread that could
have completed it. Multi-thread runtime breaks the cycle: tokio has its own
workers separate from fuser's.

### 7.2 Why no starvation

If all 4 fuser workers are simultaneously inside `block_on`, that's fine —
each `block_on` consumes one fuser thread, but tokio workers are entirely
separate (typically ≥4 on modern hardware). The bottleneck becomes either
the store RPC throughput (network) or the meta lock (serialized metadata
ops), neither of which is cured by adding more fuser workers.

Increasing `fuse_threads` past `num_cpus` yields diminishing returns; 4 is
the sweet spot for an 8-core box per single-machine benchmarking.

### 7.3 The meta server's runtime

Meta uses one `tokio::runtime::Builder::new_multi_thread()` runtime, no
fuser involvement. Tonic spawns one task per inbound stream. The
`RwLock<MetaState>` is `tokio::sync::RwLock`, not `std::sync` — its lock
guards are `Send`, so a handler can `.await` an outbound DeleteBlock RPC
while holding the lock if it has to (it tries not to: see §4.6 Unlink).

### 7.4 The store's runtime

Same shape as meta. The block table is a `DashMap` so request-handling
tasks rarely contend; gRPC's per-stream task model gives us per-request
concurrency for free.

---

## 8. Error handling end-to-end

Trace: store crashes mid-read of inode 42, block 3.

1. Client `DtmpfsFs::read` calls `self.fetch_block(of, BlockIdx(3))`.
2. `fetch_block` finds `(ino=42, gen=7, idx=3)` is not in dirty/block_cache.
3. It looks up `of.block_map[3].primary = NodeId("store-1")`.
4. `self.stores.get(&store-1).await` returns the cached `StoreClient`.
5. `client.read_block(...)` issues a `tonic` RPC over the cached channel.
6. The channel's HTTP/2 connection sees the TCP RST (or read timeout).
7. tonic returns `Err(Status { code: Unavailable, .. })`.
8. The match arm transforms it via:

```rust
match client.read_block(req).await {
    Err(s) if s.code() == tonic::Code::Unavailable => {
        // try next replica (R>=2) or fall through
        continue;
    }
    ...
}
```

If R=1 there are no replicas; the loop exits with no `return Ok(...)`.
9. `fetch_block` returns
   `Err(DtmpfsError::StoreUnavailable(NodeId("store-1")))`.
10. `read` calls `libc::c_int::from(e)` which matches `StoreUnavailable(_)
   => libc::EIO`.
11. `reply.error(libc::EIO)` sends to the kernel.
12. The application's `read(2)` syscall returns `-1` with `errno = EIO`.

The `From` impl:

```rust
impl From<DtmpfsError> for libc::c_int {
    fn from(e: DtmpfsError) -> libc::c_int {
        use DtmpfsError::*;
        match e {
            NotFound              => libc::ENOENT,
            AlreadyExists         => libc::EEXIST,
            NotADirectory         => libc::ENOTDIR,
            IsADirectory          => libc::EISDIR,
            NotEmpty              => libc::ENOTEMPTY,
            PermissionDenied      => libc::EACCES,
            MetaUnavailable
            | StoreUnavailable(_)
            | BlockGenerationMismatch
            | Io(_)
            | Rpc(_)              => libc::EIO,
        }
    }
}
```

The same trace for **flush** keeps the dirty buffer in `OpenFile.dirty`
(no `mem::take` if writes failed) so the next `fsync` retries. This is a
deliberate departure from §6.5's `mem::take` — the production code must
swap dirty back on error:

```rust
let dirty_save = std::mem::take(&mut of.dirty);
let res = ... flush parallel ... .await;
if res.is_err() {
    of.dirty = dirty_save;     // put it back
    return Err(...);
}
```

(Implementation reminder: this is missing from the example in §6.5 for
clarity; the real code must restore the buffer on failure.)

---

## 9. Memory management

### 9.1 Block lifetime in the data path

```
client write(offset, data)
    -> dirty.insert(idx, BytesMut::zeroed(B))   // 1 alloc
    -> dirty[idx][..].copy_from_slice(data)     // 0 allocs

client flush
    -> let frozen: Bytes = dirty.remove(idx).freeze()  // 0 allocs (transmute)
    -> tonic.write_block(WriteBlockReq { data: frozen.to_vec() })
       NOTE: prost's bytes-mode emits Bytes for `bytes` proto fields,
       avoiding the .to_vec() if `tonic_build::configure().bytes(["."])`
       is set.

store read
    -> entry.value.value.clone()   // refcount bump on Bytes; 0 allocs
    -> ReadBlockResp { data: cloned.to_vec() }  // one Vec alloc
       OR with prost bytes-mode: zero-copy Bytes on the wire too.

client read
    -> Bytes::from(resp.data)      // takes ownership of Vec; 0 allocs
    -> block_cache.insert(...,    Bytes)
    -> on subsequent reads: Bytes::clone() refcount bump
```

### 9.2 BytesMut → Bytes freeze

`BytesMut::freeze()` is `O(1)`: it transmutes the buffer into a `Bytes` and
moves ownership, no copy. Subsequent clones are atomic refcount bumps.
Once frozen, the buffer is immutable — exactly the contract the store needs
(blocks are versioned, never mutated in place).

### 9.3 Memory budgets

| Component | Budget                                    | Enforced where                   |
|-----------|-------------------------------------------|----------------------------------|
| Store     | `StoreConfig.ram_budget_bytes`            | `WriteBlock` precondition (§5.5) |
| Client BlockCache | `ClientConfig.block_cache_capacity_mb` × 1 MiB | `moka` weigher (§6.3)            |
| Client AttrCache | 100k entries fixed (~12 MiB)             | `moka` max_capacity              |
| Client OpenFile dirty | **unbounded**                       | nothing — known v1 limitation     |
| Meta inode table | unbounded                              | nothing — fits 8M inodes / GB      |

The unbounded dirty-buffer is a documented v1 limitation: a single
`dd if=/dev/zero of=/mnt/dtmpfs/big bs=1M count=1000000` will OOM the
client process before flushing. Phase 6 adds a watermark trigger
(`auto_flush_at_dirty_mb`) that flushes when dirty exceeds a threshold
without waiting for `close()`.

### 9.4 Tonic and zero-copy

With `tonic_build::configure().bytes([".",])`, prost emits `bytes::Bytes`
for the `bytes` proto field type. tonic's encoder then writes the `Bytes`
directly into the HTTP/2 framing without copying. Set this in the
`dtmpfs-proto` build script (already shown in §1.2).

---

## 10. Logging / observability

### 10.1 Tracing setup (every binary's main)

```rust
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    fmt()
        .with_env_filter(EnvFilter::from_default_env()
            .add_directive("dtmpfs=info".parse().unwrap()))
        .with_target(true)
        .with_thread_ids(false)
        .compact()
        .init();
}
```

Operators set `RUST_LOG=info`, `RUST_LOG=dtmpfs=debug`, or
`RUST_LOG=dtmpfs_client=trace,dtmpfs_store=info` as needed.

### 10.2 Suggested span names

| Span                 | Component | Fields                                  |
|----------------------|-----------|-----------------------------------------|
| `meta.lookup`        | meta      | `parent`, `name`                        |
| `meta.open`          | meta      | `ino`, `flags`                          |
| `meta.close`         | meta      | `fh`, `ino`, `dirty_count`              |
| `meta.allocate_blocks` | meta    | `ino`, `idx_count`                      |
| `meta.heartbeat`     | meta      | `node_id`                               |
| `store.read_block`   | store     | `ino`, `idx`                            |
| `store.write_block`  | store     | `ino`, `idx`, `gen`, `len`              |
| `store.delete_block` | store     | `ino`, `idx`                            |
| `client.read`        | client    | `ino`, `fh`, `offset`, `size`           |
| `client.write`       | client    | `ino`, `fh`, `offset`, `size`           |
| `client.flush`       | client    | `fh`, `dirty_count`                     |

### 10.3 Suggested log levels

- `info`: open/close, mount/unmount, heartbeat state changes (Up→Down),
  cluster membership delta.
- `debug`: each RPC at the granularity of one call.
- `trace`: per-block read/write, cache hit/miss.
- `warn`: stale-write rejection, replica fallback used, RAM near budget.
- `error`: meta unreachable, panicked task, configuration parse error.

### 10.4 Metrics (Phase 6+, deferred)

Suggested Prometheus names — wire up via `metrics` + `metrics-exporter-prometheus`
behind a `metrics` feature flag.

```
dtmpfs_meta_ops_total{op="open|close|lookup|...", outcome="ok|err"}
dtmpfs_meta_inodes
dtmpfs_meta_open_handles
dtmpfs_store_blocks_total{node="store-0"}
dtmpfs_store_ram_used_bytes{node="store-0"}
dtmpfs_store_ram_budget_bytes{node="store-0"}
dtmpfs_client_cache_hits_total{cache="block|attr"}
dtmpfs_client_cache_misses_total{cache="block|attr"}
dtmpfs_client_dirty_blocks
dtmpfs_rpc_duration_seconds_bucket{component, op}
```

`docs/operations.md` will host the full metrics reference once it ships.

---

## 11. Initialization order

### 11.1 `metasrv` main

```rust
fn main() -> anyhow::Result<()> {
    let args = Args::parse();                 // 1. clap CLI: --config <path>
    let cfg: MetaConfig = toml::from_str(&std::fs::read_to_string(&args.config)?)?
        .expect_meta();                       // 2. load + validate
    init_tracing();                           // 3. tracing
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all().build()?;               // 4. runtime
    rt.block_on(async move {
        let state = Arc::new(RwLock::new(MetaState::new()));
        heartbeat::spawn_watcher(
            state.clone(),
            Duration::from_millis(cfg.heartbeat_timeout_ms),
        );                                    // 5. watcher
        let svc = MetaService::new(state.clone(), cfg.clone());
        let addr: SocketAddr = cfg.listen.parse()?;
        tracing::info!(%addr, "metasrv listening");
        tonic::transport::Server::builder()
            .add_service(MetaServer::new(svc))
            .serve(addr)
            .await?;                          // 6. blocks until shutdown
        Ok::<(), anyhow::Error>(())
    })
}
```

### 11.2 `storesrv` main

```rust
fn main() -> anyhow::Result<()> {
    let args = Args::parse();                 // 1.
    let cfg: StoreConfig = toml::from_str(&std::fs::read_to_string(&args.config)?)?
        .expect_store();                      // 2.
    init_tracing();                           // 3.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all().build()?;               // 4.
    rt.block_on(async move {
        let state = Arc::new(StoreState::new(&cfg));
        heartbeat::spawn_heartbeat(state.clone(), cfg.advertise_addr.clone()); // 5.
        if let Some(bind) = &cfg.debug_http_listen {
            spawn_debug_http(state.clone(), bind.clone());                     // 6.
        }
        let svc = StoreService::new(state.clone());
        let addr: SocketAddr = cfg.listen.parse()?;
        tracing::info!(%addr, "storesrv listening");
        tonic::transport::Server::builder()
            .add_service(StoreServer::new(svc))
            .serve(addr).await?;              // 7.
        Ok::<(), anyhow::Error>(())
    })
}
```

### 11.3 `dtmpfs-mount` main

```rust
fn main() -> anyhow::Result<()> {
    let args = Args::parse();                 // 1.
    let cfg: ClientConfig = toml::from_str(&std::fs::read_to_string(&args.config)?)?
        .expect_client();                     // 2.
    init_tracing();                           // 3.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num_cpus::get().max(2))
        .enable_all()
        .build()?;                            // 4.
    let handle = rt.handle().clone();

    let fs = handle.block_on(async {          // 5. setup on the runtime
        // 5a. connect to meta
        let chan = Channel::from_shared(cfg.meta_addr.clone())?
            .connect().await?;
        let meta = MetaClient::new(chan);

        // 5b. fetch initial node list
        let mut meta_clone = meta.clone();
        let nodes = meta_clone.list_nodes(Empty {}).await?.into_inner();

        // 5c. build store pool
        let mut addrs = HashMap::new();
        for n in nodes.nodes {
            addrs.insert(NodeId(n.id), n.addr);
        }
        let stores = Arc::new(StoreClientPool {
            clients: DashMap::new(),
            addrs:   ArcSwap::new(Arc::new(addrs)),
        });

        // 5d. caches
        let attr_cache  = build_attr_cache(&cfg);
        let block_cache = build_block_cache(&cfg);

        Ok::<DtmpfsFs, anyhow::Error>(DtmpfsFs {
            rt: handle.clone(),
            meta: Mutex::new(meta),
            stores,
            attr_cache,
            block_cache,
            open_files: DashMap::new(),
            block_size: cfg.block_size,
            replication_factor: cfg.replication_factor,
        })
    })?;

    let opts = mount_options(&cfg);           // 6. mount options
    tracing::info!(mount = %cfg.mount_point, "mounting");
    fuser::mount2(fs, &cfg.mount_point, &opts)?;     // 7. blocks until unmount
    tracing::info!("unmounted");
    Ok(())
}
```

The order matters:

- Tracing **before** any meaningful work so the next-line errors are
  captured.
- Runtime **before** anything that calls `block_on`.
- ListNodes before mount so the first FUSE op already has a populated
  store pool.
- `fuser::mount2` last because it blocks; nothing after it runs until
  unmount.

---

## 12. Open implementation questions

These are points where the v1 plan is genuinely ambiguous. They are
documented here so the implementer (or a future contributor) can find them
and decide rather than rediscovering them in code review.

### 12.1 Cross-directory rename and block placement

**Question**: When `rename` moves a file from `/a/x` to `/b/x` where `/a`
and `/b` resolve via different node-affinity rules, should we re-place the
file's blocks?

**Decision (v1)**: No. Placement is per-(ino, block_idx); the inode
identity is preserved by rename, so the placement is preserved. There is
no "directory affinity" in HRW. Document in `docs/architecture.md` and
move on.

### 12.2 `truncate(0)` — eager or lazy block deletion?

**Question**: `truncate(0)` (or `SetAttr` with `size=0`) drops all blocks.
Should we issue `Store.DeleteBlock` immediately or lazily via a GC scan?

**Decision (v1)**: Immediately, fire-and-forget. Meta calls
`tokio::spawn(async move { let _ = client.delete_block(...).await; })`
under the meta write lock for each freed block, then returns success to
the client without awaiting. The store leaks RAM if a DeleteBlock fails;
a Phase 6 GC sweep ("for each block on this store, ask meta if it's
referenced") reconciles. Document in `docs/failure-model.md`.

### 12.3 What's the right `buffer_unordered` width at flush?

**Question**: §6.5 hard-codes 16. On a 10 GbE LAN with 1 MiB blocks, the
optimum is closer to 64; on a constrained link it might be 8.

**Decision (v1)**: Hard-code 16 with a `// TODO(perf)` marker. Make it
configurable in Phase 6 (`ClientConfig.flush_concurrency`).

### 12.4 Should `Open` re-fetch attrs even on AttrCache hit?

**Question**: The plan says "AttrCache TTL = 1 s; bypassed on `open`". But
a hit-and-also-fresh attribute *is* a valid open-time view of the file —
re-fetching wastes an RPC.

**Decision (v1)**: Always re-fetch attr+block_map on `open`, even when
AttrCache is fresh. Reason: the close-to-open invariant is
"reads after open see the writes that closed before that open", and that
invariant relies on `open` being a synchronization point with the meta
server, not with the local AttrCache. If we trusted AttrCache here we'd
have to invalidate it on remote close events, which we have no mechanism
for in v1. Document this as the cost of close-to-open in
`docs/consistency.md`.

### 12.5 Replica writes — fail-any vs require-all

**Question**: At flush, should one failed replica write fail the whole
close (returning EIO), or succeed if the primary succeeded?

**Decision (v1)**: Require-all (the `try_join_all` in §6.5). Reasoning:
v1's whole point of replication is failure tolerance on **read**; if a
replica is silently missing data, a failover read returns EIO and the user
is surprised. Better to fail the close loudly. Phase 6 adds a config
toggle `client.flush_policy = "primary" | "all"`.

### 12.6 `Generation` overflow

**Question**: `Generation` is a `u64`. At 1M closes/sec it wraps in 584k
years, but the check should still exist.

**Decision (v1)**: No check. Note that `bump()` is `self.0 + 1`, which
panics on overflow in debug and wraps in release. Both are acceptable —
the cluster is dead long before this matters.

### 12.7 Meta restart and inode reuse

**Question**: Meta is RAM-only in v1. After a meta restart, `next_ino`
restarts at 2. Can a client cached an inode `42` from before the restart
collide with a freshly-issued `42`?

**Decision (v1)**: Yes, this is a known data-loss-on-meta-crash scenario
documented in `docs/failure-model.md`. The mitigation is "don't survive
meta crashes in v1; the FS is dead and clients must remount". Phase 7
adds a restart epoch to inode IDs (top 16 bits = epoch). Document.

### 12.8 `readdir` cookie stability

**Question**: §4.6 uses an integer offset into BTreeMap iteration as the
cookie. If the directory mutates between calls, entries can be skipped or
double-listed.

**Decision (v1)**: POSIX `readdir(3)` permits this behavior. Document in
`docs/consistency.md` — directory readdir is not snapshot-isolated.

### 12.9 `mknod` on FIFO/socket/dev

**Question**: §6.6 says "FIFO only; block/char devices return EPERM".
Should we support FIFOs at all?

**Decision (v1)**: Yes, FIFOs are filesystem-level objects (no kernel
plumbing needed beyond a special inode). Block/char devices require host
permissions we don't want — return `EPERM`.

---

## End of Low-Level Design

For the corresponding high-level rationale see `docs/HLD.md`. For the
on-the-wire contract see `docs/protocol.md`. For the consistency model and
its limitations see `docs/consistency.md`. For testing strategy and the
acceptance gate that closes out v1 see `docs/testing.md` and
`docs/acceptance-tests.md`.
