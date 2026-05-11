# dtmpfs-meta — Agent Brief

## Role

The single authoritative metadata server. Owns the inode table, directory tree, file-handle
allocation, block placement, and store membership. Exposes the `dtmpfs.meta.v1.Meta` gRPC service
plus a debug HTTP endpoint. Runs as binary `metasrv`.

## Prerequisites

`dtmpfs-proto` and `dtmpfs-common` must compile first (`cargo build -p dtmpfs-proto dtmpfs-common`).

## Crate boundaries

- You own everything under `crates/dtmpfs-meta/`.
- Do NOT touch `proto/`, `crates/dtmpfs-proto/`, `crates/dtmpfs-common/`, or `crates/dtmpfs-store/`.

## Files to create

1. `Cargo.toml`
2. `src/main.rs`   — binary entry point, CLI, tracing, server startup
3. `src/state.rs`  — MetaState struct and all mutation helpers
4. `src/service.rs` — tonic MetaService impl (all 14 RPCs)
5. `src/auth.rs`   — cluster-token interceptor
6. `src/debug.rs`  — axum `/debug/state` HTTP endpoint

---

## 1. `Cargo.toml`

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
axum               = { workspace = true }
subtle             = { workspace = true }
```

---

## 2. `src/state.rs` — key types

```rust
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
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
    pub replication_factor: usize,
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
    pub blocks:         BTreeMap<BlockIdx, BlockPlacement>,
    pub symlink_target: Option<String>,
}

pub struct OpenHandleSt {
    pub fh:                 u64,
    pub ino:                InodeId,
    pub flags:              i32,
    pub generation_at_open: Generation,
}

#[derive(Clone, Debug)]
pub struct NodeInfo {
    pub node_id:   NodeId,
    pub addr:      String,
    pub ram_used:  u64,
    pub ram_total: u64,
    pub status:    NodeStatus,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NodeStatus { Up, Down }
```

### MetaState initialization

```rust
impl MetaState {
    pub fn new(replication_factor: usize) -> Arc<RwLock<Self>> {
        let mut inodes = HashMap::new();
        let mut dirs   = HashMap::new();
        let now = SystemTime::now();
        let root = Inode {
            ino: InodeId::ROOT, kind: InodeKind::Dir,
            mode: 0o40755, uid: 0, gid: 0, size: 4096, nlink: 2,
            atime: now, mtime: now, ctime: now,
            generation: Generation(0), blocks: BTreeMap::new(), symlink_target: None,
        };
        inodes.insert(InodeId::ROOT, root);
        dirs.insert(InodeId::ROOT, BTreeMap::new());
        Arc::new(RwLock::new(MetaState {
            inodes, dirs,
            next_ino: AtomicU64::new(2),
            open_handles: HashMap::new(),
            next_fh: AtomicU64::new(1),
            nodes: HashMap::new(),
            last_heartbeat: HashMap::new(),
            replication_factor,
        }))
    }

    pub fn alloc_ino(&self) -> InodeId {
        InodeId(self.next_ino.fetch_add(1, Ordering::Relaxed))
    }

    pub fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }

    pub fn live_nodes(&self) -> Vec<NodeId> {
        self.nodes.values()
            .filter(|n| n.status == NodeStatus::Up)
            .map(|n| n.node_id.clone())
            .collect()
    }

    pub fn allocate_blocks(&self, ino: InodeId, idxs: &[BlockIdx]) -> Vec<(BlockIdx, BlockPlacement)> {
        use dtmpfs_common::hash::pick_nodes;
        use dtmpfs_common::id::{BlockKey, Generation};
        let live = self.live_nodes();
        let r = self.replication_factor;
        idxs.iter().map(|&idx| {
            let key = BlockKey { ino, block_idx: idx, generation: Generation(0) };
            let chosen = pick_nodes(&key, &live, r);
            let placement = BlockPlacement {
                primary:  chosen.first().cloned().unwrap_or_else(|| NodeId::new("?")),
                replicas: chosen.into_iter().skip(1).collect(),
            };
            (idx, placement)
        }).collect()
    }
}
```

### build_attr helper

```rust
pub fn build_attr(inode: &Inode) -> dtmpfs_proto::meta::Attr {
    use dtmpfs_proto::meta::Attr;
    use std::time::UNIX_EPOCH;
    let ts = |t: SystemTime| {
        let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
        (d.as_secs() as i64, d.subsec_nanos())
    };
    let (at_s, at_ns) = ts(inode.atime);
    let (mt_s, mt_ns) = ts(inode.mtime);
    let (ct_s, ct_ns) = ts(inode.ctime);
    Attr {
        ino:        inode.ino.0,
        size:       inode.size,
        blocks:     inode.blocks.len() as u64,
        generation: inode.generation.0,
        mode:       inode.mode,
        nlink:      inode.nlink,
        uid:        inode.uid,
        gid:        inode.gid,
        atime_s:    at_s, atime_ns: at_ns,
        mtime_s:    mt_s, mtime_ns: mt_ns,
        ctime_s:    ct_s, ctime_ns: ct_ns,
    }
}
```

---

## 3. `src/service.rs` — RPC handlers (pseudocode + key implementations)

Implement `dtmpfs_proto::meta::meta_server::Meta` on a `MetaService { state: Arc<RwLock<MetaState>>, ... }`.

**`Lookup(parent_ino, name) -> LookupResp`**
```
read lock
dirs.get(parent_ino) -> get(name) -> inodes.get(ino) -> build_attr
```

**`GetAttr(ino) -> Attr`**
```
read lock; inodes.get(ino) -> build_attr or NOT_FOUND
```

**`SetAttr(ino, mask)`**
```
write lock; mutate mode/uid/gid/size/mtime/atime under mask
if size shrinks: drop inode.blocks entries past new last block_idx
ctime = now; return updated attr
```

**`Create(parent_ino, name, mode, uid, gid) -> (Attr, fh, [])`**
```
write lock
collision check -> ALREADY_EXISTS
alloc_ino; insert Inode{kind=File} + dirent
alloc_fh; insert OpenHandleSt
return (attr, fh, [])   // empty block_map for new file
```

**`Mkdir(parent_ino, name, mode) -> Attr`**
```
write lock; collision check
alloc_ino; insert Inode{kind=Dir, nlink=2}; dirs.insert(ino, BTreeMap::new())
parent nlink += 1
```

**`Unlink(parent_ino, name)`**
```
write lock
remove dirent; remove inode (if kind==Dir -> EISDIR)
for (idx, placement) in inode.blocks: fire-and-forget DeleteBlock RPCs
```

**`Rmdir(parent_ino, name)`**
```
write lock
child dir must be empty (NotEmpty)
remove dir, remove inode, remove dirent; parent nlink -= 1
```

**`Rename(src_parent, src_name, dst_parent, dst_name)`**
```
write lock; atomic move under single lock (MetaState holds both)
POSIX: if dst exists, unlink/rmdir it first (checking dir-not-empty)
```

**`ReadDir(ino, cookie, max_entries) -> ReadDirResp`**
```
read lock; dirs.get(ino)
cookie = last returned name (or "" for start); BTreeMap range from cookie
collect up to max_entries; return {entries, next_cookie, eof}
```

**`Open(ino, flags) -> OpenResp`**
```
read lock for inode; write lock for handle insert
Inode must be File
alloc_fh; insert OpenHandleSt{generation_at_open = inode.generation}
return {attr, fh, block_map clone}
```

**`Close(fh, ino, expected_generation, new_size, mtime_s, mtime_ns, written_block_idxs)`**
```
write lock
remove open_handles[fh] -> NOT_FOUND if missing
if !written_block_idxs.empty:
    if inode.generation != expected_generation -> FAILED_PRECONDITION
    inode.generation = inode.generation.bump()
    inode.size = new_size; inode.mtime = max(req_mtime, now - 60s); inode.ctime = now
return CloseResp{attr}
```

**`AllocateBlocks(ino, block_idxs) -> AllocResp`**
```
write lock
for each idx: if already in inode.blocks, return existing; else allocate via pick_nodes
insert new placements into inode.blocks
return block_map (BlockLoc list)
```

**`HeartbeatNode(node_id, addr, used_bytes, capacity_bytes, epoch_s)`**
```
write lock
upsert nodes[node_id] = NodeInfo{addr, ram_used, ram_total, status=Up}
last_heartbeat[node_id] = Instant::now()
return HeartbeatResp{cluster: all nodes}
```

**`ListNodes() -> NodeList`**
```
read lock; return all Up nodes as NodeInfo list
```

### Heartbeat watcher task (spawn from main)

```rust
pub fn spawn_heartbeat_watcher(state: Arc<RwLock<MetaState>>, dead_after: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let now = Instant::now();
            let mut s = state.write().await;
            let downs: Vec<NodeId> = s.last_heartbeat.iter()
                .filter(|(_, last)| now.duration_since(**last) > dead_after)
                .map(|(id, _)| id.clone()).collect();
            for id in downs {
                if let Some(ni) = s.nodes.get_mut(&id) {
                    if ni.status != NodeStatus::Down {
                        tracing::warn!(?id, "node marked Down");
                        ni.status = NodeStatus::Down;
                    }
                }
            }
        }
    });
}
```

---

## 4. `src/auth.rs` — token interceptor

```rust
use subtle::ConstantTimeEq;
use tonic::{Request, Status};

pub fn check_token(req: Request<()>, expected: &str) -> Result<Request<()>, Status> {
    let got = req.metadata()
        .get("cluster-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if got.as_bytes().ct_eq(expected.as_bytes()).into() {
        Ok(req)
    } else {
        Err(Status::unauthenticated("invalid cluster token"))
    }
}
```

Use as a tonic interceptor via `MetaServer::with_interceptor(svc, move |req| check_token(req, &token))`.

---

## 5. `src/main.rs`

```rust
// Sketch — fill in with real clap derive + tonic Server::builder setup
#[derive(clap::Parser)]
struct Cli {
    #[arg(long)] config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt::init();
    let cfg = dtmpfs_common::config::load(&cli.config)?;
    let dtmpfs_common::config::Config::Meta(meta_cfg) = cfg else {
        anyhow::bail!("expected role=meta in config");
    };
    let state = MetaState::new(meta_cfg.replication_factor);
    let dead = Duration::from_millis(meta_cfg.heartbeat_timeout_ms);
    spawn_heartbeat_watcher(state.clone(), dead);
    let token = meta_cfg.cluster_token.clone();
    let svc = MetaService { state };
    let addr = meta_cfg.listen.parse()?;
    Server::builder()
        .max_decoding_message_size(8 * 1024 * 1024)
        .max_encoding_message_size(8 * 1024 * 1024)
        .add_service(MetaServer::with_interceptor(svc, move |req| check_token(req, &token)))
        .serve(addr)
        .await?;
    Ok(())
}
```

---

## 6. `src/debug.rs` — optional HTTP `/debug/state`

```rust
pub fn spawn_debug_http(state: Arc<RwLock<MetaState>>, bind: String) {
    use axum::{routing::get, Router, Json};
    let app = Router::new().route("/debug/state", get(move || {
        let state = state.clone();
        async move {
            let s = state.read().await;
            Json(serde_json::json!({
                "inodes": s.inodes.len(),
                "open_handles": s.open_handles.len(),
                "nodes": s.nodes.keys().map(|n| n.as_str()).collect::<Vec<_>>(),
            }))
        }
    }));
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&bind).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });
}
```

---

## Build command

```bash
cargo build -p dtmpfs-meta
```

## Done when

`cargo build -p dtmpfs-meta` succeeds and `./target/debug/metasrv --help` prints usage.
