use bytes::Bytes;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tonic::{Request, Response, Status};

use dtmpfs_proto::store::{
    store_server::Store, DeleteBlockReq, Empty, ReadBlockReq, ReadBlockResp, ReplicateReq,
    StoreStat, WriteBlockReq, WriteBlockResp,
};
use dtmpfs_common::id::{BlockIdx, BlockKey, Generation, InodeId};

use crate::state::StoreState;

pub struct StoreService {
    pub state: Arc<StoreState>,
}

#[tonic::async_trait]
impl Store for StoreService {
    async fn read_block(
        &self,
        req: Request<ReadBlockReq>,
    ) -> Result<Response<ReadBlockResp>, Status> {
        let r = req.into_inner();
        let proto_key = r.key.ok_or_else(|| Status::invalid_argument("missing key"))?;
        let key = proto_key_to_block_key(&proto_key);
        let entry = self
            .state
            .blocks
            .get(&key)
            .ok_or_else(|| Status::not_found("block not found"))?;
        let data = entry.value().clone();
        let len = data.len() as u32;
        Ok(Response::new(ReadBlockResp { data, len }))
    }

    async fn write_block(
        &self,
        req: Request<WriteBlockReq>,
    ) -> Result<Response<WriteBlockResp>, Status> {
        let r = req.into_inner();
        let proto_key = r.key.ok_or_else(|| Status::invalid_argument("missing key"))?;
        let key = proto_key_to_block_key(&proto_key);
        let data = Bytes::from(r.data);
        let len = data.len() as u64;

        // Stale-write rejection: refuse writes whose generation is older than
        // the highest generation we have already stored for this logical block.
        let logical = (key.ino, key.block_idx);
        if let Some(hw) = self.state.high_water.get(&logical) {
            if key.generation < *hw {
                return Err(Status::failed_precondition("stale generation"));
            }
        }

        // Budget check (racy for v1 — acceptable, see LLD §5.5)
        if self.state.ram_used.load(Ordering::Relaxed) + len > self.state.ram_budget {
            return Err(Status::resource_exhausted("ram budget exceeded"));
        }

        let prev_len = self
            .state
            .blocks
            .insert(key, data)
            .map(|b| b.len() as u64)
            .unwrap_or(0);
        self.state.ram_used.fetch_add(len, Ordering::Relaxed);
        self.state.ram_used.fetch_sub(prev_len, Ordering::Relaxed);

        // Advance high-water mark.
        self.state.high_water
            .entry(logical)
            .and_modify(|hw| { if key.generation > *hw { *hw = key.generation; } })
            .or_insert(key.generation);

        Ok(Response::new(WriteBlockResp { len: len as u32 }))
    }

    async fn delete_block(
        &self,
        req: Request<DeleteBlockReq>,
    ) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let proto_key = r.key.ok_or_else(|| Status::invalid_argument("missing key"))?;
        let key = proto_key_to_block_key(&proto_key);
        if let Some((_, prev)) = self.state.blocks.remove(&key) {
            self.state
                .ram_used
                .fetch_sub(prev.len() as u64, Ordering::Relaxed);
        }
        self.state.high_water.remove(&(key.ino, key.block_idx));
        Ok(Response::new(Empty {}))
    }

    async fn replicate(&self, req: Request<ReplicateReq>) -> Result<Response<Empty>, Status> {
        use dtmpfs_proto::store::{store_client::StoreClient, ReadBlockReq};

        let r = req.into_inner();
        let source_addr = r.source_addr;
        let proto_key = r.key.ok_or_else(|| Status::invalid_argument("missing key"))?;
        let key = proto_key_to_block_key(&proto_key);

        let mut client = StoreClient::connect(source_addr)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        let fetch_req = ReadBlockReq {
            key: Some(proto_key),
            offset: 0,
            len: 0,
        };
        let resp = client.read_block(fetch_req).await?.into_inner();
        let data = Bytes::from(resp.data);
        self.state
            .ram_used
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.state.blocks.insert(key, data);
        Ok(Response::new(Empty {}))
    }

    async fn stat(&self, _req: Request<Empty>) -> Result<Response<StoreStat>, Status> {
        Ok(Response::new(StoreStat {
            node_id:           self.state.node_id.0.clone(),
            used_bytes:        self.state.ram_used.load(Ordering::Relaxed),
            capacity_bytes:    self.state.ram_budget,
            block_count:       self.state.blocks.len() as u64,
            read_bytes_total:  0,
            write_bytes_total: 0,
        }))
    }
}

fn proto_key_to_block_key(k: &dtmpfs_proto::store::BlockKey) -> BlockKey {
    BlockKey {
        ino:        InodeId(k.ino),
        block_idx:  BlockIdx(k.block_idx),
        generation: Generation(k.generation),
    }
}
