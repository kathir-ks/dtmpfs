# dtmpfs-store — Agent Brief

## Role

Block storage server. Holds blocks in a `DashMap<BlockKey, Bytes>`, serves `ReadBlock` /
`WriteBlock` / `DeleteBlock` / `Replicate` / `Stat` RPCs. Sends heartbeats to the meta server.
Runs as binary `storesrv`. Has no knowledge of inodes or paths — it's a content-addressed RAM store.

## Prerequisites

`dtmpfs-proto` and `dtmpfs-common` must compile first (`cargo build -p dtmpfs-proto dtmpfs-common`).

## Crate boundaries

- You own everything under `crates/dtmpfs-store/`.
- Do NOT touch `proto/`, `crates/dtmpfs-proto/`, `crates/dtmpfs-common/`, `crates/dtmpfs-meta/`.

## Files to create

1. `Cargo.toml`
2. `src/main.rs`      — binary entry, CLI, server startup
3. `src/state.rs`     — StoreState struct
4. `src/service.rs`   — tonic StoreService impl
5. `src/heartbeat.rs` — background heartbeat task
6. `src/auth.rs`      — token interceptor (same pattern as meta)
7. `src/debug.rs`     — axum `/debug/blocks` HTTP endpoint

---

## 1. `Cargo.toml`

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
subtle             = { workspace = true }
```

---

## 2. `src/state.rs`

```rust
use bytes::Bytes;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use dtmpfs_common::id::{BlockIdx, Generation, InodeId, NodeId};

// Re-use the same BlockKey from dtmpfs_common.
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
            blocks:     DashMap::new(),
            node_id,
            meta_addr,
            ram_budget,
            ram_used:   AtomicU64::new(0),
        })
    }
}

fn proto_key_to_block_key(k: &dtmpfs_proto::store::BlockKey) -> BlockKey {
    BlockKey {
        ino:        InodeId(k.ino),
        block_idx:  BlockIdx(k.block_idx),
        generation: Generation(k.generation),
    }
}
```

---

## 3. `src/service.rs` — RPC implementations

```rust
use bytes::Bytes;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tonic::{Request, Response, Status};

use dtmpfs_proto::store::{
    store_server::Store,
    DeleteBlockReq, Empty, ReadBlockReq, ReadBlockResp,
    ReplicateReq, StoreStat, WriteBlockReq, WriteBlockResp,
};
use crate::state::StoreState;

pub struct StoreService { pub state: Arc<StoreState> }

#[tonic::async_trait]
impl Store for StoreService {
    async fn read_block(&self, req: Request<ReadBlockReq>)
        -> Result<Response<ReadBlockResp>, Status>
    {
        let r = req.into_inner();
        let proto_key = r.key.ok_or_else(|| Status::invalid_argument("missing key"))?;
        let key = proto_key_to_block_key(&proto_key);
        let entry = self.state.blocks.get(&key)
            .ok_or_else(|| Status::not_found("block not found"))?;
        let data = entry.value().clone();
        let len = data.len() as u32;
        Ok(Response::new(ReadBlockResp { data: data.to_vec(), len }))
    }

    async fn write_block(&self, req: Request<WriteBlockReq>)
        -> Result<Response<WriteBlockResp>, Status>
    {
        let r = req.into_inner();
        let proto_key = r.key.ok_or_else(|| Status::invalid_argument("missing key"))?;
        let key = proto_key_to_block_key(&proto_key);
        let data = Bytes::from(r.data);
        let len = data.len() as u64;

        // Budget check (racy for v1 — acceptable, see LLD §5.5)
        if self.state.ram_used.load(Ordering::Relaxed) + len > self.state.ram_budget {
            return Err(Status::resource_exhausted("ram budget exceeded"));
        }

        let prev_len = self.state.blocks
            .insert(key, data)
            .map(|b| b.len() as u64)
            .unwrap_or(0);
        self.state.ram_used.fetch_add(len, Ordering::Relaxed);
        self.state.ram_used.fetch_sub(prev_len, Ordering::Relaxed);

        Ok(Response::new(WriteBlockResp { len: len as u32 }))
    }

    async fn delete_block(&self, req: Request<DeleteBlockReq>)
        -> Result<Response<Empty>, Status>
    {
        let r = req.into_inner();
        let proto_key = r.key.ok_or_else(|| Status::invalid_argument("missing key"))?;
        let key = proto_key_to_block_key(&proto_key);
        if let Some((_, prev)) = self.state.blocks.remove(&key) {
            self.state.ram_used.fetch_sub(prev.len() as u64, Ordering::Relaxed);
        }
        Ok(Response::new(Empty {}))
    }

    async fn replicate(&self, req: Request<ReplicateReq>)
        -> Result<Response<Empty>, Status>
    {
        // Recipient pulls the block from source via a Store.ReadBlock RPC.
        let r = req.into_inner();
        let source_addr = r.source_addr;
        let proto_key = r.key.ok_or_else(|| Status::invalid_argument("missing key"))?;
        let key = proto_key_to_block_key(&proto_key);

        use dtmpfs_proto::store::store_client::StoreClient;
        use dtmpfs_proto::store::ReadBlockReq;
        let mut client = StoreClient::connect(source_addr).await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        let fetch_req = ReadBlockReq {
            key: Some(proto_key),
            offset: 0,
            len: 0,
        };
        let resp = client.read_block(fetch_req).await?.into_inner();
        let data = Bytes::from(resp.data);
        self.state.blocks.insert(key, data.clone());
        self.state.ram_used.fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(Response::new(Empty {}))
    }

    async fn stat(&self, _req: Request<Empty>)
        -> Result<Response<StoreStat>, Status>
    {
        Ok(Response::new(StoreStat {
            node_id:          self.state.node_id.0.clone(),
            used_bytes:       self.state.ram_used.load(Ordering::Relaxed),
            capacity_bytes:   self.state.ram_budget,
            block_count:      self.state.blocks.len() as u64,
            read_bytes_total:  0,
            write_bytes_total: 0,
        }))
    }
}

fn proto_key_to_block_key(k: &dtmpfs_proto::store::BlockKey) -> dtmpfs_common::id::BlockKey {
    use dtmpfs_common::id::{BlockIdx, Generation, InodeId};
    dtmpfs_common::id::BlockKey {
        ino:        InodeId(k.ino),
        block_idx:  BlockIdx(k.block_idx),
        generation: Generation(k.generation),
    }
}
```

---

## 4. `src/heartbeat.rs`

```rust
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use dtmpfs_proto::meta::{meta_client::MetaClient, HeartbeatReq};
use crate::state::StoreState;
use std::sync::atomic::Ordering;

pub fn spawn_heartbeat(state: Arc<StoreState>, advertise_addr: String) {
    let meta_addr = state.meta_addr.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let epoch_s = SystemTime::now()
            .duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let mut client = loop {
            match MetaClient::connect(meta_addr.clone()).await {
                Ok(c) => break c,
                Err(e) => {
                    tracing::warn!(?e, "heartbeat: waiting for meta");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        };
        loop {
            interval.tick().await;
            let req = HeartbeatReq {
                node_id:       state.node_id.0.clone(),
                addr:          advertise_addr.clone(),
                used_bytes:    state.ram_used.load(Ordering::Relaxed),
                capacity_bytes: state.ram_budget,
                epoch_s,
            };
            if let Err(e) = client.heartbeat_node(req).await {
                tracing::warn!(?e, "heartbeat failed");
            }
        }
    });
}
```

---

## 5. `src/auth.rs`

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

---

## 6. `src/debug.rs`

```rust
use std::sync::Arc;
use std::sync::atomic::Ordering;
use crate::state::StoreState;

pub fn spawn_debug_http(state: Arc<StoreState>, bind: String) {
    use axum::{routing::get, Router, Json};
    let app = Router::new().route("/debug/blocks", get(move || {
        let state = state.clone();
        async move {
            let mut blocks = Vec::new();
            for entry in state.blocks.iter() {
                let (k, v) = (entry.key(), entry.value());
                blocks.push(serde_json::json!({
                    "ino": k.ino.0, "idx": k.block_idx.0,
                    "gen": k.generation.0, "len": v.len(),
                }));
            }
            Json(serde_json::json!({
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

---

## 7. `src/main.rs`

```rust
#[derive(clap::Parser)]
struct Cli {
    #[arg(long)] config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt::init();
    let cfg = dtmpfs_common::config::load(&cli.config)?;
    let dtmpfs_common::config::Config::Store(store_cfg) = cfg else {
        anyhow::bail!("expected role=store in config");
    };
    let state = StoreState::new(
        store_cfg.node_id.clone(),
        store_cfg.meta_addr.clone(),
        store_cfg.ram_budget_bytes,
    );
    spawn_heartbeat(state.clone(), store_cfg.advertise_addr.clone());
    if let Some(bind) = store_cfg.debug_http_listen.clone() {
        spawn_debug_http(state.clone(), bind);
    }
    let token = store_cfg.cluster_token.clone();
    let svc = StoreService { state };
    let addr = store_cfg.listen.parse()?;
    Server::builder()
        .max_decoding_message_size(8 * 1024 * 1024)
        .max_encoding_message_size(8 * 1024 * 1024)
        .add_service(StoreServer::with_interceptor(svc, move |req| check_token(req, &token)))
        .serve(addr)
        .await?;
    Ok(())
}
```

---

## Build command

```bash
cargo build -p dtmpfs-store
```

## Done when

`cargo build -p dtmpfs-store` succeeds and `./target/debug/storesrv --help` prints usage.
