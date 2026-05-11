use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};

use dtmpfs_common::id::{BlockIdx, BlockPlacement, Generation, InodeId, InodeKind, NodeId};
use dtmpfs_proto::meta::{
    self,
    meta_server::Meta,
    AllocReq, AllocResp, Attr, BlockLoc, CloseReq, CloseResp, CreateReq, CreateResp, DirEntry,
    Empty, GetAttrReq, HeartbeatReq, HeartbeatResp, LookupReq, LookupResp, MkdirReq, NodeInfo as
    ProtoNodeInfo, NodeList, OpenReq, OpenResp, ReadDirReq, ReadDirResp, RenameReq, RmdirReq,
    SetAttrReq, SetAttrResp, UnlinkReq,
};

use crate::state::{build_attr, Inode, MetaState, NodeInfo, NodeStatus, OpenHandleSt};

pub struct MetaService {
    pub state: Arc<RwLock<MetaState>>,
    pub token: String,
}

fn fire_delete_blocks(
    blocks: Vec<(BlockIdx, BlockPlacement)>,
    node_addrs: HashMap<NodeId, String>,
    ino: InodeId,
    token: String,
) {
    use dtmpfs_proto::store::store_client::StoreClient;
    use dtmpfs_proto::store::{BlockKey as ProtoBlockKey, DeleteBlockReq};

    for (idx, placement) in blocks {
        let nodes: Vec<NodeId> = std::iter::once(placement.primary)
            .chain(placement.replicas)
            .collect();
        for node_id in nodes {
            let Some(addr) = node_addrs.get(&node_id).cloned() else { continue };
            let token = token.clone();
            tokio::spawn(async move {
                let chan = match tonic::transport::Channel::from_shared(addr) {
                    Ok(e) => match e.connect().await { Ok(c) => c, Err(_) => return },
                    Err(_) => return,
                };
                let mut client = StoreClient::new(chan);
                let mut req = tonic::Request::new(DeleteBlockReq {
                    key: Some(ProtoBlockKey { ino: ino.0, block_idx: idx.0, generation: 0 }),
                });
                if let Ok(v) = token.parse() {
                    req.metadata_mut().insert("cluster-token", v);
                }
                let _ = client.delete_block(req).await;
            });
        }
    }
}

fn placement_to_block_loc(idx: BlockIdx, p: &BlockPlacement) -> BlockLoc {
    BlockLoc {
        block_idx: idx.0,
        primary:   p.primary.as_str().to_string(),
        replicas:  p.replicas.iter().map(|n| n.as_str().to_string()).collect(),
    }
}

fn not_found() -> Status {
    Status::not_found("inode not found")
}

fn kind_u32(k: InodeKind) -> u32 {
    match k {
        InodeKind::File    => 1,
        InodeKind::Dir     => 2,
        InodeKind::Symlink => 3,
    }
}

fn node_info_to_proto(ni: &NodeInfo) -> ProtoNodeInfo {
    use meta::node_info::Status as PStatus;
    ProtoNodeInfo {
        node_id:          ni.node_id.as_str().to_string(),
        addr:             ni.addr.clone(),
        status:           match ni.status {
            NodeStatus::Up   => PStatus::Up as i32,
            NodeStatus::Down => PStatus::Down as i32,
        },
        used_bytes:       ni.ram_used,
        capacity_bytes:   ni.ram_total,
        last_heartbeat_s: 0,
    }
}

#[tonic::async_trait]
impl Meta for MetaService {
    async fn lookup(&self, req: Request<LookupReq>) -> Result<Response<LookupResp>, Status> {
        let r = req.into_inner();
        let s = self.state.read().await;
        let parent_ino = InodeId(r.parent_ino);
        let child_ino = s
            .dirs
            .get(&parent_ino)
            .and_then(|d| d.get(&r.name))
            .copied()
            .ok_or_else(not_found)?;
        let inode = s.inodes.get(&child_ino).ok_or_else(not_found)?;
        Ok(Response::new(LookupResp { attr: Some(build_attr(inode)) }))
    }

    async fn get_attr(&self, req: Request<GetAttrReq>) -> Result<Response<Attr>, Status> {
        let r = req.into_inner();
        let s = self.state.read().await;
        let inode = s.inodes.get(&InodeId(r.ino)).ok_or_else(not_found)?;
        Ok(Response::new(build_attr(inode)))
    }

    async fn set_attr(&self, req: Request<SetAttrReq>) -> Result<Response<SetAttrResp>, Status> {
        let r = req.into_inner();
        let mut s = self.state.write().await;
        let now = SystemTime::now();
        let inode = s.inodes.get_mut(&InodeId(r.ino)).ok_or_else(not_found)?;

        if let Some(mode) = r.mode  { inode.mode = mode; }
        if let Some(uid)  = r.uid   { inode.uid  = uid; }
        if let Some(gid)  = r.gid   { inode.gid  = gid; }

        if let Some(s_val) = r.atime_s {
            let ns = r.atime_ns.unwrap_or(0);
            inode.atime = UNIX_EPOCH + Duration::new(s_val.max(0) as u64, ns);
        }
        if let Some(s_val) = r.mtime_s {
            let ns = r.mtime_ns.unwrap_or(0);
            inode.mtime = UNIX_EPOCH + Duration::new(s_val.max(0) as u64, ns);
        }

        let mut dropped_blocks: Vec<(BlockIdx, BlockPlacement)> = Vec::new();
        if let Some(new_size) = r.size {
            if new_size < inode.size {
                let block_size: u64 = 1 << 20;
                let last_idx = if new_size == 0 { 0 } else { (new_size - 1) / block_size };
                dropped_blocks = inode.blocks
                    .iter()
                    .filter(|(k, _)| k.0 > last_idx)
                    .map(|(&k, v)| (k, v.clone()))
                    .collect();
                inode.blocks.retain(|k, _| k.0 <= last_idx);
            }
            inode.size = new_size;
        }

        inode.ctime = now;
        let attr = build_attr(inode);
        let ino = InodeId(r.ino);

        if !dropped_blocks.is_empty() {
            let node_addrs: HashMap<NodeId, String> = s.nodes.values()
                .map(|n| (n.node_id.clone(), format!("http://{}", n.addr)))
                .collect();
            fire_delete_blocks(dropped_blocks, node_addrs, ino, self.token.clone());
        }

        Ok(Response::new(SetAttrResp { attr: Some(attr) }))
    }

    async fn create(&self, req: Request<CreateReq>) -> Result<Response<CreateResp>, Status> {
        let r = req.into_inner();
        let mut s = self.state.write().await;
        let parent_ino = InodeId(r.parent_ino);

        let dir = s.dirs.get(&parent_ino).ok_or_else(|| Status::not_found("parent not found"))?;
        if dir.contains_key(&r.name) {
            return Err(Status::already_exists("entry already exists"));
        }

        let now = SystemTime::now();
        let ino = s.alloc_ino();
        let inode = Inode {
            ino,
            kind: InodeKind::File,
            mode: 0o100000 | (r.mode & 0o7777),
            uid:  r.uid,
            gid:  r.gid,
            size: 0,
            nlink: 1,
            atime: now,
            mtime: now,
            ctime: now,
            generation: Generation(0),
            blocks: BTreeMap::new(),
            symlink_target: None,
        };
        let attr = build_attr(&inode);
        s.inodes.insert(ino, inode);
        s.dirs.get_mut(&parent_ino).unwrap().insert(r.name, ino);

        let fh = s.alloc_fh();
        s.open_handles.insert(fh, OpenHandleSt {
            fh,
            ino,
            flags: 0,
            generation_at_open: Generation(0),
        });

        Ok(Response::new(CreateResp { attr: Some(attr), fh, block_map: vec![] }))
    }

    async fn mkdir(&self, req: Request<MkdirReq>) -> Result<Response<Attr>, Status> {
        let r = req.into_inner();
        let mut s = self.state.write().await;
        let parent_ino = InodeId(r.parent_ino);

        {
            let dir = s.dirs.get(&parent_ino)
                .ok_or_else(|| Status::not_found("parent not found"))?;
            if dir.contains_key(&r.name) {
                return Err(Status::already_exists("entry already exists"));
            }
        }

        let now = SystemTime::now();
        let ino = s.alloc_ino();
        let inode = Inode {
            ino,
            kind: InodeKind::Dir,
            mode: 0o040000 | (r.mode & 0o7777),
            uid:  r.uid,
            gid:  r.gid,
            size: 4096,
            nlink: 2,
            atime: now,
            mtime: now,
            ctime: now,
            generation: Generation(0),
            blocks: BTreeMap::new(),
            symlink_target: None,
        };
        let attr = build_attr(&inode);
        s.inodes.insert(ino, inode);
        s.dirs.insert(ino, BTreeMap::new());
        s.dirs.get_mut(&parent_ino).unwrap().insert(r.name, ino);
        if let Some(parent) = s.inodes.get_mut(&parent_ino) {
            parent.nlink += 1;
        }

        Ok(Response::new(attr))
    }

    async fn unlink(&self, req: Request<UnlinkReq>) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let mut s = self.state.write().await;
        let parent_ino = InodeId(r.parent_ino);

        let child_ino = s
            .dirs
            .get(&parent_ino)
            .and_then(|d| d.get(&r.name))
            .copied()
            .ok_or_else(not_found)?;

        let kind = s
            .inodes
            .get(&child_ino)
            .map(|i| i.kind)
            .ok_or_else(not_found)?;

        if kind == InodeKind::Dir {
            return Err(Status::invalid_argument("is a directory"));
        }

        let file_blocks: Vec<(BlockIdx, BlockPlacement)> = s.inodes
            .get(&child_ino)
            .map(|i| i.blocks.iter().map(|(&k, v)| (k, v.clone())).collect())
            .unwrap_or_default();

        s.dirs.get_mut(&parent_ino).unwrap().remove(&r.name);
        s.inodes.remove(&child_ino);

        if !file_blocks.is_empty() {
            let node_addrs: HashMap<NodeId, String> = s.nodes.values()
                .map(|n| (n.node_id.clone(), format!("http://{}", n.addr)))
                .collect();
            fire_delete_blocks(file_blocks, node_addrs, child_ino, self.token.clone());
        }

        Ok(Response::new(Empty {}))
    }

    async fn rmdir(&self, req: Request<RmdirReq>) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let mut s = self.state.write().await;
        let parent_ino = InodeId(r.parent_ino);

        let child_ino = s
            .dirs
            .get(&parent_ino)
            .and_then(|d| d.get(&r.name))
            .copied()
            .ok_or_else(not_found)?;

        {
            let child_dir = s.dirs.get(&child_ino)
                .ok_or_else(|| Status::invalid_argument("not a directory"))?;
            if !child_dir.is_empty() {
                return Err(Status::failed_precondition("directory not empty"));
            }
        }

        s.dirs.remove(&child_ino);
        s.inodes.remove(&child_ino);
        s.dirs.get_mut(&parent_ino).unwrap().remove(&r.name);
        if let Some(parent) = s.inodes.get_mut(&parent_ino) {
            parent.nlink = parent.nlink.saturating_sub(1);
        }

        Ok(Response::new(Empty {}))
    }

    async fn rename(&self, req: Request<RenameReq>) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let mut s = self.state.write().await;
        let src_parent = InodeId(r.src_parent_ino);
        let dst_parent = InodeId(r.dst_parent_ino);

        let src_ino = s
            .dirs
            .get(&src_parent)
            .and_then(|d| d.get(&r.src_name))
            .copied()
            .ok_or_else(not_found)?;

        // If dst exists, remove it first (POSIX semantics).
        let mut dst_file_blocks: Vec<(BlockIdx, BlockPlacement)> = Vec::new();
        let mut dst_file_ino: Option<InodeId> = None;
        if let Some(&dst_ino) = s.dirs.get(&dst_parent).and_then(|d| d.get(&r.dst_name)) {
            let dst_kind = s.inodes.get(&dst_ino).map(|i| i.kind).ok_or_else(not_found)?;
            match dst_kind {
                InodeKind::Dir => {
                    let empty = s.dirs.get(&dst_ino).map_or(true, |d| d.is_empty());
                    if !empty {
                        return Err(Status::failed_precondition("destination directory not empty"));
                    }
                    s.dirs.remove(&dst_ino);
                    if let Some(p) = s.inodes.get_mut(&dst_parent) {
                        p.nlink = p.nlink.saturating_sub(1);
                    }
                }
                InodeKind::File => {
                    dst_file_blocks = s.inodes.get(&dst_ino)
                        .map(|i| i.blocks.iter().map(|(&k, v)| (k, v.clone())).collect())
                        .unwrap_or_default();
                    dst_file_ino = Some(dst_ino);
                }
                _ => {}
            }
            s.inodes.remove(&dst_ino);
            s.dirs.get_mut(&dst_parent).unwrap().remove(&r.dst_name);
        }

        // Move the entry.
        s.dirs.get_mut(&src_parent).unwrap().remove(&r.src_name);
        s.dirs.get_mut(&dst_parent)
            .ok_or_else(|| Status::not_found("dst parent not found"))?
            .insert(r.dst_name, src_ino);

        // Adjust nlink if moving a directory between parents.
        let moved_kind = s.inodes.get(&src_ino).map(|i| i.kind);
        if moved_kind == Some(InodeKind::Dir) && src_parent != dst_parent {
            if let Some(sp) = s.inodes.get_mut(&src_parent) { sp.nlink = sp.nlink.saturating_sub(1); }
            if let Some(dp) = s.inodes.get_mut(&dst_parent) { dp.nlink += 1; }
        }

        if !dst_file_blocks.is_empty() {
            if let Some(ino) = dst_file_ino {
                let node_addrs: HashMap<NodeId, String> = s.nodes.values()
                    .map(|n| (n.node_id.clone(), format!("http://{}", n.addr)))
                    .collect();
                fire_delete_blocks(dst_file_blocks, node_addrs, ino, self.token.clone());
            }
        }

        Ok(Response::new(Empty {}))
    }

    async fn read_dir(&self, req: Request<ReadDirReq>) -> Result<Response<ReadDirResp>, Status> {
        let r = req.into_inner();
        let s = self.state.read().await;
        let ino = InodeId(r.ino);
        let dir = s.dirs.get(&ino).ok_or_else(|| Status::not_found("directory not found"))?;

        let cookie_str = String::from_utf8(r.cookie.to_vec()).unwrap_or_default();
        let max = if r.max_entries == 0 { usize::MAX } else { r.max_entries as usize };

        let iter: Box<dyn Iterator<Item = (&String, &InodeId)>> = if cookie_str.is_empty() {
            Box::new(dir.iter())
        } else {
            Box::new(dir.range((std::ops::Bound::Excluded(cookie_str.clone()), std::ops::Bound::Unbounded)))
        };

        let mut entries = Vec::new();
        for (name, &child_ino) in iter.take(max) {
            let kind = s.inodes.get(&child_ino).map(|i| i.kind).unwrap_or(InodeKind::File);
            entries.push(DirEntry {
                name: name.clone(),
                ino:  child_ino.0,
                kind: kind_u32(kind),
            });
        }

        let eof = entries.len() < max;
        let next_cookie = entries
            .last()
            .map(|e| bytes::Bytes::copy_from_slice(e.name.as_bytes()))
            .unwrap_or_default();

        Ok(Response::new(ReadDirResp { entries, next_cookie, eof }))
    }

    async fn open(&self, req: Request<OpenReq>) -> Result<Response<OpenResp>, Status> {
        let r = req.into_inner();
        let ino = InodeId(r.ino);

        // Read inode info first, then write-lock to insert handle.
        let (attr, generation, block_map) = {
            let s = self.state.read().await;
            let inode = s.inodes.get(&ino).ok_or_else(not_found)?;
            if inode.kind != InodeKind::File {
                return Err(Status::invalid_argument("not a file"));
            }
            let block_map: Vec<BlockLoc> = inode.blocks.iter()
                .map(|(&idx, p)| placement_to_block_loc(idx, p))
                .collect();
            (build_attr(inode), inode.generation, block_map)
        };

        let fh = {
            let mut s = self.state.write().await;
            let fh = s.alloc_fh();
            s.open_handles.insert(fh, OpenHandleSt {
                fh,
                ino,
                flags: r.flags as i32,
                generation_at_open: generation,
            });
            fh
        };

        Ok(Response::new(OpenResp { attr: Some(attr), fh, block_map }))
    }

    async fn close(&self, req: Request<CloseReq>) -> Result<Response<CloseResp>, Status> {
        let r = req.into_inner();
        let mut s = self.state.write().await;

        let handle = s.open_handles.remove(&r.fh)
            .ok_or_else(|| Status::not_found("file handle not found"))?;
        let ino = handle.ino;

        if !r.written_block_idxs.is_empty() {
            let inode = s.inodes.get(&ino).ok_or_else(not_found)?;
            if inode.generation != Generation(r.expected_generation) {
                return Err(Status::failed_precondition("generation mismatch"));
            }

            let now = SystemTime::now();
            let req_mtime = UNIX_EPOCH + Duration::new(r.mtime_s.max(0) as u64, r.mtime_ns);
            let floor = now - Duration::from_secs(60);
            let new_mtime = req_mtime.max(floor);

            let inode = s.inodes.get_mut(&ino).unwrap();
            inode.generation = inode.generation.bump();
            inode.size = r.new_size;
            inode.mtime = new_mtime;
            inode.ctime = now;
        }

        let inode = s.inodes.get(&ino).ok_or_else(not_found)?;
        let attr = build_attr(inode);
        Ok(Response::new(CloseResp { attr: Some(attr) }))
    }

    async fn allocate_blocks(&self, req: Request<AllocReq>) -> Result<Response<AllocResp>, Status> {
        let r = req.into_inner();
        let mut s = self.state.write().await;
        let ino = InodeId(r.ino);

        if !s.inodes.contains_key(&ino) {
            return Err(not_found());
        }

        let idxs: Vec<BlockIdx> = r.block_idxs.iter().map(|&i| BlockIdx(i)).collect();
        let mut block_map = Vec::new();

        for idx in &idxs {
            if let Some(p) = s.inodes.get(&ino).and_then(|i| i.blocks.get(idx)) {
                block_map.push(placement_to_block_loc(*idx, p));
            } else {
                let new_placements = s.allocate_blocks(ino, &[*idx]);
                for (new_idx, placement) in new_placements {
                    block_map.push(placement_to_block_loc(new_idx, &placement));
                    s.inodes.get_mut(&ino).unwrap().blocks.insert(new_idx, placement);
                }
            }
        }

        Ok(Response::new(AllocResp { block_map }))
    }

    async fn heartbeat_node(
        &self,
        req: Request<HeartbeatReq>,
    ) -> Result<Response<HeartbeatResp>, Status> {
        let r = req.into_inner();
        let mut s = self.state.write().await;
        let node_id = NodeId::new(&r.node_id);
        s.nodes.insert(node_id.clone(), NodeInfo {
            node_id: node_id.clone(),
            addr:      r.addr,
            ram_used:  r.used_bytes,
            ram_total: r.capacity_bytes,
            status:    NodeStatus::Up,
        });
        s.last_heartbeat.insert(node_id, std::time::Instant::now());

        let nodes: Vec<ProtoNodeInfo> = s.nodes.values().map(node_info_to_proto).collect();
        Ok(Response::new(HeartbeatResp {
            cluster: Some(NodeList { nodes }),
        }))
    }

    async fn list_nodes(&self, _req: Request<Empty>) -> Result<Response<NodeList>, Status> {
        let s = self.state.read().await;
        let nodes = s
            .nodes
            .values()
            .filter(|n| n.status == NodeStatus::Up)
            .map(node_info_to_proto)
            .collect();
        Ok(Response::new(NodeList { nodes }))
    }
}
