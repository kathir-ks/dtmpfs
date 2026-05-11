# dtmpfs Wire Protocol Specification (v1)

This document is the normative reference for every byte that crosses a network
boundary in dtmpfs v1. It defines the transport, the gRPC service surface, the
proto3 message shapes, error mapping, idempotency, and connection lifecycle.

For higher-level context see:

- `docs/HLD.md` — top-down architecture
- `docs/LLD.md` — module-level design
- `docs/architecture.md` — diagrams and component relationships
- `docs/consistency.md` — close-to-open consistency model and generation semantics
- `docs/failure-model.md` — fault categories and how RPCs surface them
- `docs/operations.md` — running clusters
- `docs/configuration.md` — config keys
- `docs/testing.md`, `docs/acceptance-tests.md` — test plan

## 1. Transport

### 1.1 gRPC over HTTP/2

dtmpfs uses gRPC as its sole inter-process transport. The gRPC implementation is
[`tonic`](https://docs.rs/tonic) 0.12 on top of `hyper` and `tokio`. HTTP/2 is
mandatory; HTTP/1.1 fallback is not supported.

In v1 every connection is **plaintext (h2c)**: no TLS. dtmpfs assumes the LAN it
runs on is trusted (operator-controlled, firewalled). TLS is deliberately
deferred to a later release; see Section 9 for the migration path.

Each role exposes a single TCP listener:

| Role  | Default port | What it speaks                       |
|-------|--------------|--------------------------------------|
| meta  | 7100         | `dtmpfs.meta.v1.Meta`                |
| store | 7200, 7201…  | `dtmpfs.store.v1.Store`              |
| client| (none)       | client is gRPC client only, no server|

All ports are operator-configurable via `meta.toml` / `store.toml`.

### 1.2 Authentication

There is no per-user authentication in v1. Instead every RPC carries a static
shared secret in gRPC metadata:

```
cluster-token: <opaque-string-from-config>
```

The token is loaded from the `cluster_token` key in each role's TOML config and
injected into outgoing requests by an interceptor. Servers install a matching
interceptor that:

1. Reads the `cluster-token` metadata header.
2. Compares it byte-for-byte against the configured token (constant-time
   comparison, `subtle::ConstantTimeEq`).
3. Rejects mismatched or missing tokens with `Status::unauthenticated`.

Note: the token lives in **metadata**, not in any request body. Putting it in
metadata means we can keep the protobuf surface stripped of auth concerns and
we can swap the scheme (e.g., per-tenant JWTs) without touching the messages.

`Status::unauthenticated` is reserved for token failures. User-level permission
denial (e.g., `chmod` to write a read-only file) maps to `Status::permission_denied`.
See Section 4.

### 1.3 Encoding

- proto3 syntax everywhere.
- `prost` for code generation, `tonic-build` driving compilation in
  `crates/dtmpfs-proto/build.rs`.
- All field numbers are stable from v1.0.0 onward; renumbering is a breaking
  change and requires `v2`.

### 1.4 Versioning

Each service lives under a versioned proto package:

- `dtmpfs.meta.v1.Meta`
- `dtmpfs.store.v1.Store`

The `v1` segment is part of the gRPC method path on the wire
(`/dtmpfs.meta.v1.Meta/Lookup`). A breaking change — removing a field, changing
a field's type, repurposing a field number, or changing observable semantics —
bumps the package to `v2`. The two versions can be served side-by-side from the
same process during migration; the rust trait impls live in separate modules
so the legacy code path stays compilable.

Adding a new RPC method or a new optional field is **non-breaking** in proto3
and does **not** bump the version.

### 1.5 Timeouts

Clients set a per-RPC deadline via the gRPC `grpc-timeout` header. Default
deadlines for v1:

| RPC family            | Default deadline |
|-----------------------|------------------|
| Metadata RPCs (meta)  | 5 s              |
| `Store.ReadBlock`     | 5 s              |
| `Store.WriteBlock`    | 30 s             |
| `Store.DeleteBlock`   | 5 s              |
| `Store.Replicate`     | 30 s             |
| `Heartbeat*`          | 2 s              |
| `Stat`, `ListNodes`   | 2 s              |

The longer 30 s `WriteBlock` deadline accounts for a full 1 MiB payload over a
saturated link plus per-replica fan-out. The server enforces the same deadline
via `tonic`'s `Request::deadline()`; if the deadline fires server-side mid-RPC,
the handler aborts (futures cancel) and returns `Status::deadline_exceeded`.

### 1.6 Retries

v1 has **no transport-level retries**. Every error bubbles up to the caller
unchanged. Retry policy, when added, will be implemented at the call site —
e.g., the FUSE flush path may decide to retry a single `WriteBlock` once after
a `Status::unavailable`. The transport itself does not retry, because we want
the call site to make the decision based on idempotency (Section 3) and on
whether the operation has user-visible side effects.

Operators who need resilience today should use `R >= 2` so reads can fail over
to a replica without needing transport retry.

### 1.7 Compression

No compression in v1. Block payloads are typically already-compressed user data
(images, model weights, archives) and per-RPC compression cost is rarely worth
the CPU. `gzip`/`zstd` can be added later via `tonic`'s
`Request::set_accept_compression_encodings()` without a version bump.

## 2. Service definitions

Both `.proto` files are reproduced here in full. The canonical source lives in
`proto/meta.proto` and `proto/store.proto` and is compiled by
`crates/dtmpfs-proto/build.rs` via `tonic_build::compile_protos`.

### 2.1 `proto/meta.proto`

```proto
// proto/meta.proto
//
// Service: dtmpfs.meta.v1.Meta
// Speaker: dtmpfs-client       Listener: dtmpfs-meta
//
// All RPCs require the `cluster-token` metadata header.
// All timestamps are split into seconds (`*_s`, signed int64 since the
// UNIX epoch UTC) and nanoseconds within that second (`*_ns`, uint32
// in the range [0, 1_000_000_000)). This avoids the protobuf
// well-known-types dependency for a tiny perf win and keeps the wire
// representation stable.

syntax = "proto3";

package dtmpfs.meta.v1;

// ---------- Common ----------

message Empty {}

// File or directory attributes. Mirrors the relevant subset of
// `struct stat`. Returned by Lookup, GetAttr, SetAttr, Open, Mkdir, Create.
message Attr {
  // Inode number. Stable for the lifetime of the inode. A deleted-and-
  // recreated path gets a NEW inode (no recycling within the run).
  uint64 ino = 1;

  // Logical size in bytes. For directories: implementation-defined
  // (currently a constant block-size; not the number of entries).
  uint64 size = 2;

  // Number of 1-MiB blocks allocated. blocks * 1MiB >= size, generally.
  uint64 blocks = 3;

  // Generation counter. Bumped by Meta.Close iff the close flushed
  // dirty blocks. See docs/consistency.md section 4 for the full
  // mechanics. Clients use this to key their BlockCache.
  uint64 generation = 4;

  // POSIX mode bits: S_IFMT high bits (S_IFREG | S_IFDIR | S_IFLNK) OR'd
  // with permission low bits (e.g. 0o644). Layout matches glibc's stat.
  uint32 mode = 5;

  // Hard link count. v1: always 1 for files, 2+ for directories
  // (self + child dirs that contain `..`). Hardlinks are not supported.
  uint32 nlink = 6;

  uint32 uid = 7;
  uint32 gid = 8;

  // Last access time. v1 mounts with NoAtime so this is rarely updated.
  int64 atime_s = 9;        // seconds since UNIX epoch UTC
  uint32 atime_ns = 10;     // 0..999_999_999

  // Last modification time. Updated by Meta.Close-with-dirty,
  // Meta.SetAttr, Meta.Create, Meta.Mkdir.
  int64 mtime_s = 11;
  uint32 mtime_ns = 12;

  // Last status (metadata) change. Updated for any inode mutation
  // including chmod/chown/rename. Always >= mtime.
  int64 ctime_s = 13;
  uint32 ctime_ns = 14;
}

// Where one block of a file lives in the cluster.
message BlockLoc {
  uint64 block_idx = 1;            // 0-based block index within the file
  string primary = 2;              // node_id of primary store
  repeated string replicas = 3;    // additional replicas; len == R - 1
}

// One entry within a directory listing.
message DirEntry {
  string name = 1;                 // single path segment, no slashes
  uint64 ino = 2;                  // child inode
  uint32 kind = 3;                 // POSIX d_type: DT_REG=8, DT_DIR=4, DT_LNK=10
}

// A storage node as seen by the meta.
message NodeInfo {
  string node_id = 1;              // operator-assigned, e.g. "store-0"
  string addr = 2;                 // "http://10.0.0.20:7200"
  // Liveness as observed by the meta. UP / DOWN derived from heartbeats.
  enum Status {
    UNKNOWN = 0;
    UP = 1;
    DOWN = 2;
    DRAINING = 3;                  // cordoned, no new placements
  }
  Status status = 3;
  uint64 used_bytes = 4;           // most recently reported by store
  uint64 capacity_bytes = 5;       // configured RAM budget
  int64 last_heartbeat_s = 6;      // monotonic seconds since meta start
}

message NodeList {
  repeated NodeInfo nodes = 1;
}

// ---------- Path resolution ----------

message LookupReq {
  uint64 parent_ino = 1;           // directory to look in
  string name = 2;                 // entry name (no slash)
}

message LookupResp {
  Attr attr = 1;                   // attributes of the resolved child
  // generation (inside attr) is what the client will key BlockCache on.
}

message GetAttrReq {
  uint64 ino = 1;
}

// SetAttr supports truncate, chmod, chown, utimens. Each oneof field
// is set iff the client wants that attribute changed. Unset fields
// are preserved on the inode.
message SetAttrReq {
  uint64 ino = 1;

  // Optional new size. If set and shrinks the file, the meta deletes
  // (asynchronously) any blocks past the new last block. If grows, a
  // hole is recorded; reads of the hole return zero bytes without
  // contacting any store.
  optional uint64 size = 2;

  optional uint32 mode = 3;        // new POSIX mode bits
  optional uint32 uid = 4;
  optional uint32 gid = 5;

  // utimens: if both _s and _ns are set, the meta uses them; if neither,
  // the field is left alone.
  optional int64 atime_s = 6;
  optional uint32 atime_ns = 7;
  optional int64 mtime_s = 8;
  optional uint32 mtime_ns = 9;
}

message SetAttrResp {
  Attr attr = 1;                   // post-mutation attributes
}

// ---------- Namespace mutation ----------

message CreateReq {
  uint64 parent_ino = 1;
  string name = 2;
  uint32 mode = 3;                 // permission bits; S_IFREG implied
  uint32 uid = 4;
  uint32 gid = 5;
}

message CreateResp {
  Attr attr = 1;
  uint64 fh = 2;                   // an open handle is also returned
                                   // because POSIX `creat()` opens for writing
  repeated BlockLoc block_map = 3; // empty for new files (size=0, no blocks)
}

message MkdirReq {
  uint64 parent_ino = 1;
  string name = 2;
  uint32 mode = 3;                 // permission bits; S_IFDIR implied
  uint32 uid = 4;
  uint32 gid = 5;
}

message UnlinkReq {
  uint64 parent_ino = 1;
  string name = 2;
}

message RmdirReq {
  uint64 parent_ino = 1;
  string name = 2;
}

// Meta-side rename. Both src and dst directories live in the same
// MetaState, so this is atomic under the meta RwLock.
message RenameReq {
  uint64 src_parent_ino = 1;
  string src_name = 2;
  uint64 dst_parent_ino = 3;
  string dst_name = 4;
}

// ---------- Directory enumeration ----------

message ReadDirReq {
  uint64 ino = 1;                  // directory inode
  // Opaque cookie returned by a previous ReadDirResp. Empty cookie
  // means start from the beginning. The meta currently encodes the
  // cookie as the last-returned name (sorted lexicographically).
  bytes cookie = 2;
  // Soft cap on response entries. The server may return fewer.
  uint32 max_entries = 3;
}

message ReadDirResp {
  repeated DirEntry entries = 1;
  bytes next_cookie = 2;           // empty when EOF
  bool eof = 3;                    // true on the response that finishes the dir
}

// ---------- Open / close ----------

message OpenReq {
  uint64 ino = 1;
  // POSIX open flags subset (bits we care about):
  // O_RDONLY = 0, O_WRONLY = 1, O_RDWR = 2, O_APPEND = 0o2000, O_TRUNC = 0o1000.
  uint32 flags = 2;
}

message OpenResp {
  Attr attr = 1;                   // includes generation
  uint64 fh = 2;                   // server-allocated handle id
  repeated BlockLoc block_map = 3; // placement at this generation
}

message CloseReq {
  uint64 fh = 1;
  uint64 ino = 2;
  // Generation the client opened with. Meta uses this to detect that
  // some other client has already published a newer generation, in
  // which case this Close is rejected (FAILED_PRECONDITION). See
  // docs/consistency.md section 5.5.
  uint64 expected_generation = 3;
  // New file size, in bytes, after the writes the client just flushed.
  uint64 new_size = 4;
  // Block indices the client wrote during this open. Empty list means
  // no dirty blocks => Close MUST NOT bump generation.
  repeated uint64 written_block_idxs = 5;
  // Wall-clock the client wants recorded as mtime. Server clamps to
  // its own clock if skew > 60 s.
  int64 mtime_s = 6;
  uint32 mtime_ns = 7;
}

message CloseResp {
  Attr attr = 1;                   // post-close, post-bump attributes
}

// ---------- Block placement ----------

message AllocReq {
  uint64 ino = 1;
  // Block indices the client wants placement for. The meta returns
  // BlockLocs for each. Idempotent: requesting an already-placed
  // block returns its existing placement.
  repeated uint64 block_idxs = 2;
}

message AllocResp {
  repeated BlockLoc block_map = 1; // one BlockLoc per requested idx, same order
}

// ---------- Cluster membership ----------

message HeartbeatReq {
  string node_id = 1;
  string addr = 2;                 // store's gRPC base URL
  uint64 used_bytes = 3;
  uint64 capacity_bytes = 4;
  // Liveness/identity epoch. Lets meta detect a store restart
  // (epoch changes) vs. a steady-state heartbeat.
  uint64 epoch_s = 5;              // store start time, seconds since UNIX epoch
}

message HeartbeatResp {
  // What the meta currently believes about the cluster, so a freshly
  // booted store can know its peers.
  NodeList cluster = 1;
}

// ---------- Service ----------

service Meta {
  // Path resolution.
  rpc Lookup(LookupReq)    returns (LookupResp);
  rpc GetAttr(GetAttrReq)  returns (Attr);
  rpc SetAttr(SetAttrReq)  returns (SetAttrResp);

  // Namespace mutation.
  rpc Create(CreateReq)    returns (CreateResp);
  rpc Mkdir(MkdirReq)      returns (Attr);
  rpc Unlink(UnlinkReq)    returns (Empty);
  rpc Rmdir(RmdirReq)      returns (Empty);
  rpc Rename(RenameReq)    returns (Empty);
  rpc ReadDir(ReadDirReq)  returns (ReadDirResp);

  // File handles + close-to-open hooks.
  rpc Open(OpenReq)        returns (OpenResp);
  rpc Close(CloseReq)      returns (CloseResp);
  rpc AllocateBlocks(AllocReq) returns (AllocResp);

  // Cluster membership.
  rpc HeartbeatNode(HeartbeatReq) returns (HeartbeatResp);
  rpc ListNodes(Empty)            returns (NodeList);
}
```

### 2.2 `proto/store.proto`

```proto
// proto/store.proto
//
// Service: dtmpfs.store.v1.Store
// Speakers: dtmpfs-client (read/write), dtmpfs-store (replicate, peer pulls)
// Listener: dtmpfs-store

syntax = "proto3";

package dtmpfs.store.v1;

message Empty {}

// Identity of one block on one storage node. Three-tuple because we
// keep a single block index across multiple generations alive briefly
// (until GC) so that a stale writer's WriteBlock can be rejected
// without colliding with the live data.
message BlockKey {
  uint64 ino = 1;
  uint64 block_idx = 2;
  // Generation the *writer* opened with. The store treats this as
  // the version it is recording. See WriteBlockReq below for the
  // freshness check.
  uint64 generation = 3;
}

message ReadBlockReq {
  BlockKey key = 1;
  // Optional sub-range. If both are zero, returns the full block.
  // Otherwise returns bytes [offset, offset+len). Zero-padding past
  // EOB is the caller's job (it has the file size from Meta.GetAttr).
  uint32 offset = 2;
  uint32 len = 3;                  // 0 => "to end of block"
}

message ReadBlockResp {
  bytes data = 1;                  // payload, exactly `len` bytes
  uint32 len = 2;                  // mirror of bytes-actually-returned
}

// WriteBlockReq.data MUST be <= the cluster's configured block_size
// (default 1 MiB). Sending a larger payload returns
// Status::invalid_argument. Sending a smaller payload is fine — the
// store records the smaller length and zero-pads on read. (This is
// how tail-block partial writes are stored.)
message WriteBlockReq {
  BlockKey key = 1;
  bytes data = 2;
}

message WriteBlockResp {
  // Bytes written. Equals data.len() on success.
  uint32 len = 1;
}

message DeleteBlockReq {
  BlockKey key = 1;
}

// Asks the recipient store to PULL a block from `source`. Used during
// replication and during after-the-fact re-replication. The recipient
// dials `source` via its standard Store.ReadBlock RPC.
message ReplicateReq {
  string source_node_id = 1;       // node_id from the meta's NodeList
  string source_addr = 2;          // "http://10.0.0.20:7200"
  BlockKey key = 3;
}

message StoreStat {
  string node_id = 1;
  uint64 used_bytes = 2;
  uint64 capacity_bytes = 3;
  uint64 block_count = 4;
  uint64 read_bytes_total = 5;     // since start
  uint64 write_bytes_total = 6;
}

service Store {
  rpc ReadBlock(ReadBlockReq)    returns (ReadBlockResp);
  rpc WriteBlock(WriteBlockReq)  returns (WriteBlockResp);
  rpc DeleteBlock(DeleteBlockReq) returns (Empty);
  rpc Replicate(ReplicateReq)    returns (Empty);
  rpc Stat(Empty)                returns (StoreStat);
}
```

#### 2.2.1 `WriteBlockReq.data` size rationale

`bytes data = 2;` — the wire field is a single contiguous byte string. The
maximum payload is the cluster's `block_size` (1 MiB by default). We chose
1 MiB because:

- HTTP/2 frame size in tonic defaults to 16 KiB, so a 1 MiB payload becomes 64
  frames. That's still well under the 64-frame-per-message practical sweet
  spot for tonic and avoids the head-of-line costs of mega-payloads.
- 1 MiB is large enough to amortize per-RPC overhead for sequential workloads
  (a 1 GiB file is 1024 RPCs, not 16384).
- 1 MiB is small enough that a partial-block read-modify-write doesn't move an
  unreasonable amount of data. A 4 KiB write at the head of a 1 MiB block
  reads ~1 MiB to RMW; that's still acceptable for a RAM-only store.

If profiling reveals many-tiny-files dominance, the operator can drop
`block_size` to 64 KiB (still well above the HTTP/2 frame size). Larger than
4 MiB is discouraged: the gRPC default `max_decoding_message_size` is 4 MiB
and bumping it to e.g. 16 MiB on both ends makes the framing pathological.

## 3. RPC reference table

| Method                  | Caller        | Callee | Idempotent | Side effects                                              | Typical latency (1 VM) |
|-------------------------|---------------|--------|------------|-----------------------------------------------------------|------------------------|
| `Meta.Lookup`           | client        | meta   | yes        | none                                                      | < 200 µs               |
| `Meta.GetAttr`          | client        | meta   | yes        | none                                                      | < 200 µs               |
| `Meta.SetAttr`          | client        | meta   | conditional| inode mutation; may schedule async DeleteBlock on shrink  | < 500 µs               |
| `Meta.Create`           | client        | meta   | **no**     | new inode + dirent + open handle                          | < 1 ms                 |
| `Meta.Mkdir`            | client        | meta   | **no**     | new inode + dirent                                        | < 1 ms                 |
| `Meta.Unlink`           | client        | meta   | yes\*      | dirent removal; if last link, inode + async DeleteBlock   | < 1 ms                 |
| `Meta.Rmdir`            | client        | meta   | yes\*      | dirent + inode removal                                    | < 1 ms                 |
| `Meta.Rename`           | client        | meta   | yes        | atomic dirent move                                        | < 1 ms                 |
| `Meta.ReadDir`          | client        | meta   | yes        | none                                                      | < 500 µs               |
| `Meta.Open`             | client        | meta   | **no**     | open handle entry; refreshes attr + block_map snapshot    | < 1 ms                 |
| `Meta.Close`            | client        | meta   | **no**     | bumps generation + inode update; closes handle            | < 1 ms                 |
| `Meta.AllocateBlocks`   | client        | meta   | yes        | new BlockPlacement entries on inode                       | < 1 ms                 |
| `Meta.HeartbeatNode`    | store         | meta   | yes        | NodeInfo update                                           | < 500 µs               |
| `Meta.ListNodes`        | client/store  | meta   | yes        | none                                                      | < 500 µs               |
| `Store.ReadBlock`       | client/store  | store  | yes        | none                                                      | 50 µs–5 ms (size dep.) |
| `Store.WriteBlock`      | client/store  | store  | conditional| inserts/replaces DashMap entry                            | 50 µs–5 ms             |
| `Store.DeleteBlock`     | client/meta   | store  | yes\*      | DashMap entry removal                                     | < 200 µs               |
| `Store.Replicate`       | meta/store    | store  | yes        | recipient pulls + WriteBlock locally                      | 100 µs–5 ms            |
| `Store.Stat`            | client/meta   | store  | yes        | none                                                      | < 200 µs               |

`*` "yes\*" means: idempotent if you treat the second call's "not found" as
success. v1 callers should adopt that convention.

### 3.1 Idempotency notes (normative)

- `Lookup`, `GetAttr`, `ReadDir`, `ReadBlock`, `Stat`, `ListNodes`, `Heartbeat*`
  are pure reads (or near-pure: heartbeat updates a timestamp). Safely
  retryable any number of times.
- `Create`, `Mkdir` are **not** idempotent: a second call after success returns
  `Status::already_exists`. Callers wanting at-most-once semantics should use
  the `O_CREAT | O_EXCL` discipline at the FUSE layer.
- `Unlink`, `Rmdir`, `DeleteBlock` are idempotent **if** the caller treats
  "second call returns NOT_FOUND" as success. v1's recommended client policy is
  exactly that: the caller swallows `Status::not_found` from these RPCs after a
  retry. The first call's NOT_FOUND, however, propagates to the user as
  `ENOENT` — clients distinguish "this is a retry" by tracking whether they
  saw a successful response previously.
- `Rename` is idempotent on the (src, dst) pair: rename succeeds, second call
  with same args returns NOT_FOUND on src (because src is now empty); treat
  same way as Unlink.
- `WriteBlock` is idempotent for the **same `(BlockKey, data)`** pair. If a
  client retries with different data and the same key, the result is
  last-writer-wins **at the same generation**, which is a programming error.
  The generation in `BlockKey` ensures stale writes (writes from a sender that
  has not yet noticed a newer generation exists) are rejected with
  `Status::failed_precondition`.
- `Open`, `Close`, `AllocateBlocks` are state-mutating bookkeeping operations.
  None of them is safely retryable without an outer recovery protocol. In
  particular:
  - Retrying `Open` on a transient error allocates a *new* handle each time;
    the client must remember the latest `fh` and call `Close` on whichever it
    accepted.
  - Retrying `Close` after an unknown outcome is dangerous: if the first call
    succeeded, the second one finds the handle gone and returns NOT_FOUND. The
    correct recovery is to re-`Open` the file and treat the original close as
    lost work; v1 does not attempt to reconstruct mid-flush state.
  - Retrying `AllocateBlocks` is fine *if* the client uses the same inode and
    the same block indices. The meta dedupes against existing placements.

## 4. Error codes

Every domain error maps to a single `tonic::Status` variant. Constructors below
show the exact form servers use; clients pattern-match on `status.code()`.

| Domain error                                | gRPC code              | Constructor                                                       |
|---------------------------------------------|------------------------|-------------------------------------------------------------------|
| Path component missing (Lookup)             | `NOT_FOUND`            | `Status::not_found(format!("name {} not in dir {}", n, p))`       |
| Inode missing (GetAttr/SetAttr/Open)        | `NOT_FOUND`            | `Status::not_found(format!("ino {}", ino))`                       |
| Unlink/Rmdir target absent                  | `NOT_FOUND`            | `Status::not_found(format!("{}/{}", parent, name))`               |
| ReadBlock missing block                     | `NOT_FOUND`            | `Status::not_found(format!("{:?}", key))`                         |
| Create/Mkdir collision                      | `ALREADY_EXISTS`       | `Status::already_exists("name exists")`                           |
| Rmdir non-empty                             | `FAILED_PRECONDITION`  | `Status::failed_precondition("dir not empty")`                    |
| Rename onto non-empty dir                   | `FAILED_PRECONDITION`  | `Status::failed_precondition("rename target dir not empty")`      |
| WriteBlock with stale generation            | `FAILED_PRECONDITION`  | `Status::failed_precondition("stale generation")`                 |
| Close with stale `expected_generation`      | `FAILED_PRECONDITION`  | `Status::failed_precondition("close: expected gen N, observed M")`|
| Cluster token missing/wrong                 | `UNAUTHENTICATED`      | `Status::unauthenticated("invalid cluster token")`                |
| User-level POSIX permission deny            | `PERMISSION_DENIED`    | `Status::permission_denied("EACCES")`                             |
| Store RAM budget hit                        | `RESOURCE_EXHAUSTED`   | `Status::resource_exhausted("store full")`                        |
| Meta detected store DOWN; placement skipped | `UNAVAILABLE`          | `Status::unavailable("primary down")`                             |
| Server is restarting / draining             | `UNAVAILABLE`          | `Status::unavailable("draining")`                                 |
| Deadline ran out mid-RPC                    | `DEADLINE_EXCEEDED`    | (set by tonic from `grpc-timeout`)                                 |
| Unexpected internal bug                     | `INTERNAL`             | `Status::internal(format!("bug: {:?}", e))`                       |
| Caller passed a bad argument (e.g. oversize block) | `INVALID_ARGUMENT` | `Status::invalid_argument("data > block_size")`                   |

### 4.1 `unauthenticated` vs `permission_denied`

The two codes are distinct on purpose:

- **`UNAUTHENTICATED`** is reserved for transport-level identity failures.
  Today that is only "the cluster token is wrong or missing". A future JWT
  scheme would also report token-expired as UNAUTHENTICATED. The client
  should treat this as a fatal cluster misconfiguration and bubble it up, not
  retry.

- **`PERMISSION_DENIED`** is used for in-cluster authorization decisions tied
  to user/group/mode bits. v1 honours `MountOption::DefaultPermissions` so the
  kernel does most checks; the meta itself returns `PERMISSION_DENIED` only
  for chmod/chown attempts that the meta can detect violate POSIX (e.g. a
  non-root setuid attempt). Most user-level denials never reach the meta.

### 4.2 What `INTERNAL` means

`Status::internal` is the dtmpfs equivalent of "your bug, not the user's".
Anything that's a programming invariant violation — `unreachable!()`, a
poisoned RwLock, an unexpected `None` — surfaces here. The client maps
`INTERNAL` to `EIO` and logs at WARN. Frequent INTERNAL responses are a
production incident.

## 5. Wire-level details

### 5.1 HTTP/2 framing for 1 MiB blocks

A `WriteBlockReq.data` of 1 MiB is fragmented by tonic into the configured
HTTP/2 max frame size. The default is 16 KiB, so 1 MiB ≈ 64 DATA frames. This
is fine: tonic batches frames per message and the server-side reassembly is
zero-copy through `bytes::Bytes`. We do **not** raise the frame size in v1 —
the larger the frame, the worse the head-of-line latency for small RPCs that
share the connection.

We also do not use server-streaming for `ReadBlock` in v1: a single block fits
a single logical message and the unary path is simpler. Future improvement:
when block_size grows above 4 MiB, switch to `stream ReadBlockChunk` to start
returning data before the whole block is materialized server-side. Flagged for
v2 in Section 9.

### 5.2 `max_*_message_size`

Both meta and store servers set:

```rust
Server::builder()
    .max_decoding_message_size(8 * 1024 * 1024)   // 8 MiB ceiling
    .max_encoding_message_size(8 * 1024 * 1024)
```

The 8 MiB ceiling is 8x the default `block_size`, leaving headroom for
proto-overhead and operator-overridden block sizes up to 4 MiB.

### 5.3 gRPC reflection

`tonic_reflection` is wired in only when the binary is built with
`--features reflection` (default in debug, off in release). It's there so that
operators can run `grpcurl` against a meta or store to inspect schemas during
incident response. We omit it from release builds to keep the binary size and
attack surface down.

### 5.4 No content-type negotiation

The wire content-type is fixed to `application/grpc+proto`. We do not support
`grpc+json`. Tools that need JSON (curl + grpcurl) go through reflection.

## 6. Connection management

### 6.1 Client side

- The meta-side `Channel` is built once at mount time and reused for the life
  of the process. If we ever go HA we'll switch to
  `tonic::transport::Channel::balance_list` — for now the address is a single
  endpoint.
- Each store has its own `Channel`, lazy-created on first use and cached in a
  `dashmap::DashMap<NodeId, Channel>`. A store that comes back after being
  marked DOWN reuses the existing channel; tonic transparently reconnects.

### 6.2 Channel options

```rust
Channel::from_static(addr)
    .http2_keep_alive_interval(Duration::from_secs(30))
    .keep_alive_timeout(Duration::from_secs(10))
    .keep_alive_while_idle(true)
    .tcp_nodelay(true)
    .tcp_keepalive(Some(Duration::from_secs(60)))
    .connect_timeout(Duration::from_secs(2))
    .connect()
    .await?;
```

Rationale:

- `keep_alive_interval=30s`: catches a half-open TCP connection within ~30 s
  without flooding healthy connections with PING frames.
- `keep_alive_timeout=10s`: if a PING does not get a PONG within 10 s the
  channel is closed and the next RPC dials.
- `keep_alive_while_idle=true`: keepalive even when there's no in-flight RPC.
  We need this because between flushes a client may sit idle for minutes.
- `tcp_nodelay=true`: disables Nagle. dtmpfs is latency-sensitive at the small
  end (metadata RPCs are < 1 KiB) and Nagle's 40 ms penalty is unacceptable.
- `connect_timeout=2s`: store dial timeouts; longer than the LAN should ever
  need but short enough to surface a bad node quickly.

### 6.3 Server side

```rust
Server::builder()
    .http2_keepalive_interval(Some(Duration::from_secs(30)))
    .http2_keepalive_timeout(Some(Duration::from_secs(10)))
    .tcp_nodelay(true)
    .tcp_keepalive(Some(Duration::from_secs(60)))
    .concurrency_limit_per_connection(256)
    .timeout(Duration::from_secs(60))   // hard ceiling above per-RPC deadlines
    .add_service(MetaServer::new(svc))
    .serve(addr)
    .await
```

`concurrency_limit_per_connection=256`: prevents a single misbehaving client
from saturating the meta's executor with thousands of in-flight calls.

### 6.4 Heartbeat coupling

Stores call `Meta.HeartbeatNode` every `heartbeat_interval_ms` (default 1000).
The meta marks a store DOWN after `heartbeat_miss_threshold` consecutive misses
(default 5 → 5 s grace). DOWN nodes are removed from the placement ring on the
next AllocateBlocks call but their existing data is left in place — replicas
continue serving. See `docs/failure-model.md` for the full state machine.

## 7. Compatibility & evolution

dtmpfs commits to forward and backward proto compatibility within `v1.x.y`:

- **Adding fields**: always non-breaking in proto3. Older clients ignore the
  new field; older servers leave it at default. Always pick a fresh field
  number.
- **Removing fields**: never. Mark unwanted fields `reserved`:
  ```proto
  message Foo { reserved 7; reserved "old_name"; }
  ```
  This prevents a future engineer from accidentally re-using the field number.
- **Renaming RPCs**: never. The wire path includes the method name, so a
  rename is wire-breaking. Add a new RPC, deprecate the old one in comments,
  drop the old one only when bumping the package to `v2`.
- **Changing semantics**: requires a `v2` package. Old and new must be
  servable from the same binary during migration. The role's TOML config gains
  a `compat = "v1" | "v2" | "both"` knob.
- **Default-value changes**: changing what a default value *means* is a
  semantic change and triggers `v2`. Adding a default-having field where the
  default's meaning is "absent" is fine.

### 7.1 Field numbers reserved for future use

- `Attr`: 15-19 reserved for `nlink_max`, `xattr_count`, `flags`, future
  generation-related fields.
- `OpenResp`: 4 reserved for a forthcoming lease token.
- `Inode`-adjacent messages: leave a five-number gap before re-using.

## 8. Worked examples

These show the protobuf message shapes, written in the prost-style text format
(`{field: value}`), abbreviated where uninteresting. Times shown as the pair
`(_s, _ns)`.

### 8.1 Lookup of `/foo` in root

Request:

```
LookupReq { parent_ino: 1, name: "foo" }
```

Response on success (`/foo` is a 5 MiB regular file owned by uid=1000):

```
LookupResp {
  attr: Attr {
    ino: 17,
    size: 5242880,
    blocks: 5,
    generation: 7,
    mode: 0o100644,         // S_IFREG | 0644
    nlink: 1,
    uid: 1000, gid: 1000,
    atime_s: 1714329600, atime_ns: 0,
    mtime_s: 1714329600, mtime_ns: 0,
    ctime_s: 1714329600, ctime_ns: 0,
  }
}
```

If `/foo` does not exist:

```
Status::not_found("name foo not in dir 1")
```

### 8.2 Open of an existing 5 MiB file

Request (read-only, no flags of interest):

```
OpenReq { ino: 17, flags: 0 }
```

Response:

```
OpenResp {
  attr: Attr { ino: 17, size: 5242880, blocks: 5, generation: 7, ... },
  fh: 42,
  block_map: [
    BlockLoc { block_idx: 0, primary: "store-0", replicas: [] },
    BlockLoc { block_idx: 1, primary: "store-1", replicas: [] },
    BlockLoc { block_idx: 2, primary: "store-0", replicas: [] },
    BlockLoc { block_idx: 3, primary: "store-1", replicas: [] },
    BlockLoc { block_idx: 4, primary: "store-0", replicas: [] },
  ],
}
```

The client now caches `(ino=17, gen=7, block_idx=k) → bytes` keys for any
subsequent reads.

### 8.3 64 KiB write at offset 1.5 MiB into a 5 MiB file

Offset 1.5 MiB lies inside block 1 (block 1 covers bytes [1 MiB, 2 MiB)). The
write of 64 KiB lands fully inside block 1 (1.5 MiB + 64 KiB = 1.5625 MiB, well
within the block).

Client behaviour, in order:

1. Locate `OpenFile.dirty_blocks[1]`. If absent, fetch block 1 from its
   primary (`Store.ReadBlock(BlockKey{ino:17, block_idx:1, generation:7})`)
   into a new `BytesMut`. This is the read-modify-write step.
2. Splice the 64 KiB into the buffer at offset 0.5 MiB inside it.
3. Reply to FUSE immediately. **No RPC on this `write(2)`**.

Block 1 is already placed (file has 5 blocks); no `AllocateBlocks` needed for
this write. `AllocateBlocks` would only be needed if the user grew the file
past the existing 5-block end.

Later, on `flush`/`close`, the client emits:

```
Store.WriteBlock { key: {ino:17, block_idx:1, generation:7}, data: <1 MiB> }
```

(Note: the writer sends `generation: 7` — the gen it opened at — *not* gen 8.
The store records the write under gen 7 momentarily; the meta promotes it on
Close. See consistency.md section 4.)

### 8.4 Close that bumps generation 7 → 8

After flushing block 1, the client calls:

```
CloseReq {
  fh: 42,
  ino: 17,
  expected_generation: 7,
  new_size: 5242880,        // unchanged in this case
  written_block_idxs: [1],
  mtime_s: 1714329900, mtime_ns: 123_000_000,
}
```

Meta logic (under write lock on `MetaState`):

1. Lookup open handle `42`. If absent → `NOT_FOUND`.
2. Lookup inode `17`. If absent → `INTERNAL` (handle outlived inode = bug).
3. If `inode.generation != expected_generation` → `FAILED_PRECONDITION` and
   the handle is *not* dropped (client must redo the close after re-Open).
4. If `written_block_idxs` is empty → no generation bump; just close handle.
5. Otherwise: `inode.generation += 1`, install/refresh `BlockPlacement` entries
   for each written idx, set `inode.size = new_size`, set
   `inode.mtime = max(req_mtime, server_now - skew_tolerance)`, set
   `inode.ctime = now`.
6. Drop the open handle.
7. Return:

```
CloseResp {
  attr: Attr { ino: 17, size: 5242880, blocks: 5, generation: 8, ... },
}
```

### 8.5 ReadBlock with stale generation (rejected)

Suppose, after the close above, a long-stalled writer A finally tries to flush
its earlier write at the old generation:

```
Store.WriteBlock { key: {ino:17, block_idx:1, generation:7}, data: <1 MiB> }
```

The store's internal index already shows `block_idx:1` at `generation:8`
(installed during the successful flush on B's behalf). A write at gen 7 over a
gen 8 entry is rejected:

```
Status::failed_precondition("stale generation")
```

The store does NOT delete its gen-8 entry, does NOT touch anything; it just
refuses the write. A's client surfaces `EIO` to FUSE.

If A also tries `Meta.Close{expected_generation:7}`, the meta sees
`inode.generation == 8` and returns `FAILED_PRECONDITION`. A's client maps
that to `EIO` for FUSE and discards its dirty buffers; the user's writes are
lost (consistent with NFS-style close-to-open).

## 9. Future protocol additions

These are explicitly **not** part of v1. Listed so engineers can leave room in
field numbers and packages.

- **Server-streaming `ReadBlock`**: turn `rpc ReadBlock(...)` into
  `rpc ReadBlockStream(...) returns (stream ReadBlockChunk)` for blocks larger
  than a few MiB. Lower TTFB, smoother backpressure.
- **Bidirectional `WatchInode`**: `rpc Watch(stream WatchReq) returns (stream
  WatchEvent)` for inotify-equivalent. Lets clients subscribe to changes on a
  set of inodes; meta pushes events on Close/Unlink/Rename.
- **Range-based `ReadBlocks` / `WriteBlocks`**: amortize the per-RPC cost when
  flushing many adjacent dirty blocks.
- **TLS via `tonic`'s rustls**: `Server::builder().tls_config(...)`. Meta and
  stores get certs from a small in-house CA; clients verify cert + cluster
  token.
- **Per-tenant `cluster_token` via JWT**: signed bearer tokens with claims
  (tenant id, mount path prefix, expiration). Lets one cluster serve multiple
  tenants without rebuilding for each.
- **Raft control plane for meta HA** (`raft.proto`, package `dtmpfs.raft.v1`).
  Separate service. Meta nodes form a Raft group; clients open against the
  current leader and re-resolve on `UNAVAILABLE`.
- **Snapshots** of the meta state via `Meta.Snapshot/Restore` for backup.
- **`Stat`/`HealthCheck`** at the gRPC health protocol level
  (`grpc.health.v1.Health`) so external tools (envoy, k8s) can probe.
- **xattr RPCs**: `Meta.GetXattr/SetXattr/ListXattr/RemoveXattr`. Modeled on
  POSIX, deferred to Phase 3.
- **Symlink RPCs**: `Meta.Readlink/Symlink`. Phase 3.

## Appendix A. Interceptor: server-side token check

Sketch of the server-side token interceptor used by both meta and store:

```rust
fn token_interceptor(
    expected: Arc<String>,
) -> impl Fn(Request<()>) -> Result<Request<()>, Status> + Clone {
    move |req| {
        let got = req
            .metadata()
            .get("cluster-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if got.as_bytes().ct_eq(expected.as_bytes()).into() {
            Ok(req)
        } else {
            Err(Status::unauthenticated("invalid cluster token"))
        }
    }
}
```

`ct_eq` is `subtle::ConstantTimeEq` — important because string equality on
secrets is timing-leaky.

## Appendix B. Interceptor: client-side token injection

```rust
fn with_token<S: Service<Request<BoxBody>>>(
    inner: S, token: Arc<String>,
) -> impl Service<Request<BoxBody>, Response = S::Response, Error = S::Error> {
    tower::ServiceBuilder::new()
        .map_request(move |mut req: Request<BoxBody>| {
            let v = MetadataValue::from_str(&token).unwrap();
            req.metadata_mut().insert("cluster-token", v);
            req
        })
        .service(inner)
}
```

The wrapper is applied once at channel construction. Every RPC issued through
the channel automatically carries the token.

## Appendix C. End-to-end RPC ordering for a write workload

```
client                meta                       store-0          store-1

  open(/foo) ----->                                                          
                   [LOOKUP/OPEN tx]
              <----- attr@gen=7, block_map                                    
  write(off=0, 4KiB)   (no RPC; buffered)
  write(off=2MiB, 4KiB)(no RPC; buffered)
  flush()
                                              .---WriteBlock(idx=0, gen=7)-->|
                                              |---WriteBlock(idx=2, gen=7)----->
                                              `<--ack--                       
                                                                   <--ack-----
  Close(expected=7, ...) ->
                   [under meta write lock]
                   gen 7 -> 8
                   placements installed
              <----- attr@gen=8                                               
  release()
```

This is the v1 happy path. Failure-mode walks live in `docs/failure-model.md`.

## Appendix D. Quick reference — wire constants

| Constant                            | Default     | Where set            |
|-------------------------------------|-------------|----------------------|
| Meta listen port                    | 7100        | `meta.toml`          |
| Store listen port                   | 7200…       | `store.toml`         |
| Block size                          | 1 MiB       | client/store config  |
| Replication factor (R)              | 1           | client config        |
| `attr_cache_ttl_ms`                 | 1000        | client config        |
| `entry_timeout` / `attr_timeout`    | 1 s         | FUSE mount           |
| `block_cache_capacity_mb`           | 1024        | client config        |
| Heartbeat interval                  | 1 s         | store config         |
| Heartbeat miss threshold            | 5           | meta config          |
| HTTP/2 keepalive interval           | 30 s        | code (Section 6.2)   |
| HTTP/2 keepalive timeout            | 10 s        | code                 |
| `max_decoding_message_size`         | 8 MiB       | code (Section 5.2)   |
| Default RPC deadline                | 5 s         | code (Section 1.5)   |
| `WriteBlock` deadline               | 30 s        | code                 |

End of protocol specification.
