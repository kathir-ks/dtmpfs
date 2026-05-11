use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use moka::sync::Cache;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tonic::transport::Channel;

use dtmpfs_common::error::DtmpfsError;
use dtmpfs_common::id::{BlockIdx, BlockPlacement, Generation, InodeId, NodeId};
use dtmpfs_proto::meta::meta_client::MetaClient;
use dtmpfs_proto::meta::{
    CreateReq, GetAttrReq, LookupReq, MkdirReq, OpenReq, ReadDirReq,
    RenameReq, RmdirReq, SetAttrReq, UnlinkReq, Empty,
};
use dtmpfs_proto::store::ReadBlockReq;
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
};

use crate::cache::CachedAttr;
use crate::client::StoreClientPool;
use crate::open_file::OpenFile;

const TTL_ZERO: Duration = Duration::from_secs(0);

pub struct DtmpfsFs {
    pub rt:                 Handle,
    pub meta:               Mutex<MetaClient<Channel>>,
    pub stores:             Arc<StoreClientPool>,
    pub attr_cache:         Cache<InodeId, CachedAttr>,
    pub block_cache:        Cache<(InodeId, Generation, BlockIdx), Bytes>,
    pub open_files:         DashMap<u64, Arc<Mutex<OpenFile>>>,
    pub block_size:         usize,
    pub replication_factor: usize,
    pub token:              String,
}

impl DtmpfsFs {
    pub fn authed<T>(&self, body: T) -> tonic::Request<T> {
        let mut r = tonic::Request::new(body);
        r.metadata_mut().insert(
            "cluster-token",
            self.token.parse().expect("cluster token is valid ascii"),
        );
        r
    }
}

fn proto_attr_to_fuse(a: &dtmpfs_proto::meta::Attr) -> FileAttr {
    let kind = if a.mode & 0o170000 == 0o040000 {
        FileType::Directory
    } else if a.mode & 0o170000 == 0o120000 {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };
    FileAttr {
        ino:     a.ino,
        size:    a.size,
        blocks:  a.blocks,
        atime:   SystemTime::UNIX_EPOCH + Duration::new(a.atime_s as u64, a.atime_ns),
        mtime:   SystemTime::UNIX_EPOCH + Duration::new(a.mtime_s as u64, a.mtime_ns),
        ctime:   SystemTime::UNIX_EPOCH + Duration::new(a.ctime_s as u64, a.ctime_ns),
        crtime:  UNIX_EPOCH,
        kind,
        perm:    (a.mode & 0o7777) as u16,
        nlink:   a.nlink,
        uid:     a.uid,
        gid:     a.gid,
        rdev:    0,
        blksize: 1 << 20,
        flags:   0,
    }
}

fn proto_kind_to_fuse(kind: u32) -> FileType {
    match kind {
        2 => FileType::Directory,
        3 => FileType::Symlink,
        _ => FileType::RegularFile,
    }
}

impl DtmpfsFs {
    pub async fn fetch_block(
        &self,
        of: &OpenFile,
        idx: BlockIdx,
    ) -> Result<Bytes, DtmpfsError> {
        if let Some(buf) = of.dirty.get(&idx) {
            return Ok(Bytes::copy_from_slice(buf));
        }
        if let Some(b) = self.block_cache.get(&(of.ino, of.generation, idx)) {
            return Ok(b);
        }
        // Lazily refresh store addresses if pool is empty (e.g. after client restart).
        if self.stores.addrs.load().is_empty() {
            if let Ok(resp) = self.meta.lock().await
                .list_nodes(self.authed(Empty {})).await
            {
                use std::collections::HashMap;
                let addr_map: HashMap<NodeId, String> = resp.into_inner().nodes.iter()
                    .map(|n| (NodeId(n.node_id.clone()), format!("http://{}", n.addr)))
                    .collect();
                self.stores.refresh_addrs(addr_map);
            }
        }
        let placement = of.block_map.get(&idx).ok_or(DtmpfsError::NotFound)?;
        let mut last_err = DtmpfsError::StoreUnavailable(placement.primary.clone());
        for node in std::iter::once(&placement.primary).chain(placement.replicas.iter()) {
            let mut client = match self.stores.get(node).await {
                Ok(c) => c,
                Err(e) => { last_err = e; continue; }
            };
            let req = self.authed(ReadBlockReq {
                key: Some(dtmpfs_proto::store::BlockKey {
                    ino:        of.ino.0,
                    block_idx:  idx.0,
                    generation: 0,
                }),
                offset: 0,
                len:    0,
            });
            match client.read_block(req).await {
                Ok(r) => {
                    let b = Bytes::from(r.into_inner().data.to_vec());
                    self.block_cache.insert((of.ino, of.generation, idx), b.clone());
                    return Ok(b);
                }
                Err(s) if s.code() == tonic::Code::NotFound => {
                    return Ok(Bytes::from(vec![0u8; self.block_size]));
                }
                Err(s) => {
                    // Evict cached channel so the next attempt reconnects.
                    self.stores.evict(node);
                    last_err = DtmpfsError::StoreUnavailable(node.clone());
                    tracing::warn!(node = node.as_str(), code = ?s.code(), "read_block failed, trying next replica");
                    continue;
                }
            }
        }
        Err(last_err)
    }

    pub async fn apply_write(
        &self,
        of: &mut OpenFile,
        offset: u64,
        data: &[u8],
    ) -> Result<u32, DtmpfsError> {
        let bs  = self.block_size as u64;
        let end = offset + data.len() as u64;
        let mut written = 0u32;
        let mut cur     = offset;

        while cur < end {
            let idx       = BlockIdx(cur / bs);
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
            cur     += chunk_len as u64;
        }
        of.size_hint = of.size_hint.max(end);
        Ok(written)
    }

    fn build_open_file(
        ino: InodeId,
        fh: u64,
        attr: &dtmpfs_proto::meta::Attr,
        block_map: Vec<dtmpfs_proto::meta::BlockLoc>,
        flags: i32,
    ) -> (u64, Arc<Mutex<OpenFile>>) {
        use std::collections::BTreeMap;
        let mut bm = BTreeMap::new();
        for loc in block_map {
            bm.insert(
                BlockIdx(loc.block_idx),
                BlockPlacement {
                    primary:  NodeId(loc.primary),
                    replicas: loc.replicas.into_iter().map(NodeId).collect(),
                },
            );
        }
        let of = OpenFile {
            ino,
            generation: Generation(attr.generation),
            block_map: bm,
            dirty: BTreeMap::new(),
            size_hint: attr.size,
            flags,
        };
        (fh, Arc::new(Mutex::new(of)))
    }
}

impl Filesystem for DtmpfsFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = name.to_string_lossy().to_string();
        match self.rt.block_on(async {
            let r = self.meta.lock().await
                .lookup(self.authed(LookupReq {
                    parent_ino: parent,
                    name: name_str,
                })).await;
            r
        }) {
            Ok(resp) => {
                if let Some(a) = resp.into_inner().attr {
                    let fa = proto_attr_to_fuse(&a);
                    self.attr_cache.insert(InodeId(fa.ino), CachedAttr {
                        attr: fa,
                        fetched: std::time::Instant::now(),
                    });
                    reply.entry(&TTL_ZERO, &fa, 0);
                } else {
                    reply.error(libc::ENOENT);
                }
            }
            Err(s) => reply.error(libc::c_int::from(DtmpfsError::from_status(s, None))),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        let inode = InodeId(ino);
        if let Some(cached) = self.attr_cache.get(&inode) {
            reply.attr(&TTL_ZERO, &cached.attr);
            return;
        }
        match self.rt.block_on(async {
            self.meta.lock().await
                .get_attr(self.authed(GetAttrReq { ino })).await
        }) {
            Ok(resp) => {
                let a = resp.into_inner();
                let fa = proto_attr_to_fuse(&a);
                self.attr_cache.insert(inode, CachedAttr {
                    attr: fa,
                    fetched: std::time::Instant::now(),
                });
                reply.attr(&TTL_ZERO, &fa);
            }
            Err(s) => reply.error(libc::c_int::from(DtmpfsError::from_status(s, None))),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let (atime_s, atime_ns) = match atime {
            Some(fuser::TimeOrNow::SpecificTime(t)) => {
                let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                (Some(d.as_secs() as i64), Some(d.subsec_nanos()))
            }
            _ => (None, None),
        };
        let (mtime_s, mtime_ns) = match mtime {
            Some(fuser::TimeOrNow::SpecificTime(t)) => {
                let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                (Some(d.as_secs() as i64), Some(d.subsec_nanos()))
            }
            _ => (None, None),
        };
        match self.rt.block_on(async {
            self.meta.lock().await
                .set_attr(self.authed(SetAttrReq {
                    ino, size, mode, uid, gid,
                    atime_s, atime_ns, mtime_s, mtime_ns,
                })).await
        }) {
            Ok(resp) => {
                if let Some(a) = resp.into_inner().attr {
                    let fa = proto_attr_to_fuse(&a);
                    self.attr_cache.insert(InodeId(ino), CachedAttr {
                        attr: fa,
                        fetched: std::time::Instant::now(),
                    });
                    reply.attr(&TTL_ZERO, &fa);
                } else {
                    reply.error(libc::EIO);
                }
            }
            Err(s) => reply.error(libc::c_int::from(DtmpfsError::from_status(s, None))),
        }
    }

    fn create(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let name_str = name.to_string_lossy().to_string();
        let uid = req.uid();
        let gid = req.gid();
        match self.rt.block_on(async {
            self.meta.lock().await
                .create(self.authed(CreateReq {
                    parent_ino: parent,
                    name: name_str,
                    mode, uid, gid,
                })).await
        }) {
            Ok(resp) => {
                let r = resp.into_inner();
                if let Some(ref a) = r.attr {
                    let fa = proto_attr_to_fuse(a);
                    let (fh, of_arc) = Self::build_open_file(
                        InodeId(a.ino), r.fh, a, r.block_map, flags,
                    );
                    self.open_files.insert(fh, of_arc);
                    self.attr_cache.insert(InodeId(fa.ino), CachedAttr {
                        attr: fa,
                        fetched: std::time::Instant::now(),
                    });
                    reply.created(&TTL_ZERO, &fa, 0, fh, 0);
                } else {
                    reply.error(libc::EIO);
                }
            }
            Err(s) => reply.error(libc::c_int::from(DtmpfsError::from_status(s, None))),
        }
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        match self.rt.block_on(async {
            self.meta.lock().await
                .open(self.authed(OpenReq { ino, flags: flags as u32 })).await
        }) {
            Ok(resp) => {
                let r = resp.into_inner();
                if let Some(ref a) = r.attr {
                    let (fh, of_arc) = Self::build_open_file(
                        InodeId(ino), r.fh, a, r.block_map, flags,
                    );
                    self.open_files.insert(fh, of_arc);
                    reply.opened(fh, 0);
                } else {
                    reply.error(libc::EIO);
                }
            }
            Err(s) => reply.error(libc::c_int::from(DtmpfsError::from_status(s, None))),
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.rt.block_on(async {
            let of_arc = self.open_files.get(&fh)
                .ok_or(DtmpfsError::NotFound)?
                .clone();
            let of = of_arc.lock().await;
            let bs  = self.block_size as u64;
            let off = offset as u64;
            let end = off + size as u64;
            let mut buf = Vec::with_capacity(size as usize);
            let mut cur = off;
            while cur < end {
                let idx      = BlockIdx(cur / bs);
                let blk_off  = idx.0 * bs;
                let in_block = (cur - blk_off) as usize;
                let want     = ((blk_off + bs).min(end) - cur) as usize;
                if of.block_map.contains_key(&idx) || of.dirty.contains_key(&idx) {
                    let block = self.fetch_block(&of, idx).await?;
                    let avail = block.len().saturating_sub(in_block);
                    let take  = want.min(avail);
                    if take > 0 {
                        buf.extend_from_slice(&block[in_block..in_block + take]);
                    }
                    if take < want {
                        buf.extend(std::iter::repeat(0u8).take(want - take));
                    }
                } else {
                    buf.extend(std::iter::repeat(0u8).take(want));
                }
                cur += want as u64;
            }
            Ok::<_, DtmpfsError>(buf)
        }) {
            Ok(data) => reply.data(&data),
            Err(e)   => reply.error(libc::c_int::from(e)),
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let data_owned = data.to_vec();
        match self.rt.block_on(async {
            let of_arc = self.open_files.get(&fh)
                .ok_or(DtmpfsError::NotFound)?
                .clone();
            let mut of = of_arc.lock().await;
            self.apply_write(&mut of, offset as u64, &data_owned).await
        }) {
            Ok(n)  => reply.written(n),
            Err(e) => reply.error(libc::c_int::from(e)),
        }
    }

    fn flush(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        match self.rt.block_on(self.flush_path(fh)) {
            Ok(())  => reply.ok(),
            Err(e)  => reply.error(libc::c_int::from(e)),
        }
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.rt.block_on(self.flush_path(fh)) {
            Ok(()) => {
                self.open_files.remove(&fh);
                reply.ok();
            }
            Err(e) => reply.error(libc::c_int::from(e)),
        }
    }

    fn fsync(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.rt.block_on(self.fsync_path(fh)) {
            Ok(())  => reply.ok(),
            Err(e)  => reply.error(libc::c_int::from(e)),
        }
    }

    fn mkdir(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name_str = name.to_string_lossy().to_string();
        let uid = req.uid();
        let gid = req.gid();
        match self.rt.block_on(async {
            self.meta.lock().await
                .mkdir(self.authed(MkdirReq {
                    parent_ino: parent,
                    name: name_str,
                    mode, uid, gid,
                })).await
        }) {
            Ok(resp) => {
                let a = resp.into_inner();
                let fa = proto_attr_to_fuse(&a);
                self.attr_cache.insert(InodeId(fa.ino), CachedAttr {
                    attr: fa,
                    fetched: std::time::Instant::now(),
                });
                reply.entry(&TTL_ZERO, &fa, 0);
            }
            Err(s) => reply.error(libc::c_int::from(DtmpfsError::from_status(s, None))),
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = name.to_string_lossy().to_string();
        match self.rt.block_on(async {
            self.meta.lock().await
                .unlink(self.authed(UnlinkReq {
                    parent_ino: parent,
                    name: name_str,
                })).await
        }) {
            Ok(_)  => reply.ok(),
            Err(s) => reply.error(libc::c_int::from(DtmpfsError::from_status(s, None))),
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = name.to_string_lossy().to_string();
        match self.rt.block_on(async {
            self.meta.lock().await
                .rmdir(self.authed(RmdirReq {
                    parent_ino: parent,
                    name: name_str,
                })).await
        }) {
            Ok(_)  => reply.ok(),
            Err(s) => reply.error(libc::c_int::from(DtmpfsError::from_status(s, None))),
        }
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let src_name = name.to_string_lossy().to_string();
        let dst_name = newname.to_string_lossy().to_string();
        match self.rt.block_on(async {
            self.meta.lock().await
                .rename(self.authed(RenameReq {
                    src_parent_ino: parent,
                    src_name,
                    dst_parent_ino: newparent,
                    dst_name,
                })).await
        }) {
            Ok(_)  => reply.ok(),
            Err(s) => reply.error(libc::c_int::from(DtmpfsError::from_status(s, None))),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        match self.rt.block_on(async {
            let mut entries = vec![];
            let mut cookie = bytes::Bytes::new();
            loop {
                let resp = self.meta.lock().await
                    .read_dir(self.authed(ReadDirReq {
                        ino,
                        cookie: cookie,
                        max_entries: 256,
                    })).await
                    .map_err(|s| DtmpfsError::from_status(s, None))?
                    .into_inner();
                entries.extend(resp.entries);
                if resp.eof { break; }
                cookie = bytes::Bytes::from(resp.next_cookie);
            }
            Ok::<_, DtmpfsError>(entries)
        }) {
            Ok(entries) => {
                for (i, e) in entries.into_iter().enumerate().skip(offset as usize) {
                    let kind = proto_kind_to_fuse(e.kind);
                    if reply.add(e.ino, (i + 1) as i64, kind, &e.name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(libc::c_int::from(e)),
        }
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        match self.rt.block_on(async {
            self.meta.lock().await
                .list_nodes(self.authed(Empty {})).await
        }) {
            Ok(resp) => {
                let nodes = resp.into_inner().nodes;
                let total: u64 = nodes.iter().map(|n| n.capacity_bytes).sum();
                let used:  u64 = nodes.iter().map(|n| n.used_bytes).sum();
                let free        = total.saturating_sub(used);
                let bs          = self.block_size as u64;
                reply.statfs(
                    total / bs,
                    free  / bs,
                    free  / bs,
                    0, 0,
                    bs as u32,
                    255,
                    0,
                );
            }
            Err(_) => reply.statfs(0, 0, 0, 0, 0, self.block_size as u32, 255, 0),
        }
    }

    fn link(
        &mut self,
        _req: &Request,
        _ino: u64,
        _newparent: u64,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(libc::EPERM);
    }

    fn setxattr(
        &mut self, _req: &Request, _ino: u64, _name: &OsStr,
        _value: &[u8], _flags: i32, _position: u32, reply: ReplyEmpty,
    ) { reply.error(libc::ENOSYS); }

    fn getxattr(
        &mut self, _req: &Request, _ino: u64, _name: &OsStr,
        _size: u32, reply: fuser::ReplyXattr,
    ) { reply.error(libc::ENOSYS); }

    fn listxattr(
        &mut self, _req: &Request, _ino: u64,
        _size: u32, reply: fuser::ReplyXattr,
    ) { reply.error(libc::ENOSYS); }

    fn removexattr(
        &mut self, _req: &Request, _ino: u64,
        _name: &OsStr, reply: ReplyEmpty,
    ) { reply.error(libc::ENOSYS); }
}
