use std::time::{SystemTime, UNIX_EPOCH};
use bytes::Bytes;
use futures::stream::{StreamExt, TryStreamExt};

use std::collections::HashMap;
use dtmpfs_common::error::DtmpfsError;
use dtmpfs_common::id::{BlockIdx, Generation, NodeId};
use dtmpfs_proto::meta::{AllocReq, CloseReq, Empty};
use dtmpfs_proto::store::{BlockKey as ProtoBlockKey, WriteBlockReq};

use crate::fs::DtmpfsFs;

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
        let of_arc = self.open_files.get(&fh)
            .ok_or(DtmpfsError::NotFound)?
            .clone();
        let mut of = of_arc.lock().await;
        if of.dirty.is_empty() { return Ok(()); }

        // 0. Refresh store node addresses from meta
        let node_list = self.meta.lock().await
            .list_nodes(self.authed(Empty {})).await
            .map_err(|s| DtmpfsError::from_status(s, None))?
            .into_inner();
        let addr_map: HashMap<NodeId, String> = node_list.nodes.iter()
            .map(|n| (NodeId(n.node_id.clone()), format!("http://{}", n.addr)))
            .collect();
        self.stores.refresh_addrs(addr_map);

        // 1. AllocateBlocks for new indices
        let new_idxs: Vec<u64> = of.dirty.keys()
            .filter(|i| !of.block_map.contains_key(i))
            .map(|i| i.0)
            .collect();
        if !new_idxs.is_empty() {
            let req = self.authed(AllocReq { ino: of.ino.0, block_idxs: new_idxs });
            let resp = self.meta.lock().await.allocate_blocks(req).await
                .map_err(|s| DtmpfsError::from_status(s, None))?
                .into_inner();
            for loc in resp.block_map {
                of.block_map.insert(
                    BlockIdx(loc.block_idx),
                    dtmpfs_common::id::BlockPlacement {
                        primary:  NodeId(loc.primary),
                        replicas: loc.replicas.into_iter().map(NodeId).collect(),
                    },
                );
            }
        }

        // 2. Fan-out WriteBlock RPCs
        let dirty = std::mem::take(&mut of.dirty);
        let ino = of.ino;
        let gen = of.generation;
        let stores = self.stores.clone();
        let block_map = of.block_map.clone();
        let written_idxs: Vec<u64> = dirty.keys().map(|i| i.0).collect();
        let token = self.token.clone();

        futures::stream::iter(dirty)
            .map(move |(idx, buf)| {
                let placement = block_map.get(&idx).cloned().ok_or(DtmpfsError::NotFound);
                let stores = stores.clone();
                let token = token.clone();
                async move {
                    let placement = placement?;
                    let frozen = buf.freeze();

                    let write_one = |n: NodeId, data: Bytes| {
                        let stores = stores.clone();
                        let token = token.clone();
                        async move {
                            let mut c = stores.get(&n).await?;
                            let mut req = tonic::Request::new(WriteBlockReq {
                                key: Some(ProtoBlockKey {
                                    ino: ino.0,
                                    block_idx: idx.0,
                                    generation: 0,
                                }),
                                data: data,
                            });
                            req.metadata_mut().insert(
                                "cluster-token",
                                token.parse().expect("ascii token"),
                            );
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let close_req = CloseReq {
            fh,
            ino: ino.0,
            expected_generation: gen.0,
            new_size: of.size_hint,
            mtime_s: now.as_secs() as i64,
            mtime_ns: now.subsec_nanos(),
            written_block_idxs: written_idxs,
        };
        let resp = self.meta.lock().await
            .close(self.authed(close_req)).await
            .map_err(|s| DtmpfsError::from_status(s, None))?
            .into_inner();

        of.generation = Generation(
            resp.attr.as_ref().map(|a| a.generation).unwrap_or(gen.0)
        );

        // 4. Prune stale block cache entries
        let new_gen = of.generation;
        let ino_c = ino;
        self.block_cache
            .invalidate_entries_if(move |k, _| k.0 == ino_c && k.1 < new_gen)
            .ok();

        Ok(())
    }
}
