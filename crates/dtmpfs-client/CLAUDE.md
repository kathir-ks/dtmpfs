# dtmpfs-client — Agent Brief

## Role

FUSE filesystem client. Mounts dtmpfs at a local path by translating FUSE kernel callbacks into
gRPC RPCs against the meta and store services. Implements close-to-open consistency via
generation-keyed block caching and a flush-on-close protocol. Runs as binary `dtmpfs-mount`.

## Prerequisites

`dtmpfs-proto` and `dtmpfs-common` must compile first (`cargo build -p dtmpfs-proto dtmpfs-common`).

## Crate boundaries

- You own everything under `crates/dtmpfs-client/`.
- Do NOT touch `proto/`, `crates/dtmpfs-proto/`, `crates/dtmpfs-common/`, `crates/dtmpfs-meta/`,
  or `crates/dtmpfs-store/`.

## Files to create

1. `Cargo.toml`
2. `src/main.rs`      — binary entry, CLI, tokio runtime, FUSE mount
3. `src/fs.rs`        — `DtmpfsFs` struct implementing `fuser::Filesystem`
4. `src/cache.rs`     — `AttrCache` (moka TTL) + `BlockCache` (moka LRU weighted by bytes)
5. `src/open_file.rs` — `OpenFile` struct (per file-handle state)
6. `src/flush.rs`     — flush + fsync algorithm (`publish()`)
7. `src/client.rs`    — `StoreClientPool` (lazy DashMap of gRPC channels)

---

## 1. `Cargo.toml`

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
subtle             = { workspace = true }
```

---

## 2. `src/open_file.rs`

```rust
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
```

---

## 3. `src/client.rs`

```rust
use std::collections::HashMap;
use std::sync::Arc;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use tonic::transport::Channel;

use dtmpfs_proto::store::store_client::StoreClient;
use dtmpfs_common::error::DtmpfsError;
use dtmpfs_common::id::NodeId;

pub struct StoreClientPool {
    pub clients: DashMap<NodeId, StoreClient<Channel>>,
    pub addrs:   ArcSwap<HashMap<NodeId, String>>,
}

impl StoreClientPool {
    pub fn new() -> Arc<Self> {
        Arc::new(StoreClientPool {
            clients: DashMap::new(),
            addrs:   ArcSwap::from_pointee(HashMap::new()),
        })
    }

    pub async fn get(&self, id: &NodeId) -> Result<StoreClient<Channel>, DtmpfsError> {
        if let Some(c) = self.clients.get(id) { return Ok(c.clone()); }
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

    pub fn refresh_addrs(&self, m: HashMap<NodeId, String>) {
        self.addrs.store(Arc::new(m));
    }
}
```

---

## 4. `src/cache.rs`

```rust
use std::time::{Duration, Instant};
use bytes::Bytes;
use moka::sync::Cache;
use dtmpfs_common::id::{BlockIdx, Generation, InodeId};

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
```

---

## 5. `src/fs.rs` — DtmpfsFs struct and key FUSE callbacks

```rust
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use moka::sync::Cache;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tonic::transport::Channel;

use dtmpfs_common::id::{BlockIdx, BlockPlacement, Generation, InodeId, NodeId};
use dtmpfs_common::error::DtmpfsError;
use dtmpfs_proto::meta::meta_client::MetaClient;
use dtmpfs_proto::meta::{GetAttrReq, LookupReq, OpenReq};
use dtmpfs_proto::store::{store_client::StoreClient, ReadBlockReq};

use crate::cache::CachedAttr;
use crate::client::StoreClientPool;
use crate::open_file::OpenFile;

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
```

### Key helpers

**`fetch_block`** — dirty → block_cache → store RPC, with replica fallback:

```rust
impl DtmpfsFs {
    pub async fn fetch_block(&self, of: &OpenFile, idx: BlockIdx)
        -> Result<Bytes, DtmpfsError>
    {
        // 1. dirty buffer wins
        if let Some(buf) = of.dirty.get(&idx) {
            return Ok(Bytes::copy_from_slice(buf));
        }
        // 2. block cache
        if let Some(b) = self.block_cache.get(&(of.ino, of.generation, idx)) {
            return Ok(b);
        }
        // 3. remote: primary first, then replicas (R>=2)
        let placement = of.block_map.get(&idx).ok_or(DtmpfsError::NotFound)?;
        for node in std::iter::once(&placement.primary).chain(placement.replicas.iter()) {
            let mut client = match self.stores.get(node).await { Ok(c) => c, Err(_) => continue };
            let req = tonic::Request::new(ReadBlockReq {
                key: Some(dtmpfs_proto::store::BlockKey {
                    ino: of.ino.0, block_idx: idx.0, generation: of.generation.0,
                }),
                offset: 0, len: 0,
            });
            match client.read_block(req).await {
                Ok(r) => {
                    let b = Bytes::from(r.into_inner().data);
                    self.block_cache.insert((of.ino, of.generation, idx), b.clone());
                    return Ok(b);
                }
                Err(s) if s.code() == tonic::Code::NotFound => {
                    return Ok(Bytes::from(vec![0u8; self.block_size])); // sparse hole
                }
                Err(_) => continue,
            }
        }
        Err(DtmpfsError::NotFound)
    }
}
```

**`apply_write`** — RMW into dirty buffer (no RPC):

```rust
impl DtmpfsFs {
    pub async fn apply_write(&self, of: &mut OpenFile, offset: u64, data: &[u8])
        -> Result<u32, DtmpfsError>
    {
        let bs = self.block_size as u64;
        let mut written = 0u32;
        let mut cur = offset;
        let end = offset + data.len() as u64;
        while cur < end {
            let idx = BlockIdx(cur / bs);
            let block_off = idx.0 * bs;
            let in_block  = (cur - block_off) as usize;
            let chunk_len = ((block_off + bs).min(end) - cur) as usize;
            if !of.dirty.contains_key(&idx) {
                let init = if in_block == 0 && chunk_len == self.block_size {
                    BytesMut::zeroed(self.block_size)
                } else if of.block_map.contains_key(&idx) {
                    let b = self.fetch_block(of, idx).await?;
                    let mut bm = BytesMut::with_capacity(self.block_size);
                    bm.extend_from_slice(&b);
                    if bm.len() < self.block_size { bm.resize(self.block_size, 0); }
                    bm
                } else {
                    BytesMut::zeroed(self.block_size)
                };
                of.dirty.insert(idx, init);
            }
            let buf = of.dirty.get_mut(&idx).unwrap();
            buf[in_block..in_block + chunk_len]
                .copy_from_slice(&data[written as usize..written as usize + chunk_len]);
            written += chunk_len as u32;
            cur += chunk_len as u64;
        }
        of.size_hint = of.size_hint.max(end);
        Ok(written)
    }
}
```

### FUSE Filesystem impl

Implement `fuser::Filesystem` for `DtmpfsFs`. Every callback does:
`self.rt.block_on(async { ... })` to bridge into tokio, then calls `reply.data()/reply.attr()/reply.error()`.

**Key methods to implement (full list in docs/LLD.md §6.6):**

- `lookup` → `Meta.Lookup` → cache attr → `reply.entry()`
- `getattr` → check AttrCache → `Meta.GetAttr` on miss → `reply.attr()`
- `setattr` → `Meta.SetAttr` → invalidate AttrCache → `reply.attr()`
- `create` → `Meta.Create` → insert OpenFile → `reply.create()`
- `open` → `Meta.Open` → insert OpenFile → `reply.opened(fh, flags)`
- `read` → `fetch_block` loop → `reply.data()`
- `write` → `apply_write` → `reply.written(n)`
- `flush` → `flush_path(fh)` → `reply.ok()` or `reply.error(errno)`
- `release` → `flush_path(fh)` → remove from open_files → `reply.ok()`
- `fsync` → `fsync_path(fh)` → `reply.ok()`
- `mkdir` → `Meta.Mkdir` → `reply.entry()`
- `unlink` → `Meta.Unlink` → `reply.ok()`
- `rmdir` → `Meta.Rmdir` → `reply.ok()`
- `rename` → `Meta.Rename` → `reply.ok()`
- `readdir` → `Meta.ReadDir` → `reply.add()` loop → `reply.ok()`
- `statfs` → `Meta.ListNodes` → sum ram → `reply.statfs()`
- `link` → `reply.error(libc::EPERM)` (hardlinks unsupported)
- `setxattr`/`getxattr`/`listxattr`/`removexattr` → `reply.error(libc::ENOSYS)`

Mount options to use:
```rust
vec![
    FSName("dtmpfs".into()), Subtype("dtmpfs".into()),
    DefaultPermissions, AllowOther, AutoUnmount, NoAtime,
]
```

---

## 6. `src/flush.rs`

```rust
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum WaitPolicy { PrimariesOnly, AllReplicas }

impl DtmpfsFs {
    pub async fn flush_path(&self, fh: u64) -> Result<(), DtmpfsError> {
        self.publish(fh, WaitPolicy::PrimariesOnly).await
    }
    pub async fn fsync_path(&self, fh: u64) -> Result<(), DtmpfsError> {
        self.publish(fh, WaitPolicy::AllReplicas).await
    }

    async fn publish(&self, fh: u64, wait: WaitPolicy) -> Result<(), DtmpfsError> {
        let of_arc = self.open_files.get(&fh).ok_or(DtmpfsError::NotFound)?.clone();
        let mut of = of_arc.lock().await;
        if of.dirty.is_empty() { return Ok(()); }

        // 1. AllocateBlocks for new indices
        let new_idxs: Vec<u64> = of.dirty.keys()
            .filter(|i| !of.block_map.contains_key(i))
            .map(|i| i.0).collect();
        if !new_idxs.is_empty() {
            use dtmpfs_proto::meta::AllocReq;
            let req = tonic::Request::new(AllocReq { ino: of.ino.0, block_idxs: new_idxs });
            let resp = self.meta.lock().await.allocate_blocks(req).await
                .map_err(|s| DtmpfsError::from_status(s, None))?
                .into_inner();
            for loc in resp.block_map {
                of.block_map.insert(BlockIdx(loc.block_idx), BlockPlacement {
                    primary:  NodeId(loc.primary),
                    replicas: loc.replicas.into_iter().map(NodeId).collect(),
                });
            }
        }

        // 2. Fan out WriteBlock RPCs (buffer_unordered(16))
        use futures::stream::{StreamExt, TryStreamExt};
        use dtmpfs_proto::store::{BlockKey as ProtoBlockKey, WriteBlockReq};
        let dirty = std::mem::take(&mut of.dirty);
        let ino = of.ino; let gen = of.generation;
        let stores = self.stores.clone();
        let block_map = of.block_map.clone();
        let written_idxs: Vec<u64> = dirty.keys().map(|i| i.0).collect();

        futures::stream::iter(dirty)
            .map(move |(idx, buf)| {
                let placement = block_map.get(&idx).cloned().ok_or(DtmpfsError::NotFound);
                let stores = stores.clone();
                async move {
                    let placement = placement?;
                    let frozen = buf.freeze();
                    let write_one = |n: NodeId, data: Bytes| {
                        let stores = stores.clone();
                        async move {
                            let mut c = stores.get(&n).await?;
                            let req = tonic::Request::new(WriteBlockReq {
                                key: Some(ProtoBlockKey {
                                    ino: ino.0, block_idx: idx.0, generation: gen.0,
                                }),
                                data: data.to_vec(),
                            });
                            c.write_block(req).await
                                .map_err(|s| DtmpfsError::from_status(s, Some(n)))?;
                            Ok::<_, DtmpfsError>(())
                        }
                    };
                    write_one(placement.primary.clone(), frozen.clone()).await?;
                    match wait {
                        WaitPolicy::AllReplicas => {
                            for replica in placement.replicas.iter().cloned() {
                                write_one(replica, frozen.clone()).await?;
                            }
                        }
                        WaitPolicy::PrimariesOnly => {
                            for replica in placement.replicas.into_iter() {
                                let w = write_one(replica, frozen.clone());
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

        // 3. Meta.Close
        use dtmpfs_proto::meta::CloseReq;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let close_req = CloseReq {
            fh, ino: ino.0, expected_generation: gen.0,
            new_size: of.size_hint,
            mtime_s: now.as_secs() as i64, mtime_ns: now.subsec_nanos(),
            written_block_idxs: written_idxs,
        };
        let resp = self.meta.lock().await.close(tonic::Request::new(close_req)).await
            .map_err(|s| DtmpfsError::from_status(s, None))?
            .into_inner();
        of.generation = Generation(resp.attr.as_ref().map(|a| a.generation).unwrap_or(gen.0));

        // 4. Optional: prune stale block cache entries
        let new_gen = of.generation; let ino_c = ino;
        self.block_cache.invalidate_entries_if(move |k, _| k.0 == ino_c && k.1 < new_gen).ok();
        Ok(())
    }
}
```

---

## 7. `src/main.rs`

```rust
#[derive(clap::Parser)]
struct Cli {
    #[arg(long)] config: String,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt::init();
    let cfg = dtmpfs_common::config::load(&cli.config)?;
    let dtmpfs_common::config::Config::Client(client_cfg) = cfg else {
        anyhow::bail!("expected role=client in config");
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(client_cfg.tokio_worker_threads.unwrap_or(0) as usize)
        .enable_all()
        .build()?;
    let handle = rt.handle().clone();
    let fs = rt.block_on(async { build_fs(handle, &client_cfg).await })?;
    let opts = vec![
        fuser::MountOption::FSName("dtmpfs".into()),
        fuser::MountOption::Subtype("dtmpfs".into()),
        fuser::MountOption::DefaultPermissions,
        fuser::MountOption::AllowOther,
        fuser::MountOption::AutoUnmount,
        fuser::MountOption::NoAtime,
    ];
    fuser::mount2(fs, &client_cfg.mount_point, &opts)?;
    Ok(())
}

async fn build_fs(handle: tokio::runtime::Handle, cfg: &dtmpfs_common::config::ClientConfig)
    -> anyhow::Result<DtmpfsFs>
{
    use dtmpfs_proto::meta::meta_client::MetaClient;
    use tonic::transport::Channel;
    use crate::cache::{build_attr_cache, build_block_cache};
    use crate::client::StoreClientPool;

    let meta_chan = Channel::from_shared(cfg.meta_addr.clone())?.connect().await?;
    let meta     = tokio::sync::Mutex::new(MetaClient::new(meta_chan));
    let stores   = StoreClientPool::new();
    let attr_cache  = build_attr_cache(cfg.attr_cache_ttl_ms);
    let block_cache = build_block_cache(cfg.block_cache_capacity_mb);
    Ok(DtmpfsFs {
        rt: handle, meta, stores,
        attr_cache, block_cache,
        open_files: dashmap::DashMap::new(),
        block_size: cfg.block_size,
        replication_factor: cfg.replication_factor,
    })
}
```

---

## Build command

```bash
cargo build -p dtmpfs-client
```

## Done when

`cargo build -p dtmpfs-client` succeeds and `./target/debug/dtmpfs-mount --help` prints usage.

## Key invariants to keep in mind

- Every FUSE callback bridges to async via `self.rt.block_on(async { ... })`.
- No RPC is issued on `write(2)` — only during `flush`/`fsync`.
- `BlockCache` key includes `Generation` so stale entries never serve fresh reads.
- `flush_path` waits for primaries only; `fsync_path` waits for all replicas.
- After `Meta.Close` bumps the generation, invalidate old cache entries for that inode.
