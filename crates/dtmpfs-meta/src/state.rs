use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use dtmpfs_common::id::{BlockIdx, BlockKey, BlockPlacement, Generation, InodeId, InodeKind, NodeId};

pub struct MetaState {
    pub inodes:             HashMap<InodeId, Inode>,
    pub dirs:               HashMap<InodeId, BTreeMap<String, InodeId>>,
    pub next_ino:           AtomicU64,
    pub open_handles:       HashMap<u64, OpenHandleSt>,
    pub next_fh:            AtomicU64,
    pub nodes:              HashMap<NodeId, NodeInfo>,
    pub last_heartbeat:     HashMap<NodeId, Instant>,
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
pub enum NodeStatus {
    Up,
    Down,
}

impl MetaState {
    pub fn new(replication_factor: usize) -> Arc<RwLock<Self>> {
        let mut inodes = HashMap::new();
        let mut dirs = HashMap::new();
        let now = SystemTime::now();
        let root = Inode {
            ino: InodeId::ROOT,
            kind: InodeKind::Dir,
            mode: 0o40755,
            uid: 0,
            gid: 0,
            size: 4096,
            nlink: 2,
            atime: now,
            mtime: now,
            ctime: now,
            generation: Generation(0),
            blocks: BTreeMap::new(),
            symlink_target: None,
        };
        inodes.insert(InodeId::ROOT, root);
        dirs.insert(InodeId::ROOT, BTreeMap::new());
        Arc::new(RwLock::new(MetaState {
            inodes,
            dirs,
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
        self.nodes
            .values()
            .filter(|n| n.status == NodeStatus::Up)
            .map(|n| n.node_id.clone())
            .collect()
    }

    pub fn allocate_blocks(
        &self,
        ino: InodeId,
        idxs: &[BlockIdx],
    ) -> Vec<(BlockIdx, BlockPlacement)> {
        use dtmpfs_common::hash::pick_nodes;
        let live = self.live_nodes();
        let r = self.replication_factor;
        idxs.iter()
            .map(|&idx| {
                let key = BlockKey { ino, block_idx: idx, generation: Generation(0) };
                let chosen = pick_nodes(&key, &live, r);
                let placement = BlockPlacement {
                    primary: chosen
                        .first()
                        .cloned()
                        .unwrap_or_else(|| NodeId::new("?")),
                    replicas: chosen.into_iter().skip(1).collect(),
                };
                (idx, placement)
            })
            .collect()
    }
}

pub fn build_attr(inode: &Inode) -> dtmpfs_proto::meta::Attr {
    use dtmpfs_proto::meta::Attr;
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
        atime_s:    at_s,
        atime_ns:   at_ns,
        mtime_s:    mt_s,
        mtime_ns:   mt_ns,
        ctime_s:    ct_s,
        ctime_ns:   ct_ns,
    }
}

pub fn spawn_heartbeat_watcher(state: Arc<RwLock<MetaState>>, dead_after: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let now = Instant::now();
            let mut s = state.write().await;
            let downs: Vec<NodeId> = s
                .last_heartbeat
                .iter()
                .filter(|(_, last)| now.duration_since(**last) > dead_after)
                .map(|(id, _)| id.clone())
                .collect();
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
