# dtmpfs — Architecture

This document is the visual companion to [`HLD.md`](HLD.md). It contains the full system diagram, a table of who-talks-to-whom, sequence diagrams for every notable flow (including failure modes), the data-layout formula, the caching layers, the network model, and the threading model.

## 1. System architecture diagram

The diagram below shows two hosts each running a `dtmpfs-mount` client, with a meta server and several store servers reachable over the LAN. The kernel boundary is marked; the `/dev/fuse` channel is the only kernel-mediated path for an application syscall.

```
                        APPLICATION PROCESSES
                  (cp, dd, python, gcc, training scripts)
                                  |
                                  | open/read/write/close (libc syscalls)
                                  v
   ====================================================== kernel boundary =====
   |    Linux kernel: VFS dispatch -> fuse.ko character device /dev/fuse     |
   ============================================================================
                                  |
                                  | upcalls / replies on /dev/fuse
                                  v
   +------------------------------------------------------------------------+
   |                       dtmpfs-mount (one per host)                      |
   |                                                                        |
   |  +---------------+   +-----------------+   +------------------------+  |
   |  | fuser worker  |-->| sync FUSE        |-->| Handle::block_on(...)  |  |
   |  | threads (4)   |   | callbacks        |   |  -> async work on the |  |
   |  +---------------+   | (impl Filesystem |   |  shared tokio runtime |  |
   |                      |   for DtmpfsFs)  |   +-----------+-----------+  |
   |                      +-----------------+               |               |
   |                                                        v               |
   |  +---------------------+   +-----------------+   +-----------------+   |
   |  | AttrCache (1s TTL)  |   | OpenFile table  |   | BlockCache LRU  |   |
   |  | per ino             |   | per fh:         |   | key (ino, gen,  |   |
   |  +---------------------+   |  block_map      |   |       block_idx)|   |
   |                            |  dirty_blocks   |   +-----------------+   |
   |                            |  open_gen, etc. |                         |
   |                            +-----------------+                         |
   |                                                                        |
   |  +---------------------------------------------------------------+    |
   |  | gRPC clients (tonic): MetaClient, StoreClient pool by NodeId  |    |
   |  +---------------------------------------------------------------+    |
   +-----------------------------+------------------------------------------+
                                 |
                                 | gRPC over HTTP/2 (multiplexed)
                                 |   + cluster-token: <cluster_token>
                                 |
   ===================== TRUSTED LAN (no TLS in v1) ===========================
                                 |
       +-------------------------+--------------------------+
       |                                                    |
       v                                                    v
  +-----------------------------+        +--------------------------------+
  |     dtmpfs-meta (one)       |        |  dtmpfs-store * N              |
  |     listen :7100            |        |  listen :7200..:7200+N         |
  |                             |        |                                |
  |  MetaService (tonic)        |        |  StoreService (tonic)          |
  |    Lookup, GetAttr, ...     |        |    ReadBlock, WriteBlock,      |
  |    Open, Close              |        |    Replicate, DeleteBlock      |
  |                             |        |                                |
  |  RwLock<MetaState>          |        |  DashMap<BlockKey, Bytes>      |
  |    inodes, dirs,            |        |  heartbeat task ->             |
  |    open_handles, nodes,     |        |    Meta.HeartbeatNode()        |
  |    next_ino, next_fh,       |        |                                |
  |    HRW ring                 |        |  optional /debug/blocks (HTTP) |
  +-----------------------------+        +--------------------------------+
                ^                                          |
                |                                          | optional store->store
                |  Meta.HeartbeatNode every 1s             | Replicate (when R>=2)
                +------------------------------------------+
```

Default ports (overridable in TOML):

- `dtmpfs-meta`: gRPC `7100`
- `dtmpfs-store` (each): gRPC `7200 + index`; optional debug HTTP `7300 + index`
- `dtmpfs-mount`: client only, listens on nothing

## 2. Component interactions

The table below enumerates every caller -> callee edge and the protocol it uses. "Client" means `dtmpfs-mount`; "store" means `dtmpfs-store`; "meta" means `dtmpfs-meta`.

| Caller | Callee  | Protocol               | RPC / endpoint                                                                                              | Trigger                                                |
|--------|---------|------------------------|-------------------------------------------------------------------------------------------------------------|--------------------------------------------------------|
| client | meta    | gRPC `Meta`            | `Lookup`                                                                                                    | FUSE `lookup(parent, name)`                            |
| client | meta    | gRPC `Meta`            | `GetAttr`                                                                                                   | FUSE `getattr` cache miss                              |
| client | meta    | gRPC `Meta`            | `SetAttr`                                                                                                   | FUSE `setattr` (truncate, chmod, utimens)              |
| client | meta    | gRPC `Meta`            | `Create` / `Mkdir` / `Unlink` / `Rmdir` / `Rename`                                                          | corresponding FUSE call                                |
| client | meta    | gRPC `Meta`            | `ReadDir`                                                                                                   | FUSE `readdir`                                         |
| client | meta    | gRPC `Meta`            | `Open`                                                                                                      | FUSE `open` / `create`                                 |
| client | meta    | gRPC `Meta`            | `AllocateBlocks`                                                                                            | first time a new block_idx is dirtied during a flush   |
| client | meta    | gRPC `Meta`            | `Close`                                                                                                     | FUSE `release` or `flush` (explicit close)             |
| client | meta    | gRPC `Meta`            | `ListNodes`                                                                                                 | client startup (membership snapshot)                   |
| client | store   | gRPC `Store`           | `ReadBlock`                                                                                                 | BlockCache miss inside FUSE `read`                     |
| client | store   | gRPC `Store`           | `WriteBlock`                                                                                                | per-block during flush                                 |
| store  | meta    | gRPC `Meta`            | `HeartbeatNode`                                                                                             | every 1 s                                              |
| store  | store   | gRPC `Store`           | `Replicate`                                                                                                 | primary fan-out to replicas (Phase 5+)                 |
| client | store   | gRPC `Store`           | `DeleteBlock`                                                                                               | unlink path (eager Phase 5+)                           |
| operator | store | HTTP (debug)           | `GET /debug/blocks`                                                                                         | sharding sanity check (smoke)                          |

All gRPC calls carry `metadata.cluster-token = <cluster_token>` and are subject to per-call deadlines (default 5 s for control RPCs, 30 s for `WriteBlock` / `ReadBlock`).

## 3. Sequence diagrams

Each diagram below uses the convention:

```
  L = local kernel (FUSE)        Cl = dtmpfs-mount (client)
  M = dtmpfs-meta                 S0,S1 = dtmpfs-store nodes
```

Time flows top to bottom. `===>` is a request; `<===` is a response. `[..]` annotates the call.

### 3.1 Mount + ListNodes

When `dtmpfs-mount` starts, it loads its TOML config, opens a gRPC channel to `meta_addr`, fetches the current node membership, and only then calls `fuser::mount2`. Until the FUSE loop starts, no application traffic can reach it.

```
  Cl                              M
  |                               |
  | --- ListNodes() ------------->|
  |                               | snapshot self.state.nodes
  | <-- NodeList { nodes } -------|
  |                               |
  | build StoreClient pool keyed  |
  | by NodeId; HRW ring seeded    |
  |                               |
  | mount2(/mnt/dtmpfs, fs)       |
  | ...FUSE loop running          |
```

The client refreshes membership opportunistically: every `Open` carries a `BlockLoc` list grounded in meta's current view, which is always authoritative. A periodic `ListNodes` is not required for correctness.

### 3.2 Lookup + Open + Read of a small file (single block)

The application does `open("/mnt/dtmpfs/x", O_RDONLY)` then `read(fd, buf, 4096)`. File `x` is 1 KiB total, fits in block 0.

```
  app -> L                 Cl                    M                       S0
  open("/mnt/dtmpfs/x")
   ->  lookup(1, "x")
       |--- FUSE upcall -->|
                           | --- Meta.Lookup(parent=1, name="x") -->|
                           |                                         | inodes[ino_x] -> Attr
                           | <-- Attr(ino=42, mode, size=1024, ...) -|
       <-- entry --|
   ->  open(ino=42)
       |--- FUSE upcall -->|
                           | --- Meta.Open(ino=42, flags=RDONLY) ----->|
                           |                                            | open_handles[fh] = ...
                           | <-- OpenResp{attr,fh=7,block_map=[
                           |       BlockLoc{idx=0,primary="store-0",..} 
                           |     ]}
                           | OpenFile[fh=7] = { ino=42, gen=3,
                           |                    block_map, dirty={} }
       <-- fh=7 ----|
  read(fd, buf, 4096)
   ->  read(fh=7, off=0, sz=4096)
       |--- FUSE upcall -->|
                           | bc = BlockCache.get((42,3,0))
                           | miss
                           | --- Store.ReadBlock(BlockKey{42,0,3}) ---------------------------->|
                           |                                                                     | dashmap.get
                           | <-- ReadBlockResp{data: Bytes(1024), len:1024} ----------------------|
                           | BlockCache.insert((42,3,0), Bytes)
                           | slice [0..4096) -> only 1024 valid bytes
       <-- 1024 bytes ----|
  close(fd)
   ->  release(fh=7)
       |--- FUSE upcall -->|
                           | OpenFile[fh=7].dirty is empty
                           | --- Meta.Close(fh=7, dirty=false) ----->|
                           |                                          | drop open_handles[fh]
                           | <-- CloseResp{generation=3} ------------|
                           | drop OpenFile[fh=7]
       <-- ok ----|
```

### 3.3 Lookup + Open + Read of a large file (multi-block, parallel)

Same flow as above for `lookup`/`open`. The interesting piece is the multi-block read with `buffer_unordered(16)`.

```
  Cl                                                  S0      S1
  | OpenResp.block_map for inode I, gen=g:
  |   idx 0 primary=S0   replicas=[S1]
  |   idx 1 primary=S1   replicas=[S0]
  |   idx 2 primary=S0   replicas=[S1]
  |   idx 3 primary=S1   replicas=[S0]
  |
  | read(fh, off=0, sz=4*MiB)
  | block_idxs = [0, 1, 2, 3]
  | for each: BlockCache.get((I,g,idx)) -> all miss
  |
  | stream::iter(reqs).buffer_unordered(16)
  |     |  ReadBlock(I,0,g) ====>|
  |     |  ReadBlock(I,1,g) ============>|
  |     |  ReadBlock(I,2,g) ====>|
  |     |  ReadBlock(I,3,g) ============>|
  |     |                       |             |
  |     |  <==== resp(0)        |             |
  |     |                       |  <==== resp(1)
  |     |  <==== resp(2)        |             |
  |     |                       |  <==== resp(3)
  |
  | concat in offset order, slice as needed,
  | populate BlockCache with all 4 entries
  |
  | --- 4 MiB --> kernel
```

The four RPCs share at most two TCP connections (one per peer store) thanks to HTTP/2 multiplexing. With `R=1`, replicas are not consulted on the read path; with `R>=2` and a primary failure, the client retries against the next replica in `BlockLoc.replicas` (see §3.6).

### 3.4 Create + Write + Close (cross-host visibility)

Client A on host A writes a 3 MiB file. Client B on host B reads it after the writer closes.

```
  ClA                  M                     S0       S1                    ClB
  --- Meta.Create(parent=1, name="big",
                  mode=0644, uid, gid) ---->|
  <-- CreateResp{ino=99, fh=11, attr,
                 block_map=[]} -------------|
  OpenFile[fh=11] = {ino:99, open_gen:0,
                     block_map:[], dirty:{}}

  write(fh=11, off=0, data=3 MiB)
  - block 0,1,2 all dirty
  - none of them existed yet -> not RMW
  - dirty_blocks[0..2] = BytesMut(1 MiB each)
  return immediately, no RPC

  close(fh=11)
  - dirty.len = 3
  --- Meta.AllocateBlocks(ino=99,
                          indices=[0,1,2]) -->|
                                              | ring.place((99,0)) -> [S0,S1]
                                              | ring.place((99,1)) -> [S1,S0]
                                              | ring.place((99,2)) -> [S0,S1]
  <-- AllocResp{locs=[
        BlockLoc{idx=0,primary=S0,replicas=[]},
        BlockLoc{idx=1,primary=S1,replicas=[]},
        BlockLoc{idx=2,primary=S0,replicas=[]}]} R=1 so no replicas
  - WriteBlock fan-out, buffer_unordered(16):
                  WriteBlock(BlockKey{99,0,1}) ==> S0
                  WriteBlock(BlockKey{99,1,1}) ==> S1
                  WriteBlock(BlockKey{99,2,1}) ==> S0
                                              <== ok x3
  --- Meta.Close(fh=11,
                 new_size=3*MiB,
                 mtime=now,
                 written_idxs=[0,1,2]) ---->|
                                              | inode.size=3MiB
                                              | inode.blocks insert/update
                                              |   with placements above
                                              | inode.generation += 1   -> gen=1
  <-- CloseResp{generation=1} ---------------|

   ... time passes; ClB now does open("big") ...

                                                                         open("/mnt/dtmpfs/big")
                                                                         lookup(1,"big")
                                              | <-------- Meta.Lookup ----|
                                              | -- Attr(ino=99,size=3MiB,
                                              |    generation=1) -------->|
                                                                         open(99)
                                              | <-------- Meta.Open ------|
                                              | -- OpenResp{attr,fh=4,
                                              |    block_map=...} ------->|
                                                                         read covers all blocks
                                                                         for each idx: BlockCache miss,
                                                                         Store.ReadBlock as in 3.3
```

The crucial moment is `Meta.Close` bumping `generation` from 0 -> 1. ClB has never observed this inode before, so AttrCache is empty; its `Open` produces `attr.generation = 1`, and any reads it makes are keyed `(99, 1, idx)`. There is no shared `BlockCache` to invalidate; cache coherence is achieved by the cache key never colliding with a stale entry.

### 3.5 Concurrent close-to-open from two clients (the canonical race)

This is the hardest case to reason about. Both ClA and ClB open the same file at generation `g`, write to overlapping blocks, and close. Last-close-wins, per block.

```
  ClA                            M                                ClB
  open(I) -- Meta.Open --->|
  <-- OpenResp{gen=g} -----|
  open_gen_A = g, dirty={}

                            | <-- Meta.Open(I) ----- ClB
                            | -- OpenResp{gen=g} -->
                                                                  open_gen_B = g, dirty={}

  write(fh_A, off=0, "AAA")
  - block 0 RMW from gen g
  - dirty_blocks[0] = "AAA..." padded with old bytes from gen g

                                                                  write(fh_B, off=512, "BBB")
                                                                  - block 0 RMW from gen g
                                                                  - dirty_blocks[0] = "...BBB..."
                                                                    where the "..." is old bytes from gen g

  close(fh_A)
   - WriteBlock(BlockKey{I,0,g}) -> S0     [puts ClA's "AAA..." at gen g]
   --- Meta.Close(...) ----------->|
                                    | inode.gen = g+1
                                    | inode.blocks[0] gets BlockPlacement
                                    | refreshed (still S0 if HRW unchanged)
   <-- CloseResp{gen=g+1} ---------|

                                                                  close(fh_B)
                                                                  - WriteBlock(BlockKey{I,0,g}) -> S0
                                                                    (NB: ClB still uses its open-time gen,
                                                                     not the post-A gen)
                                                                                              !!!
                                    | <-- Meta.Close(...) ---- ClB
                                    | inode.gen = g+2
                                    | inode.blocks[0] BlockPlacement
                                    |   refreshed
                                    | -> CloseResp{gen=g+2} -->
```

What just happened on S0:

- After ClA's WriteBlock, the entry at `BlockKey{I,0,g}` is ClA's "AAA...".
- Before meta processed ClA's `Close`, both ClA and ClB held the same block (gen `g`) idea.
- ClB's WriteBlock then *overwrites* the entry at `BlockKey{I,0,g}` with ClB's "...BBB..." — and ClB's data was an RMW against the **pre-A** version of the block. ClA's bytes are lost from offset 0.

So: each client's RMW is consistent with the version it opened, but the on-store value is whichever finished writing last. This matches NFS close-to-open semantics. We document this as a known limitation (`docs/consistency.md`).

Phase 6 hardening introduces stale-write rejection on the store: if `WriteBlock` arrives with a `generation` older than what the store last saw for that `(ino, idx)`, it is rejected with `Status::failed_precondition`. The client treats this as a flush failure and surfaces `EIO`. This narrows the race to "first close wins" rather than "last write wins" but does not eliminate the lost-update problem; it merely makes one of the writers visibly fail rather than silently lose data.

### 3.6 Failure: store node dies mid-read

Two cases: `R=1` (no fallback) and `R=2` (transparent recovery).

#### 3.6.1 `R=1`

```
  Cl                                       S0
  | --- ReadBlock(BlockKey{I,k,g}) ====>X (TCP RST or timeout)
  | <-- Status::unavailable -------- (tonic transport error)
  | block_map[k].replicas is empty (R=1)
  | nothing to fall back to
  | return EIO to FUSE callback
  | (read syscall returns -1 with errno=EIO)
```

The read fails. The OpenFile remains open; subsequent reads of *other* blocks (on still-live stores) succeed. Subsequent reads of the same dead block also fail with EIO until the store comes back, OR until the file is re-opened post-meta-noticing-the-store-is-down (because by then the placement may have changed and the data is gone for `R=1`).

#### 3.6.2 `R=2`

```
  Cl                                       S0           S1
  | --- ReadBlock(BlockKey{I,k,g}) ====>X
  | <-- Status::unavailable
  | block_map[k].replicas = [S1]
  | --- ReadBlock(BlockKey{I,k,g}) =================>|
  |                                                   | dashmap.get -> Bytes
  | <-- ReadBlockResp{data,len} ----------------------|
  | BlockCache.insert((I,g,k), data)
  | return data to FUSE
```

The retry uses the next replica from `block_map[k].replicas`. The client does not call meta on this path; the `BlockLoc` it cached at `Open` is still authoritative for *this generation* of the block. After meta's heartbeat detection marks S0 down (5 missed heartbeats, ~5 s), subsequent `Open` calls produce `BlockLoc`s without S0, and writes are routed elsewhere.

### 3.7 Failure: meta node dies

```
  Cl                          M
  | --- Meta.Lookup(...) ====>X
  | <-- Status::unavailable
  | (or transport error)
  |
  | return EIO to FUSE for any
  | metadata-bearing call
```

In v1, the meta is a SPOF. The client does not have an alternative. Cached state (AttrCache, BlockCache, OpenFile entries) keeps in-flight reads/writes that don't need meta usable for a brief window — for example, reads of cached blocks against an already-open `fh` continue to succeed until the cache eviction or until the kernel's `entry_timeout`/`attr_timeout` expires and FUSE retries a `lookup`.

Once the meta restarts:

- `MetaState` is reconstructed from scratch (no persistence in v1). The inode table is empty, the directory tree is empty, `next_ino` resets.
- All client `OpenFile` handles refer to `fh` values the new meta does not know. The next `Close` returns `Status::not_found`; the client surfaces `EIO`.
- Stores' `DashMap<BlockKey, Bytes>` still holds blocks, but no `BlockMap` references them, so they are orphaned. Phase 6 GC sweeps them.

In short: a meta crash in v1 is equivalent to a cluster reset.

### 3.8 Heartbeat + membership update

```
  Sn (every 1s)                       M
  |                                    |
  | --- HeartbeatNode(NodeId,           |
  |       capacity_mb, used_mb,         |
  |       version) -------->|           |
  |                                    | nodes[Sn].last_seen = now
  | <-- HeartbeatResp{
  |       cluster_epoch,
  |       maybe new node list} ---|
  |                                    |
                                      every 1s, M's reaper task:
                                       for each node:
                                         if now - last_seen > 5s:
                                           nodes[node].state = Down
                                           ring.remove(node)
                                           epoch += 1
```

Clients learn about epoch changes opportunistically: every `Open` returns a `BlockLoc` consistent with meta's current epoch. There is no push from meta to clients in v1 (no streaming RPC, no callback). A client that holds a stale `block_map` for an open file and reads from a now-Down node falls into §3.6: `R>=2` recovers; `R=1` returns EIO until the file is re-opened.

## 4. Data layout

### 4.1 File -> blocks

A regular file's bytes are split into fixed-size blocks of `client.block_size` bytes (1 MiB in v1). Block N covers byte range `[N * block_size, (N+1) * block_size)`. The final block may be partial; `Inode.size` is the true byte length and is the source of truth, not `blocks.len() * block_size`.

```
  file inode I, size = 2.5 MiB, block_size = 1 MiB

   bytes:  [0........1MiB-1][1MiB...2MiB-1][2MiB..2.5MiB)
   blocks:    block 0           block 1        block 2 (512KiB)

   Inode I.blocks = {
     0 -> BlockPlacement { primary: S0, replicas: [...] },
     1 -> BlockPlacement { primary: S1, replicas: [...] },
     2 -> BlockPlacement { primary: S0, replicas: [...] },
   }
```

Holes (sparse files via `lseek` past EOF) are represented as missing entries in `Inode.blocks` for their indices. `read` of a hole returns zeroes; `write` of a partial block past EOF triggers RMW that materializes the block lazily.

### 4.2 Where block N of inode I lives — HRW

Placement is computed as follows (pseudocode; see [`LLD.md`](LLD.md) for the real signature):

```
  fn place(ino: u64, block_idx: u64, ring: &Ring, R: usize) -> Vec<NodeId> {
      // ring.live_nodes() returns currently-alive NodeIds.
      // For each node, compute h = hash64(ino, block_idx, node_id).
      // Sort nodes by h descending; take the first R.
      let mut weighted: Vec<(NodeId, u64)> = ring.live_nodes()
          .map(|n| (n.clone(), hash64(ino, block_idx, &n)))
          .collect();
      weighted.sort_by_key(|(_, h)| std::cmp::Reverse(*h));
      weighted.into_iter().take(R).map(|(n, _)| n).collect()
  }
```

Properties:

- Adding or removing one node re-homes only the blocks for which that node was in the top R; the rest are unaffected.
- Distribution is uniform (within hash quality) over inodes and block indices.
- Computing `place` is `O(N)` per call; for v1's N <= 8, this is in the noise.

The `hash64` function uses `xxh3` (or `siphash`; pinned in `dtmpfs-common::hash`).

### 4.3 Replica placement

When `R = 2`, `place` returns `[primary, replica1]`. When `R = 3`, `[primary, replica1, replica2]`. The client writes to the primary first and fans out to replicas; on `fsync`/`close`, the meta receives the `written_idxs` after primaries acknowledge.

Per `decisions.md` D3 and R13, the v1 replica-wait policy splits `flush` from `fsync`:

- **`flush` (called on `close(2)`)** — waits for **primaries only**. `Meta.Close` is called once primaries ack and bumps `Inode.generation` if the close flushed any dirty blocks. Replicas may still be in flight when `close()` returns to userspace; close-to-open visibility is preserved because subsequent opens hit the meta, which already has the new placement.
- **`fsync(2)` (and `fdatasync(2)`)** — same flush-then-`Meta.Close` path as above, but with `R≥2` the client waits for **all replicas** to ack before issuing `Meta.Close`. Equivalent to `flush()` on a single-replica cluster; stronger on a replicated cluster. Provides cross-host visibility on `fsync`, matching the behaviour databases and build tools expect.

In both cases `Meta.Close`-with-dirty bumps the generation counter under the meta's RwLock. There is no `fsync_wait_replicas` toggle in v1; the split is implicit in the `flush` vs `fsync` callbacks.

## 5. Caching layers

dtmpfs has three caches, each at a different layer, with different invalidation rules.

```
  +-----------------------------------------------------------------+
  | (1) Linux kernel page cache, per FUSE inode entry / attr        |
  |     - Populated by FUSE replies                                 |
  |     - TTL: entry_timeout=1s, attr_timeout=1s (set in mount opts)|
  |     - Invalidated by: TTL expiry, fuse_invalidate calls (NIY)   |
  +-----------------------------------------------------------------+
                            |
                            v
  +-----------------------------------------------------------------+
  | (2) Client AttrCache: HashMap<u64, (Attr, Instant)>             |
  |     - Keyed by inode                                            |
  |     - TTL: 1 s (configurable: client.attr_cache_ttl_ms)         |
  |     - BYPASSED on FUSE `open` (always Meta.Open the truth)      |
  |     - Updated on every Meta.* RPC that returns an Attr          |
  +-----------------------------------------------------------------+
                            |
                            v
  +-----------------------------------------------------------------+
  | (3) Client BlockCache: moka LRU                                 |
  |     - Key: (ino, generation, block_idx)                         |
  |     - Value: Bytes                                              |
  |     - Capacity: client.block_cache_capacity_mb (default 1024)   |
  |     - Eviction: moka W-TinyLFU                                  |
  |     - Invalidation by generation: bumping `generation` makes    |
  |       old entries unreachable; they age out via LRU.            |
  +-----------------------------------------------------------------+
```

Why no explicit `BlockCache` invalidation:

- The cache key includes `generation`. After `Meta.Close` bumps `generation` from `g` to `g+1`, the next `Open` returns `attr.generation = g+1`. Any `read` against this open uses keys `(ino, g+1, *)` — disjoint from `(ino, g, *)`. The old entries take RAM until they age out, but they cannot be incorrectly served.

Why AttrCache TTL is 1 s and bypassed on `open`:

- FUSE's kernel-side `attr_timeout` of 1 s means the kernel will re-call `getattr` at most once per second per inode. AttrCache sits behind that.
- `open` is the cache-coherence point of close-to-open. Bypassing the AttrCache on `open` guarantees that two `open` calls separated by another client's `close` see the new generation.

## 6. Network model

### 6.1 Transport

- **gRPC over HTTP/2** via `tonic`. One TCP connection per (client, server) pair, multiplexed by HTTP/2 streams.
- **Connection pooling** in the client: one `MetaClient` (one channel to meta) plus a `HashMap<NodeId, StoreClient>` populated lazily on first reference. The map is rebuilt when membership epoch changes.
- **No TLS in v1.** The transport is plaintext HTTP/2 (h2c).
- **Deadlines.** Every RPC has a timeout: 5 s for control RPCs (`Lookup`, `GetAttr`, `Open`, `Close`, etc.), 30 s for data RPCs (`ReadBlock`, `WriteBlock`). Configurable.
- **Keepalive.** HTTP/2 PING every 30 s; idle disconnect at 10 min. `tonic` defaults are fine; we override only if profiling shows reconnect storms.

### 6.2 Trust model

- **`cluster_token`** is a static shared secret carried in the gRPC metadata header `cluster-token` on every RPC. Both server roles validate the header and return `Status::unauthenticated` on mismatch.
- The token is loaded from each role's TOML at startup. Changing the token requires a coordinated restart.
- The token defends against accidental cross-cluster traffic and casual eavesdropping by tools that don't know the value, **not** against an attacker on the wire. There is no replay protection, no mutual auth, no encryption.
- All hosts are assumed equally trusted. There is no per-host isolation: any host with the token can read any block.

### 6.3 Ports

| Role          | Default port | Protocol     | Notes                                    |
|---------------|--------------|--------------|------------------------------------------|
| `dtmpfs-meta` | 7100         | gRPC h2c     | Single port; configurable in `meta.toml` |
| `dtmpfs-store`| 7200 + idx   | gRPC h2c     | Per-instance index; configurable         |
| `dtmpfs-store`| 7300 + idx   | HTTP/1.1     | Optional `/debug/blocks` endpoint        |
| `dtmpfs-mount`| (none)       | -            | Outbound only                            |

## 7. Threading model

This is the most subtle part of the client; getting it wrong leads to deadlock or data races. The plan's chosen approach (`tokio::runtime::Handle::block_on` from FUSE worker threads) is sketched below in detail.

### 7.1 Components

- **fuser worker pool.** `fuser` 0.14 spawns N OS threads (config: `client.fuse_threads`, default 4). Each thread loops on `read(/dev/fuse)`, decodes a request, calls a method on `impl Filesystem for DtmpfsFs` synchronously, and writes the reply back to `/dev/fuse`.
- **Tokio runtime.** One `tokio::runtime::Runtime` is built at startup with `Builder::new_multi_thread().enable_all()`. The number of tokio worker threads defaults to `num_cpus::get()`. The runtime owns the gRPC client tasks and the cache state.
- **gRPC tasks.** `tonic` clients run as tokio tasks; each in-flight RPC is one future scheduled on the runtime.
- **Meta and store servers** each have their own runtime within their own process; this section describes the client only.

### 7.2 Diagram

```
  +-- dtmpfs-mount process ----------------------------------------+
  |                                                                 |
  | fuser worker threads (OS threads, NOT tokio workers):           |
  |                                                                 |
  |   Thread fuse-0   Thread fuse-1   Thread fuse-2   Thread fuse-3 |
  |       |               |               |               |         |
  |       |               |               |               |         |
  |       v               v               v               v         |
  |   read(/dev/fuse) returns one request per worker; each thread   |
  |   calls a method on DtmpfsFs which is sync (Filesystem trait    |
  |   methods are sync). Inside that method:                        |
  |                                                                 |
  |       handle.block_on(async {                                   |
  |           // ... use tonic clients, await RPCs ...              |
  |       })                                                        |
  |                                                                 |
  |   The block_on parks the FUSE worker thread until the future    |
  |   completes, but inside the future we are on the tokio runtime  |
  |   and free to use buffer_unordered / spawn / awaits.            |
  |                                                                 |
  +---------------------------+-------------------------------------+
                              |
                              | Handle (cheap clone, Arc inside)
                              v
  +----------------------------------------------------------------+
  | Tokio runtime (multi-thread):                                  |
  |                                                                |
  |   Worker T0   Worker T1   Worker T2   Worker T3                |
  |   - executes the futures spawned by block_on                    |
  |   - hosts tonic client tasks                                    |
  |   - hosts AttrCache / BlockCache (interior mutability)          |
  +----------------------------------------------------------------+
```

### 7.3 The handoff

The critical rule: **fuser worker threads must not be tokio worker threads**. Calling `Handle::block_on` from a tokio worker thread deadlocks if the awaited future requires the runtime that the calling thread belongs to.

Because `fuser` spawns dedicated OS threads outside the tokio runtime, `block_on` is safe: it blocks the FUSE worker, but the tokio runtime continues servicing the future on its own workers.

```
   FUSE worker thread (sync)                Tokio runtime workers (async)
   -----------------------------            -----------------------------------
   on FUSE call (e.g. read)                 (idle, ready)
       |
       v
   handle.block_on(async {
       let f = client.read_blocks(...);     // schedules tasks on runtime
                                            (runtime picks up tasks,
                                             executes ReadBlock RPC futures)
       f.await                              <-- result ready, future completes
   })
       |
       v
   continue in FUSE worker, write reply
   into /dev/fuse, loop for next request
```

### 7.4 Sync FUSE callback wrapping async RPC — read path skeleton

```
  impl Filesystem for DtmpfsFs {
      fn read(&mut self, _req: &Request, ino: u64, fh: u64,
              offset: i64, size: u32, _flags: i32, _lock: Option<u64>,
              reply: ReplyData) {
          // sync context (fuser worker thread)
          let handle = self.runtime_handle.clone();
          let result: Result<Bytes, Errno> = handle.block_on(async {
              self.read_inner(ino, fh, offset as u64, size as usize).await
          });
          match result {
              Ok(bytes) => reply.data(&bytes),
              Err(e)    => reply.error(e.into()),
          }
      }
  }
```

`read_inner` is plain async Rust: it consults the BlockCache, issues `Store.ReadBlock` futures, awaits them, returns. Inside `read_inner` we are free to use `tokio::join!`, `buffer_unordered`, etc.

### 7.5 Locking discipline

- `MetaState` on the meta server is a single `tokio::sync::RwLock`. Writers (Create, Close, etc.) take the write lock; readers (Lookup, GetAttr) take the read lock. We have no nested locking from the meta server outwards (the meta does not call out to stores in v1 except via heartbeat replies, which do not need the lock).
- `DashMap<BlockKey, Bytes>` on the store is lock-free at the map level; per-bucket short critical sections.
- On the client, `AttrCache` and `BlockCache` are `moka::sync::Cache` (lock-free, with internal sharding). The `OpenFile` table is `dashmap::DashMap<u64, OpenFile>` keyed by `fh`. A single `OpenFile`'s `dirty_blocks` is mutated only by FUSE workers operating on that `fh`; we rely on FUSE not multiplexing one `fh` across worker threads (it serializes per-`fh` requests).

### 7.6 Backpressure

Two backpressure points to mind:

- **Per-flush parallelism.** `buffer_unordered(16)` caps concurrent block writes at 16 per flush. Hardcoded today; OQ-7 proposes making this configurable.
- **AttrCache / BlockCache size.** Bounded by `client.block_cache_capacity_mb`; LRU eviction handles overflow.

There is no rate-limiter on FUSE callbacks themselves; if the application drives faster than the gRPC fan-out can handle, the FUSE worker blocks in `block_on`, which naturally propagates as latency to the application syscall. This is the desired behavior.

## 8. Crosslinks

For everything not covered here:

- Struct definitions, algorithm pseudocode, lock ordering: [`LLD.md`](LLD.md).
- Wire types, every RPC field's meaning: [`protocol.md`](protocol.md).
- The full close-to-open analysis and the canonical race in §3.5 expanded: [`consistency.md`](consistency.md).
- Per-failure-mode behaviour (timeouts, retries, what surfaces as `EIO` vs `ENOSPC` vs `EAGAIN`): [`failure-model.md`](failure-model.md).
- Deployment, log levels, metrics, troubleshooting runbooks: [`operations.md`](operations.md).
- Every TOML knob: [`configuration.md`](configuration.md).
- Test strategy and concrete acceptance tests per phase: [`testing.md`](testing.md), [`acceptance-tests.md`](acceptance-tests.md).
