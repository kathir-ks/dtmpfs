# dtmpfs — High-Level Design

This document is the entry point for evaluating dtmpfs. It explains what the system is for, where it sits in the storage landscape, the architecture, and the load-bearing design decisions. It is paired with [`architecture.md`](architecture.md), which contains diagrams and step-by-step dataflow walkthroughs.

## 1. Overview & goals

### 1.1 Problem statement

Single-host `tmpfs(5)` is fast but local: a process on host A cannot read a file written to `/dev/shm` on host B. NFS, Ceph, and Lustre solve sharing but pay for it with disk I/O, complex deployment, and operational overhead that is wasteful when the data only needs to live for minutes and the working set fits in cluster RAM.

dtmpfs targets the gap: a small, trusted-LAN cluster (2–8 hosts) wants a **shared, RAM-backed scratch namespace** with POSIX semantics close enough that standard tools (`cp`, `dd`, `python open()`, `tar`) work without modification. We trade durability and strong consistency for simplicity and latency.

### 1.2 Target use cases

- **ML scratch.** Worker hosts in a training job stage tensors, dataset shards, intermediate checkpoints, and pre-processed batches that another worker will consume within seconds-to-minutes. The data is regenerable; durability is not required.
- **Distributed build cache.** Object files, compiled artefacts, container layers being assembled. Hot working set in RAM; cold tier elsewhere.
- **Generic POSIX share.** A short-lived shared namespace for ad-hoc cluster scripts that pre-date the team having a "real" distributed FS.

### 1.3 Goals (v1)

- POSIX-ish file and directory operations, sufficient for the tools above.
- Single namespace, mountable on N hosts simultaneously.
- Cross-host visibility on close (close-to-open consistency).
- Sharded data across storage nodes; optional replication.
- Sub-millisecond metadata operations on a quiet cluster.
- Read throughput approaching wire speed at 1 MiB block size.
- Operable by one person from a `cargo build` and three TOML files.

### 1.4 Non-goals (v1)

- Durability of any kind (no disk, no journal, no on-platter `fsync`).
- Strong consistency (no linearizability, no leases, no byte-range locks).
- Wide-area or untrusted-network deployment (no TLS, no auth beyond a shared token).
- High-availability metadata (single meta process is a SPOF; Raft is Phase 7).
- Snapshots, clones, quotas, hardlinks, xattrs, ACLs.
- Mac, Windows, BSD. Linux only.
- `mmap` writeback. Reads via `mmap` of small files may work via the kernel page cache; relying on it is unsupported.

A more thorough list lives in §12.

## 2. Glossary

- **Inode** — the metadata record for a single file or directory. Identified by a 64-bit `ino`. Owned by `dtmpfs-meta`. Carries `size`, `mode`, timestamps, `nlink`, and (for files) the ordered map of block indices to placements.
- **Block** — a fixed-size (1 MiB in v1) contiguous chunk of a file. Block N of inode I covers byte range `[N * block_size, (N+1) * block_size)`. Stored on `dtmpfs-store` as a `Bytes` value keyed by `BlockKey { ino, block_idx, generation }`.
- **Generation** — a monotonically-increasing 64-bit counter on each file inode. Bumped by `Meta.Close` whenever the close flushed dirty blocks. Used as a cache-key component on the client (`BlockCache`) and as a freshness check on the store.
- **Primary** — the storage node returned first in a `BlockLoc.replicas` list (or in `BlockLoc.primary`). Reads target the primary first; writes go to primary then replicas.
- **Replica** — a non-primary node carrying a copy of a block. Used for read failover when `R >= 2` and the primary is unreachable.
- **Store node (`dtmpfs-store`)** — a process whose only job is to hold blocks in a `DashMap<BlockKey, Bytes>` and answer `Store.ReadBlock` / `Store.WriteBlock` / `Store.Replicate` / `Store.DeleteBlock`.
- **Meta node (`dtmpfs-meta`)** — the single process that owns the inode table, directory entries, file-handle table, node membership, and the placement ring. Implements the `Meta` gRPC service.
- **Client (`dtmpfs-mount`)** — the FUSE process that mounts dtmpfs at a path. Bridges kernel FUSE callbacks to gRPC RPCs against the meta and store services.
- **FUSE** — Filesystem in Userspace. The kernel module `fuse` plus userspace library `libfuse3`. Lets `dtmpfs-mount` implement a filesystem in user code without writing a kernel module.
- **HRW (Highest Random Weight, a.k.a. Rendezvous hashing)** — a placement function that, given a key and a list of nodes, returns a deterministic ranked permutation of the nodes for that key. Used in `dtmpfs-common::hash` to choose the primary and replicas for `(ino, block_idx)`.
- **Close-to-open consistency** — the rule that all writes performed by a writer become visible to a subsequent `open()` on any client, and not before. The same model NFSv3 uses.
- **AttrCache** — short-TTL (1 s) per-client cache of `Attr` keyed by `ino`. Bypassed on `open`.
- **BlockCache** — per-client LRU keyed by `(ino, generation, block_idx)` storing decoded block payloads.
- **Cluster token** — a static shared secret carried in a gRPC metadata header (`cluster-token`) on every RPC. The only authentication in v1.

## 3. Stakeholders & user personas

- **Operator.** Runs `metasrv` and `storesrv` on the cluster hosts, manages configs, watches logs. Cares about deployment friction, restart safety, and observability. Typically a platform engineer or ML infra team member.
- **Mount user.** Runs `dtmpfs-mount` on a host to expose `/mnt/dtmpfs`. Often the same person as the operator on a small cluster. Cares that the mount comes up and stays up.
- **Application.** Any process on a host with the mount visible. Sees a POSIX filesystem and uses it via `open`/`read`/`write`/`close`. Does not know dtmpfs exists. ML training jobs, compilers, data-processing pipelines.
- **dtmpfs developer.** Reads this doc, the LLD, and the protocol doc. Cares about invariants, the close-to-open contract, and which subsystems are safe to refactor.

## 4. System context

dtmpfs is a userspace daemon set with a kernel-mediated front door. The diagram below shows the boundary of the system (everything inside the dashed box) and what's outside.

```
                  +----------------------------------------------------------+
                  |                            host                           |
                  |                                                           |
  application ----|-> POSIX syscalls                                          |
   process        |   (open/read/write/close)                                 |
                  |        |                                                  |
                  |        v                                                  |
                  |   +----------+   /dev/fuse    +-----------------------+   |
                  |   |  Linux   |<-------------->|  dtmpfs-mount         |   |
                  |   |  kernel  |  upcalls       |  (libfuse3 + fuser    |   |
                  |   |  VFS+    |                |   + tokio runtime)    |   |
                  |   |  fuse.ko |                |                       |   |
                  |   +----------+                +-----------+-----------+   |
                  |                                           | gRPC          |
                  |                                           | over HTTP/2   |
                  |                          - - - - - - - - -|- - - - - - -  |
                  +----------------------------------------------------------+
                                                              |
                                            LAN (trusted, no TLS in v1)
                                                              |
                              +-------------------------------+----------+
                              |                                          |
                       +------v---------+                       +--------v-------+
                       | dtmpfs-meta    |                       | dtmpfs-store * |
                       | port 7100      |                       | port 7200..    |
                       +----------------+                       +----------------+
```

Outside the system:

- **Linux kernel** — VFS layer, `fuse.ko`, page cache. dtmpfs treats kernel caching as advisory; correctness comes from generation bumps.
- **The application** — uses POSIX syscalls. dtmpfs is invisible to it except through the path it mounts at and the latency profile.
- **The network** — assumed trusted, low-latency LAN. The protocol does not defend against adversaries; it expects only honest packet loss and node crashes.
- **The operator's tooling** — process supervision (systemd, tmux, k8s), log shipping, metrics scraping. Out of scope for v1; we emit `tracing` logs and that is all.

## 5. Functional requirements

- **F1.** The filesystem is mountable on N hosts simultaneously, all sharing a single namespace rooted at `ino = 1`.
- **F2.** The following POSIX operations behave correctly: `open`, `read`, `write`, `close`, `fsync`, `flush`, `release`, `mkdir`, `rmdir`, `unlink`, `rename`, `lookup`, `getattr`, `setattr` (truncate, chmod, utimens), `readdir`, `opendir`, `releasedir`, `mknod` (regular file), `create`, `statfs`.
- **F3.** Writes performed by a client become visible to subsequent `open()` calls on any client once `close()` (or `fsync()`) on the writer's file descriptor has returned successfully (close-to-open).
- **F4.** File data is sharded across storage nodes by HRW on `(ino, block_idx)`. Any storage node holds approximately `1/N` of the data assuming uniform inode/block distribution.
- **F5.** Replication factor is configurable per-cluster: `R ∈ {1, 2, 3}`. With `R >= 2`, a read can fall back from primary to a replica on transport failure to the primary.
- **F6.** The meta service supports `Lookup`, `GetAttr`, `SetAttr`, `Create`, `Mkdir`, `Unlink`, `Rmdir`, `Rename`, `ReadDir`, `Open`, `Close`, `AllocateBlocks`, `HeartbeatNode`, `ListNodes`.
- **F7.** The store service supports `ReadBlock`, `WriteBlock`, `DeleteBlock`, `Replicate`, `Stat`.
- **F8.** Symlinks are supported via the `Inode.symlink_target` field; hardlinks (`link`) return `EPERM`. Xattrs return `ENOSYS`.
- **F9.** A `cluster_token` shared secret is enforced on every RPC by both meta and store. Mismatched tokens return `Status::unauthenticated`.
- **F10.** A debug HTTP endpoint `GET /debug/blocks` on each `dtmpfs-store` returns the list of block keys it holds. Used by smoke tests to verify HRW distribution.
- **F11.** `dtmpfs-mount` exits cleanly on `SIGTERM` and `SIGINT`, calling `fuser`'s unmount via `MountOption::AutoUnmount`. The kernel's `fusermount3 -u` is also supported as an external unmount path.
- **F12.** Inode allocation is monotonic from `next_ino = 2` (1 is reserved for the root). Inode numbers are not reused; once an inode is unlinked, its number is gone for the cluster lifetime. With u64 numbers and one-allocation-per-create this is safe for any realistic uptime.

## 6. Non-functional requirements

- **NFR-perf-1.** Sequential read of a 1 GiB file on a 10 GbE LAN at 1 MiB block size and `R=1` should approach line rate (target: ≥ 800 MiB/s sustained). Single-VM loopback target: ≥ 500 MiB/s.
- **NFR-perf-2.** Metadata operations (`getattr`, `lookup`, `mkdir`) on a quiet cluster should complete in under 1 ms p99 over loopback, under 2 ms p99 over LAN.
- **NFR-perf-3.** A `close()` that flushes K dirty blocks should complete in `O(K / parallelism)` round-trips. The client uses `buffer_unordered(16)` so up to 16 stores can be hit concurrently.
- **NFR-scale-1.** v1 is sized for 2–8 nodes. The meta service is a single process with a `tokio::sync::RwLock<MetaState>`; we expect this to handle low thousands of metadata ops/sec before contention. Larger clusters require Phase 7 (Raft) or sharded meta, both out of v1 scope.
- **NFR-scale-2.** Per-store memory footprint is bounded by configuration (`ram_budget_bytes`, in bytes). Eviction is **not** implemented in v1; running out of capacity causes `WriteBlock` to return `Status::resource_exhausted`.
- **NFR-avail-1.** dtmpfs is **CP** in CAP terms during a network partition. Only the side reachable from the meta node makes progress; the other side returns `EIO`. This is a v1 simplification, not a long-term stance.
- **NFR-avail-2.** Loss of a single store with `R=1` causes EIO on reads of the affected blocks; with `R>=2` reads transparently fail over. New writes route around the dead node via heartbeat-driven membership updates.
- **NFR-avail-3.** Loss of the meta process kills the entire FS until restart. v1 accepts this; Phase 7 fixes it.
- **NFR-sec.** Trusted-LAN model. The cluster token defends against accidental cross-cluster traffic, not against an attacker on the wire. There is no TLS, no per-user authentication, no encryption-at-rest (no rest exists). Filesystem permissions are checked against `Attr.{mode, uid, gid}` via `MountOption::DefaultPermissions` (kernel checks them) but are advisory in a multi-host trust sense.

### 6.1 Performance targets, in detail

Per `decisions.md` D4, the v1 committed performance bar is the lower, achievable-on-commodity-LAN target set. Aspirational headroom (e.g., io_uring / zero-copy work) lives outside the v1 NFR contract.

| Metric                                                       | Target           |
|--------------------------------------------------------------|------------------|
| Loopback sequential read (1 MiB blocks, 1 GB file)           | ≥ 500 MiB/s      |
| Loopback sequential write (1 MiB blocks, 1 GB file)          | ≥ 300 MiB/s      |
| 10 GbE LAN sequential read                                   | ≥ 800 MiB/s      |
| 10 GbE LAN sequential write                                  | ≥ 400 MiB/s      |
| `Meta.Lookup` p99 latency                                    | < 5 ms           |
| `Meta.Open` p99 latency                                      | < 10 ms          |
| `Store.ReadBlock` p99 latency (warm)                         | < 2 ms           |
| Mount-to-first-syscall                                       | < 1 s            |

These are the v1 contract. They exist so that regressions are caught by the smoke tests in `acceptance-tests.md`. v1 does not claim to be a high-performance distributed FS; it claims to be useful enough to ship and instrument.

### 6.2 Memory model

- Each `dtmpfs-store` reserves `ram_budget_bytes` of RAM (units: bytes). Exceeding this returns `Status::resource_exhausted` on `WriteBlock`; the client surfaces `ENOSPC`.
- Each `dtmpfs-mount` allocates `client.block_cache_capacity_mb` for its `BlockCache` (default 1024 MiB) plus a small overhead for the AttrCache and OpenFile table. The BlockCache uses `moka`'s W-TinyLFU; eviction is automatic.
- The meta server's RAM is bounded by the inode table size: roughly 256 bytes per inode plus ~64 bytes per directory entry plus ~48 bytes per `BlockPlacement` per block. A 100k-file cluster with average 4 blocks/file consumes ~70 MiB on the meta. Negligible for v1 targets.

## 7. Architecture overview

### 7.0 Role separation rationale

The three-role split (meta, store, client) is load-bearing for several reasons:

- **Different failure cardinalities.** The meta is one process; clients and stores scale horizontally. Coupling meta into the client process would force every host with a mount to also be the metadata authority — incompatible with the topology.
- **Different memory profiles.** The meta holds a small map of inodes; the store holds gigabytes of blocks. Per-process tuning (`malloc` arenas, allocator choice, OOM budgets) differs.
- **Different resource sensitivity.** A store under heavy I/O should not slow down metadata operations on an unrelated client. Process-level isolation gives this for free.
- **Different upgrade cadence.** The wire protocol pins the boundaries; we can swap a store implementation (e.g., back it with a slab allocator instead of `Bytes`) without touching meta or client.

dtmpfs has three roles. Each role is one Rust crate producing one binary.

```
   +---------------------+         +---------------------+         +---------------------+
   |   dtmpfs-client     |         |    dtmpfs-meta      |         |   dtmpfs-store      |
   |   (dtmpfs-mount)    |         |     (metasrv)       |         |    (storesrv)       |
   +---------------------+         +---------------------+         +---------------------+
   | impl Filesystem     |         | MetaService impl    |         | StoreService impl   |
   | AttrCache (1s TTL)  |         | RwLock<MetaState>:  |         | DashMap<BlockKey,   |
   | BlockCache (LRU)    |         |   inodes            |         |          Bytes>     |
   | OpenFile table:     |         |   dirs              |         | local heartbeat     |
   |   block_map         |         |   open_handles      |         | optional store->    |
   |   dirty_blocks      |         |   nodes (membership)|         |   store Replicate   |
   | tokio Handle        |         |   ring (HRW seed)   |         +---------------------+
   |                     |         |   next_ino,next_fh  |
   | Talks to: meta,     |         | Talks to: nobody    |         | Talks to: peer
   |  stores             |         |  (servers only;     |         |  stores (for
   |                     |         |  receives heartbeats|         |  Replicate fan-out)
   +---------------------+         |  from stores)       |         +---------------------+
                                   +---------------------+
```

### 7.1 `dtmpfs-client`

**Responsibility.** Translate FUSE callbacks into RPCs. Cache attributes and blocks. Buffer writes per file handle. Flush on `close`/`fsync`.

**State owned.** Tokio runtime handle. AttrCache, BlockCache, OpenFile table (per `fh`: `block_map`, `dirty_blocks: BTreeMap<BlockIdx, BytesMut>`, generation at open time, gRPC channels).

**Talks to.** `dtmpfs-meta` (every metadata op, every `open`, every `close`). `dtmpfs-store` (every block read on cache miss; every dirty block on flush).

**Does not.** Hold any authoritative state. After a client crash, all of its caches and dirty buffers are lost; the meta server's view is unaffected because the client never told it about the writes.

### 7.2 `dtmpfs-meta`

**Responsibility.** Authoritative metadata, namespace, file-handle allocation, block-placement decisions, store membership.

**State owned.** `MetaState` behind a `tokio::sync::RwLock`. Inode table (`HashMap<u64, Inode>`), directory entries (`HashMap<u64, HashMap<String, u64>>`), open-handle table, monotonic ino/fh allocators, node membership, the HRW ring seed and node list.

**Talks to.** Receives RPCs from clients. Receives heartbeats from stores. Does not initiate any RPCs in v1.

**Does not.** Hold block bytes, do block I/O, or proxy reads/writes. Does not authenticate users (only the cluster token).

### 7.3 `dtmpfs-store`

**Responsibility.** Hold block bytes in RAM and serve them. Replicate to peer stores when instructed.

**State owned.** `DashMap<BlockKey, Bytes>` of block contents. A small heartbeat task. Optionally, write-side stale-generation rejection (Phase 6).

**Talks to.** Sends heartbeats to meta. Receives RPCs from clients. Receives `Replicate` RPCs from peer stores.

**Does not.** Know anything about inodes, directories, paths, or the namespace. It is a content-addressed RAM bucket keyed by `BlockKey`. It does not consult meta to validate keys.

## 8. Deployment topology

### 8.1 Prototype (single VM, four processes)

For phase-1-through-5 development, all four processes run on one machine.

```
  +------------------------------------------------------------+
  |                       single VM                            |
  |                                                            |
  |  /mnt/dtmpfs  <- FUSE mount                                |
  |       ^                                                    |
  |       | /dev/fuse                                          |
  |       |                                                    |
  |  +----+---------+   :7100   +------------------+           |
  |  | dtmpfs-mount |---------->|  dtmpfs-meta     |           |
  |  | (client)     |           |                  |           |
  |  +----+---------+           +--+---------------+           |
  |       |                        ^                           |
  |       | :7200, :7201           |  heartbeats               |
  |       v                        |                           |
  |  +-------------+   +-------------+                         |
  |  | store-0     |   | store-1     |                         |
  |  | :7200       |   | :7201       |                         |
  |  +-------------+   +-------------+                         |
  |                                                            |
  +------------------------------------------------------------+
```

`R=1`. All traffic on loopback. Useful for correctness testing; not representative of LAN performance.

### 8.2 Small cluster (3+ VMs)

```
   host A (10.0.0.10)               host B (10.0.0.11)               host C (10.0.0.12)
   +-----------------------+        +-----------------------+        +-----------------------+
   | dtmpfs-meta  :7100    |        | dtmpfs-store :7200    |        | dtmpfs-store :7200    |
   | dtmpfs-mount /mnt/... |        | dtmpfs-mount /mnt/... |        | dtmpfs-mount /mnt/... |
   +-----------------------+        +-----------------------+        +-----------------------+
                  ^                            ^                                ^
                  |                            |                                |
                  +----------------------------+--------------------------------+
                                  trusted LAN, gRPC over HTTP/2
```

Each host can run any combination of {meta, store, client}; the only hard rule is that exactly one meta process is running cluster-wide. Mounts can be on hosts that run no other dtmpfs role.

### 8.3 Process placement guidelines

- **Meta.** Place on the host with the most stable network connectivity. The meta is a SPOF in v1; if that host's NIC flaps, the cluster is dead. Co-locating with a store is fine (the meta's CPU footprint is tiny); co-locating with a noisy ML training job is not.
- **Store.** One per host, sized to a fraction of the host's free RAM (`store.capacity_mb` in `store.toml`). Running multiple store processes on the same host is supported but offers no isolation benefit — a single store with the combined capacity is simpler.
- **Client.** Co-locate with the consumer of the mount. The FUSE bridge is a per-host concern; cross-host kernel sharing of FUSE mounts is not supported by Linux.
- **Replication factor sizing.** `R=1` is fine for ML scratch where data is regenerable. `R=2` for build caches and any workload where re-running a job is expensive. `R=3` is supported but rarely justified given the trusted-LAN model and ephemeral data.

### 8.4 What dtmpfs does *not* deploy

To set expectations:

- No service mesh, no sidecar, no init container coordination is required. Each role is a standalone binary plus a TOML.
- No external dependency at runtime — no etcd, no Consul, no ZooKeeper. The meta server *is* the coordination plane.
- No external load balancer. Clients address meta and stores by IP/port from their TOML.
- No persistent state on disk. Restarting any process reset that process's in-memory state. The cluster's collective state is fully reconstructible only by re-running the workload.

## 9. Key design decisions & rationale

### 9.1 FUSE vs in-kernel

**Chosen:** FUSE via `fuser` + `libfuse3`.

**Alternatives:** custom kernel module; eBPF-based filesystem; LD_PRELOAD POSIX shim; library API only (no mount).

**Why FUSE.**

- Iteration speed. We can iterate on the on-the-wire protocol and the metadata model without rebuilding kernel modules.
- Portability across Linux versions. `libfuse3` abstracts most kernel quirks.
- The cost of context-switching across the FUSE boundary is acceptable for 1 MiB block reads where transfer time dominates. For 64 KiB random I/O, FUSE overhead would matter; that's not our target.
- No CAP_SYS_ADMIN required at runtime; mount works as an unprivileged user with `user_allow_other`.

**Cost.** Per-syscall overhead vs in-kernel. No `O_DIRECT`. Mmap writeback is unsupported.

**Revisit.** If profiling shows FUSE upcalls dominating the latency budget on small-file workloads, evaluate `fuse-passthrough` or a custom kernel module. Not in the v1 budget.

### 9.2 Rust + tonic vs Go vs Python vs C

**Chosen:** Rust 1.94, `fuser` 0.14, `tokio`, `tonic` 0.12, `prost`, `bytes`, `dashmap`, `moka`.

**Alternatives.**

- **Go.** Excellent gRPC story; `bazil.org/fuse` and `hanwen/go-fuse` are mature. Rejected because the operator wants a single binary with predictable memory and no GC pauses on the data path; also Rust ergonomics on `Bytes`/zero-copy slice handling are stronger.
- **Python.** Out of scope for systems work; insufficient throughput at the data path.
- **C.** Direct `libfuse3` bindings are fine, but writing the gRPC server, the cache, and the lock-free maps in C with the same safety budget is a multiplier on effort. We pick Rust for memory safety in the hot loops.

**Cost.** Rust compile times; smaller community for FUSE specifically; `fuser` async story requires the bridge described in §11 (architecture.md).

**Revisit.** Not planned.

### 9.3 Close-to-open consistency

**Chosen:** close-to-open. Writes visible cluster-wide on `close()`/`fsync()`; in-flight writes are not visible to other clients.

**Alternatives.**

- **Strong / linearizable.** Per-byte-range or per-inode locks; every read consults meta or a coordinator. Rejected: latency cost on the data path is incompatible with our throughput goal. Locks across N hosts with no leases is a deep correctness problem.
- **Eventual.** Writes propagate "soonish". Rejected: tools like `make`, `cp`, and ML pipelines that finish file A before opening file B would race.

**Why close-to-open.** It is the contract NFSv3 has used for decades, well-understood by application authors. It maps cleanly onto generation-bump-on-close: cheap on the meta server (a single counter increment), cache-coherent on clients without explicit invalidation messages, and matches how applications actually use shared filesystems.

**Cost.** Two clients writing the same file simultaneously: last-close-wins per block; partial-block RMW on the loser is silently overwritten. We document this and do not paper over it.

**Revisit.** A write-lease scheme for the few use cases that need it is a possible Phase 8.

### 9.4 Rendezvous (HRW) hashing vs consistent ring vs static

**Chosen:** HRW (a.k.a. Highest Random Weight) on `(ino, block_idx)`.

**Alternatives.**

- **Consistent hashing ring with virtual nodes.** Standard, slightly cheaper to query, but requires more state to manage churn well; classic ring placement is biased without vnodes.
- **Static modulo.** `node = block_idx % N`. Trivial but rebalances *everything* when N changes.

**Why HRW.** Stateless beyond the node list; each `(key, node)` gets a hash, you sort, you take the top R. Adding or removing a node only re-homes blocks that hashed to the changed node. No virtual-node bookkeeping. Implementable in 30 lines.

**Cost.** O(N) per placement query; fine for N ≤ 64. For larger clusters, ring + vnodes wins.

**Revisit.** When N exceeds ~32 nodes, or when placement query latency shows up in flame graphs.

### 9.5 Single meta in v1 vs Raft from day one

**Chosen:** single `dtmpfs-meta` process. Raft via `openraft` is Phase 7.

**Alternatives.** Raft from day one; primary-backup with manual failover; gossip-based metadata.

**Why single.** Raft is a credible 4–8 weeks of integration work and obscures every other interesting question (placement, caching, FUSE bridging) until it's done. Shipping a single-meta v1 lets the rest of the system stabilize against a known authoritative state. The meta service is small and memory-only — restart is fast.

**Cost.** The meta is a SPOF. A meta crash kills the FS. Documented in §6 and `failure-model.md`.

**Revisit.** Phase 7 explicitly. The data path does not need to change to add Raft to meta; only the durability of `MetaState` mutations changes.

### 9.6 1 MiB block vs 64 KiB

**Chosen:** 1 MiB blocks.

**Alternatives.** 64 KiB (matches some filesystems and some kernel readahead sizes); 4 KiB (page-aligned); per-file inline storage for small files.

**Why 1 MiB.**

- Targets the ML / build-cache workload, where files are typically large (tensors, archives) and sequential.
- Amortizes per-RPC overhead across more bytes; one `ReadBlock` per MiB at 800 MiB/s is 800 RPCs/s, comfortable for `tonic`.
- `BlockCache` entries are large enough to be worth caching; 4 KiB blocks would explode the keyspace.

**Cost.** Many tiny files (≤ 64 KiB) waste capacity per logical file because every distinct block_idx is a `Bytes` value, and read-modify-write of a small region of a 1 MiB block reads 1 MiB. If the workload is dominated by small files, the calculus inverts.

**Revisit.** Open question (§13). If the operator confirms a small-files workload, switch to 64 KiB by configuration, or add inline-up-to-N-bytes on the inode for files smaller than a block.

### 9.7 gRPC (`tonic`) vs hand-rolled bincode-over-TCP

**Chosen:** gRPC via `tonic` over HTTP/2.

**Alternatives.** A handwritten framing protocol with `bincode` or `postcard`; QUIC; raw TCP with length prefixes.

**Why gRPC.**

- HTTP/2 multiplexing: many concurrent RPCs share one TCP connection, which matters when `buffer_unordered(16)` fans out reads.
- Generated client/server stubs from `.proto` keep the wire shape and the in-memory type aligned.
- Ecosystem tooling: `grpcurl` for ad-hoc probing, deadline propagation, status codes.
- `bytes::Bytes` integrates cleanly: `prost` produces zero-copy `Bytes` for `bytes` proto fields, so block payloads avoid an extra copy.

**Cost.** HTTP/2 framing overhead per RPC is ~30 bytes plus headers. Negligible at 1 MiB blocks; would matter at 4 KiB blocks. `tonic` adds compile time.

**Revisit.** If the protocol layer shows up in profiles at 1 MiB blocks, evaluate raw HTTP/2 with `hyper` or a custom protocol. Not expected.

### 9.8 Config format: TOML with role tagging

**Chosen:** one `Config` enum tagged by `role`, deserialized from TOML by `serde`, one TOML file per process.

**Alternatives.** YAML; JSON; environment variables only; CLI flags only.

**Why TOML.**

- Comments survive (YAML and JSON do not in their canonical forms).
- One enum with `#[serde(tag = "role")]` covers all four roles in one type, sharing common fields (`cluster_token`, `node_id`) across variants.
- Operators read TOML readily; no indentation gotchas like YAML.

**Cost.** Slightly more parser surface than env vars. We accept this for clarity at small operator counts.

**Revisit.** If the deployment model becomes Kubernetes-native, the TOMLs may move to ConfigMaps; the file format itself stays.

### 9.9 `bytes::Bytes` for block payloads

**Chosen:** `bytes::Bytes` end to end for block payloads.

**Alternatives.** `Vec<u8>`; raw `&[u8]` slices with a pinned arena; `Arc<[u8]>`.

**Why `Bytes`.**

- Reference-counted, immutable, slice-cheap. The same payload can sit in `BlockCache`, satisfy multiple concurrent `read` calls without copying, and be passed back to `tonic` without re-allocation.
- `prost` integrates: `bytes` proto fields decode as `Bytes` views over the underlying gRPC frame. Zero-copy from wire to cache to slice.
- Matches the `tonic` codec path (which can also accept `Bytes` for outgoing payloads).

**Cost.** `BytesMut` is needed for the dirty-write path (`OpenFile.dirty_blocks`); converting `BytesMut -> Bytes` on flush is a single freeze, no copy. We accept the small ergonomic cost of distinguishing the two types.

**Revisit.** Not planned. This is a near-universal choice in modern Rust networking.

### 9.10 Single-RwLock meta state vs sharded

**Chosen:** one `tokio::sync::RwLock<MetaState>` in v1.

**Alternatives.** Per-inode locking (one `Mutex` per inode); inode-prefix sharding (M independent locks); lock-free with a transaction log.

**Why one lock.**

- Most v1 ops are short and CPU-light. The critical sections in `Lookup`, `GetAttr`, and `Open` are microseconds; a single read-write lock with mostly-readers is contention-free in practice for our 2–8 node target.
- Implementation simplicity: no lock-ordering hazards, no per-inode allocator for locks, no deadlock risk on cross-inode operations like `Rename`.

**Cost.** A pathological writer-heavy workload (millions of `Create` calls/sec) will serialize on this lock. At our target QPS (low thousands of metadata ops/sec), this is not a problem.

**Revisit.** If profiling shows write lock contention dominating tail latency, switch to inode-prefix sharding (`HashMap<u8 prefix, RwLock<MetaShard>>`). The data structures inside don't change.

## 10. High-level data flow

This section gives prose summaries. For step-by-step sequence diagrams, see [`architecture.md` §3](architecture.md).

### 10.1 `open(path, O_RDONLY)`

The FUSE kernel drives a `lookup` walk along the path components. Each `lookup` is a `Meta.Lookup { parent_ino, name }` that returns the child `Attr`. The kernel may shortcut subsequent lookups within `entry_timeout` (1 s).

The terminal `open` call is a `Meta.Open { ino, flags }` that returns `OpenResp { attr, fh, block_map }`. The client allocates an `OpenFile` keyed by `fh`, stores `attr.generation` and `block_map` on it, primes the AttrCache, and returns the handle to the kernel. No store traffic yet.

### 10.2 `read(fh, offset, size)`

The client computes the set of `(block_idx, intra_block_range)` pairs covering `[offset, offset + size)`. For each block index it consults `BlockCache.get((ino, generation, block_idx))`. On a hit, it slices the cached `Bytes`. On a miss, it looks up `block_map[block_idx]` to find the primary, and issues `Store.ReadBlock { key }`. With `R >= 2`, on `Status::unavailable` from the primary the client retries the next replica in `block_map[block_idx].replicas`. The block is inserted into the BlockCache and sliced to satisfy the read.

If the file is large enough that the read spans multiple blocks not yet cached, the client uses `buffer_unordered(16)` to issue them concurrently.

### 10.3 `write(fh, offset, data)`

The client locates or creates an entry in `OpenFile.dirty_blocks` for each affected block. For a partial overwrite of a not-yet-dirty block, it triggers a read-modify-write: fetch the block from the store at the file's open-time generation, copy into a `BytesMut`, apply the overwrite, and store under the dirty-blocks map. The write returns to FUSE immediately. **No RPC happens on `write`**.

### 10.4 `close(fh)` and `fsync(fh)`

Both routes land in the same flush logic. If `dirty_blocks` is empty, the client issues `Meta.Close { fh, dirty=false }` which is a no-op metadata-wise.

Otherwise:
1. The client calls `Meta.AllocateBlocks { ino, indices }` for any block indices that are new (i.e. extending the file). The response contains `BlockLoc` for each new index.
2. The client constructs a stream of `Store.WriteBlock` futures, one per dirty block per replica, and runs them with `buffer_unordered(16)`.
3. On success, the client issues `Meta.Close { fh, new_size, mtime, written_idxs }`. The meta service updates the inode's `size`, `mtime`, `blocks`, and bumps `generation`.

If any step fails, the dirty blocks are retained on the client and the FUSE call returns `EIO`. A subsequent `flush` or `fsync` retries.

### 10.5 `mkdir`, `unlink`, `rename`

Pure metadata operations. Each is a single RPC against the meta service. `unlink` of a regular file additionally removes the inode's blocks from the `block_map`; the actual block bytes on stores are deleted lazily by Phase 6 GC, immediately by Phase 5+ if `Meta.Unlink` returns the dropped `BlockLoc` list and the client fans out `Store.DeleteBlock`. The eager path is preferred because it bounds RAM usage.

### 10.6 `rename(old, new)`

`Meta.Rename { old_parent, old_name, new_parent, new_name }`. Atomic at the meta server under the meta lock. If `new` exists and is a regular file, it is unlinked first; if it is a non-empty directory, `ENOTEMPTY`. The rename does not change inode numbers, generations, or block placements; from a data-path perspective nothing happens. Clients that have the old path's inode cached observe no inconsistency: the inode itself is unchanged, only the directory entry moved.

### 10.7 `truncate(path, size)` and `ftruncate(fh, size)`

`Meta.SetAttr { ino, size: Some(new_size), .. }`. The meta server adjusts `Inode.size`, drops `Inode.blocks` entries with `block_idx >= ceil(new_size / block_size)`, and bumps `generation`. Truncation does not touch the store immediately; orphaned blocks are cleaned up by Phase 6 GC. Truncation that grows the file (sparse) creates no new blocks; reads of the gap return zeros until a write materializes them.

### 10.8 Cross-cutting: how a write becomes visible

The visibility chain for a single write — viewed end to end — is:

1. Application calls `write(fd, ...)`. FUSE upcall reaches `dtmpfs-mount`.
2. The client buffers into `OpenFile.dirty_blocks`. **No RPC.** The write returns to the application.
3. Application calls `close(fd)` (or `fsync(fd)`). FUSE upcall reaches `dtmpfs-mount`.
4. Client issues `Meta.AllocateBlocks` for any block_idx not already in `block_map`. Meta returns placements.
5. Client fans out `Store.WriteBlock` to primaries (and replicas if `R>=2`) with `buffer_unordered(16)`.
6. Once all primaries ack, the client calls `Meta.Close { fh, new_size, mtime, written_idxs }`.
7. Meta updates `Inode.{size, mtime, blocks}` under the meta write lock and bumps `Inode.generation`.
8. Meta's response unblocks the FUSE callback; `close()` returns to the application as success.

A subsequent `open()` from any client (including the same one) sees the new generation in step 4 of the open path (`Meta.Open`'s `OpenResp.attr`), and any reads it does are keyed at the new generation, so it cannot serve stale data from its `BlockCache`.

The write is **not** visible between steps 5 and 7: another client that calls `Meta.Open` in this window sees the *old* generation, reads from old block placements (which on the store are still the old gen), and gets pre-write data. That window is the close-to-open boundary.

## 10A. System invariants

These are properties the implementation must preserve. Violations are bugs; tests in `acceptance-tests.md` exercise them.

- **I1 (single source of truth for namespace).** `dtmpfs-meta` is the only authoritative source for inode metadata, directory entries, and `next_ino`. Clients and stores cache; they do not mutate.
- **I2 (generation monotonicity).** For any inode I, `generation` is monotonically non-decreasing. Meta only ever increments; it never resets even on inode unlink/recreate (a freshly recreated inode gets a fresh `ino`, not a recycled one — see F12).
- **I3 (block-key uniqueness within a generation).** For a given `(ino, block_idx, generation)` the store holds at most one `Bytes` value per replica. Concurrent `WriteBlock`s with the same key overwrite serially under the `DashMap` per-bucket lock; the last write wins on each individual store, but cross-store consistency is the client's responsibility (it writes the same payload to all replicas).
- **I4 (block-cache key disjointness).** A client's `BlockCache` entries keyed at `(ino, g, *)` are unreachable once the client has observed `Inode.generation > g` for that ino. They take RAM until LRU eviction but cannot be served. (This is what makes close-to-open invalidation free.)
- **I5 (open-handle scope).** A `fh` returned by `Meta.Open` is valid only at the meta that issued it, only until either side calls `Meta.Close` for it, and only at the generation noted at open time. Clients must not share `fh` values across processes.
- **I6 (no write without allocate).** Before issuing `Store.WriteBlock` for a block index that was not in the original `OpenResp.block_map`, the client must call `Meta.AllocateBlocks` for that index. Stores that receive a `WriteBlock` for an unknown placement simply accept it (Phase 6 stale-rejection extends this to verify the generation), but meta will not point future readers at the data unless it has issued the placement.
- **I7 (close ordering).** `Meta.Close` is called **after** all `Store.WriteBlock`s for the dirty set have succeeded. A failed `Store.WriteBlock` causes the client to skip `Meta.Close` and surface `EIO`; the meta's view of the inode remains at the prior generation, which is correct: the write did not happen.
- **I8 (heartbeat freshness).** A store is considered live by the meta only if its last `HeartbeatNode` arrived within `heartbeat_timeout_ms` (default 5000). Stale stores are removed from the HRW ring before any new placement decision is made.
- **I9 (cluster-token uniformity).** All four roles (meta, every store, every client) start with the identical `cluster_token` value. Mismatch surfaces as `Status::unauthenticated` immediately on the first RPC.

## 11. Phased roadmap

| Phase | Scope                                                                   | Effort      | Pass test                                              |
|-------|-------------------------------------------------------------------------|-------------|--------------------------------------------------------|
| P1    | Single-process FUSE shim, `Mutex<HashMap>` storage                      | ~1 weekend  | Local smoke test                                       |
| P2    | Split client / store; gRPC; data on one store                           | ~1 week     | Same smoke; bytes visible in store process             |
| P3    | Multi-store sharding via HRW; static node list                          | ~1 week     | Two stores, blocks split roughly evenly                |
| P4    | Metadata service, generation, AttrCache                                 | ~1.5 weeks  | Two clients see each other's writes                    |
| P5    | Replication R >= 2                                                      | ~1 week     | Kill one store with R=2, reads succeed                 |
| P6    | Heartbeats, stale-write rejection, GC, retries                          | ~1 week     | Soak test with random store kills                      |
| P7    | (stretch) Raft for meta via `openraft`                                  | weeks       | 3-meta cluster survives 1 meta kill                    |

**Why this ordering.**

- P1 forces the FUSE-callback shape and the in-memory metadata model into existence on a fast iteration loop. Catches threading and lifetime bugs early.
- P2 carves the network seam without changing what's stored. The wire types from `proto/` are pinned here; everything downstream depends on them.
- P3 introduces placement. Critical because once placement is wrong, fixing it forces data movement.
- P4 introduces the meta service and generations. After this phase, two clients see each other's writes, which is the whole point of dtmpfs.
- P5 reaches HA on the data path. The meta path is still SPOF but data survives one store death.
- P6 hardens against real-world flakiness: clock skew, dropped heartbeats, stuck stale clients.
- P7 is the only phase that requires a substantive architectural change on the meta side. Deliberately last.

Each phase is independently testable; no phase regresses earlier ones. The smoke test grows monotonically. Acceptance per phase is in [`acceptance-tests.md`](acceptance-tests.md).

## 11A. Observability stance (v1)

dtmpfs uses the `tracing` crate throughout for structured logs. The intent for v1:

- **Logs.** Each role emits `tracing` events at INFO for cluster lifecycle (mount, node-join, node-down), at DEBUG for every RPC handled, at ERROR for every `Status` returned with a non-`ok` code. `RUST_LOG=info` is the recommended default; `RUST_LOG=dtmpfs=debug,tonic=info` for protocol debugging.
- **No metrics endpoint in v1.** No Prometheus exporter, no `/metrics` route. The meta and store both expose `/debug/blocks` (store) and `/debug/state` (meta) over plain HTTP for ad-hoc inspection. Rich metrics are deferred until the operational shape is known.
- **No distributed tracing.** OpenTelemetry integration via `tonic` interceptors is straightforward to add later; we deliberately do not wire it for v1 to keep the dependency tree small.
- **Signals.** `SIGTERM`/`SIGINT` triggers a clean shutdown: client unmounts, store finishes in-flight RPCs and exits, meta drains its open handles and exits.

The intent is that the operator can reproduce any v1 problem from logs alone. Phase 6 hardening adds richer signals (heartbeat-miss counts, dropped writes), but the v1 doctrine is "log everything that mattered, nothing more".

## 12. Out of scope / non-goals (v1)

Restated from the plan, with reasoning:

- **No encryption, no TLS.** Trusted-LAN model. TLS in `tonic` is straightforward to add later; we deliberately defer to keep the v1 surface small.
- **No auth beyond the shared `cluster_token`.** Per-user authentication is a real feature, not a small one. Out of scope.
- **No snapshots, clones, quotas.** Each is a project on its own; none is necessary for the target use cases.
- **No xattrs, no hardlinks.** The semantics are subtle and the tools we care about don't need them.
- **No `mmap` writeback.** FUSE supports it but the kernel's mmap path with shared-state sharing across hosts is incompatible with close-to-open. Reads via mmap of small read-only files may incidentally work via the page cache; we do not test for this.
- **`fsync` is not a durability barrier.** RAM-only is a feature, not a bug. We document it.
- **No re-replication after a store dies.** With `R=1` the data is gone; with `R>=2` reads continue but the lost replica is not regenerated. Phase 8 stretch.
- **No background block GC for orphaned blocks.** Phase 6 implements eager delete on `unlink`; orphaned blocks (e.g. from a client that crashed mid-flush) are tolerated until cluster restart.
- **Linux only.** `fuser` works on macOS via macFUSE in theory; not tested, not supported.

## 13. Open questions

These are flagged from the plan and remain open until the operator weighs in. Each is scoped so we can pick a default and move; flagging them here so they don't get smuggled in by accident.

- **OQ-1: Block size 1 MiB vs 64 KiB.** 1 MiB is the choice for ML/build-cache workloads where files are large and sequential. Tradeoffs:
  - Larger blocks amortize per-RPC overhead and are friendly to `BlockCache` (fewer entries, larger payloads).
  - Smaller blocks waste less capacity on small files (a 1 KB file at 1 MiB blocks reserves a 1 MiB `Bytes` allocation per replica).
  - Smaller blocks make partial-overwrite RMW cheaper (read 64 KiB, modify, write 64 KiB vs 1 MiB).
  - If the workload is dominated by small files, 64 KiB blocks plus per-inode inline storage for files smaller than a block is a better fit. Decide before P3.
- **OQ-2: Default replication factor.** `R=1` saves RAM and is simple; `R=2` lets the failure-injection demo pass on day one and is closer to "useful" but doubles RAM cost. Proposed default: `R=1` in shipped configs, with `R=2` documented as the recommended setting for any non-trivial deployment.
- **OQ-3: `fsync` semantics.** Wait for primaries only (chosen) vs wait for all replicas. Tradeoffs:
  - Primaries-only: lower `fsync` latency, but a primary death between primary-ack and replica-ack loses the replica copy.
  - All-replicas: `fsync` becomes a synchronization point against all replica nodes, slowing the fast path.
  - Proposed: primaries-only by default; expose `client.fsync_wait_replicas: bool` for callers that need stronger guarantees.
- **OQ-4: Mount as user.** `MountOption::AllowOther` plus `user_allow_other` in `/etc/fuse.conf` lets non-mounting users see the FS. Proposed: yes. The use cases above require it: training jobs run as one user, a different user might inspect; the build cache is shared across CI runners under different uids. Confirm operator policy.
- **OQ-5: Use-case priority.** ML scratch vs build cache vs generic POSIX share. The defaults above target ML scratch (large blocks, eventual visibility, no auth). If the primary use case shifts:
  - **Build cache:** consider `R=2` default, smaller blocks, tighter `attr_cache_ttl` for fresher visibility.
  - **POSIX share:** consider stronger consistency story (some kind of lease) and proper auth before deploying to a less-trusted boundary.
- **OQ-6 (new): GC of orphaned blocks.** Crashed clients can leave dirty blocks on stores that no inode references. v1 tolerates this. Do we want a periodic sweep that asks meta "is `BlockKey { ino, gen, idx }` still referenced?" before P6 closes? Cost is low; surface area on the meta API grows. Proposed: defer to P6, where we add a `Meta.IsBlockReferenced` RPC and have stores opportunistically scrub via batched queries during idle.
- **OQ-7 (new): Per-fd flush concurrency cap.** `buffer_unordered(16)` is hardcoded. Should this be `client.flush_parallelism` in `client.toml`? Proposed: yes, with default 16. Trivial to expose.
- **OQ-8 (new): RPC deadline defaults.** 5 s for control, 30 s for data RPCs. Are these the right values for your network? Lower deadlines surface failures faster but risk false negatives on a busy LAN.
- **OQ-9 (new): Per-mount or per-cluster `block_size`?** Currently the client sets `block_size` and meta inherits it from each `Open` indirectly. If two clients on the same cluster disagree on block size, behaviour is undefined. Proposed: enforce that meta records the cluster's block size in its config and rejects `Open` from clients with a different value (`Status::failed_precondition`).

## 13A. How to read the rest of the docs

Suggested reading order, depending on your goal:

- **"Should I use this?"** — finish this HLD, then [`failure-model.md`](failure-model.md) (to understand what breaks), then [`consistency.md`](consistency.md) (to understand what won't be there).
- **"I want to deploy this on my cluster."** — [`README.md`](../README.md), [`configuration.md`](configuration.md), [`operations.md`](operations.md).
- **"I want to contribute code."** — this HLD, then [`architecture.md`](architecture.md) for the threading model and dataflow, then [`LLD.md`](LLD.md) for the structures, then [`protocol.md`](protocol.md) for the wire types. Tests in [`testing.md`](testing.md) and [`acceptance-tests.md`](acceptance-tests.md) for the validation bar.
- **"I want to understand the canonical race."** — go straight to [`consistency.md`](consistency.md); the example in [`architecture.md` §3.5](architecture.md) is the same story in sequence-diagram form.
- **"Something is broken."** — [`operations.md`](operations.md) troubleshooting, then [`failure-model.md`](failure-model.md) for what *should* happen in your scenario.

## 14. References

- [`README.md`](../README.md) — project front door, build, smoke test.
- [`architecture.md`](architecture.md) — diagrams and sequence walkthroughs.
- [`LLD.md`](LLD.md) — low-level design: structs, algorithms, locking discipline.
- [`protocol.md`](protocol.md) — full gRPC service definitions and field semantics.
- [`consistency.md`](consistency.md) — close-to-open semantics deep-dive, the canonical race.
- [`failure-model.md`](failure-model.md) — failure modes, recovery behavior.
- [`operations.md`](operations.md) — deployment, monitoring, troubleshooting.
- [`configuration.md`](configuration.md) — config reference per role.
- [`testing.md`](testing.md) — test strategy.
- [`acceptance-tests.md`](acceptance-tests.md) — concrete pass/fail tests per phase.
