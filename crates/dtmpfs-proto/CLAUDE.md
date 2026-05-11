# dtmpfs-proto — Agent Brief

## Role

Compile the `proto/meta.proto` and `proto/store.proto` files into Rust types using `tonic_build`.
Re-export them so other crates can use `dtmpfs_proto::meta::*` and `dtmpfs_proto::store::*`.

## Crate boundaries

- You own: `crates/dtmpfs-proto/Cargo.toml`, `crates/dtmpfs-proto/build.rs`,
  `crates/dtmpfs-proto/src/lib.rs`, `proto/meta.proto`, `proto/store.proto`.
- Do NOT touch any other crate's files.
- Do NOT implement any RPC handlers — only the generated stubs.

## Files to create

1. `Cargo.toml` — see spec below
2. `build.rs` — invokes tonic_build
3. `src/lib.rs` — re-exports the two generated modules
4. `../../proto/meta.proto` — Meta service definition
5. `../../proto/store.proto` — Store service definition

---

## 1. `Cargo.toml`

```toml
[package]
name = "dtmpfs-proto"
version.workspace = true
edition.workspace = true

[dependencies]
tonic = { workspace = true }
prost = { workspace = true }
bytes = { workspace = true }

[build-dependencies]
tonic-build = { workspace = true }
```

---

## 2. `build.rs`

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .bytes(["."])
        .build_client(true)
        .build_server(true)
        .compile_protos(
            &["../../proto/meta.proto", "../../proto/store.proto"],
            &["../../proto"],
        )?;
    Ok(())
}
```

---

## 3. `src/lib.rs`

```rust
pub mod meta {
    tonic::include_proto!("dtmpfs.meta.v1");
}
pub mod store {
    tonic::include_proto!("dtmpfs.store.v1");
}
```

---

## 4. `../../proto/meta.proto`

```proto
syntax = "proto3";
package dtmpfs.meta.v1;

message Empty {}

message Attr {
  uint64 ino        = 1;
  uint64 size       = 2;
  uint64 blocks     = 3;
  uint64 generation = 4;
  uint32 mode       = 5;
  uint32 nlink      = 6;
  uint32 uid        = 7;
  uint32 gid        = 8;
  int64  atime_s    = 9;
  uint32 atime_ns   = 10;
  int64  mtime_s    = 11;
  uint32 mtime_ns   = 12;
  int64  ctime_s    = 13;
  uint32 ctime_ns   = 14;
}

message BlockLoc {
  uint64          block_idx = 1;
  string          primary   = 2;
  repeated string replicas  = 3;
}

message DirEntry {
  string name = 1;
  uint64 ino  = 2;
  uint32 kind = 3;
}

message NodeInfo {
  string node_id  = 1;
  string addr     = 2;
  enum Status {
    UNKNOWN  = 0;
    UP       = 1;
    DOWN     = 2;
    DRAINING = 3;
  }
  Status status          = 3;
  uint64 used_bytes      = 4;
  uint64 capacity_bytes  = 5;
  int64  last_heartbeat_s = 6;
}

message NodeList { repeated NodeInfo nodes = 1; }

message LookupReq  { uint64 parent_ino = 1; string name = 2; }
message LookupResp { Attr attr = 1; }
message GetAttrReq { uint64 ino = 1; }

message SetAttrReq {
  uint64           ino      = 1;
  optional uint64  size     = 2;
  optional uint32  mode     = 3;
  optional uint32  uid      = 4;
  optional uint32  gid      = 5;
  optional int64   atime_s  = 6;
  optional uint32  atime_ns = 7;
  optional int64   mtime_s  = 8;
  optional uint32  mtime_ns = 9;
}
message SetAttrResp { Attr attr = 1; }

message CreateReq {
  uint64 parent_ino = 1;
  string name       = 2;
  uint32 mode       = 3;
  uint32 uid        = 4;
  uint32 gid        = 5;
}
message CreateResp {
  Attr             attr      = 1;
  uint64           fh        = 2;
  repeated BlockLoc block_map = 3;
}

message MkdirReq {
  uint64 parent_ino = 1;
  string name       = 2;
  uint32 mode       = 3;
  uint32 uid        = 4;
  uint32 gid        = 5;
}

message UnlinkReq { uint64 parent_ino = 1; string name = 2; }
message RmdirReq  { uint64 parent_ino = 1; string name = 2; }

message RenameReq {
  uint64 src_parent_ino = 1;
  string src_name       = 2;
  uint64 dst_parent_ino = 3;
  string dst_name       = 4;
}

message ReadDirReq {
  uint64 ino         = 1;
  bytes  cookie      = 2;
  uint32 max_entries = 3;
}
message ReadDirResp {
  repeated DirEntry entries     = 1;
  bytes             next_cookie = 2;
  bool              eof         = 3;
}

message OpenReq  { uint64 ino = 1; uint32 flags = 2; }
message OpenResp {
  Attr             attr      = 1;
  uint64           fh        = 2;
  repeated BlockLoc block_map = 3;
}

message CloseReq {
  uint64          fh                  = 1;
  uint64          ino                 = 2;
  uint64          expected_generation = 3;
  uint64          new_size            = 4;
  repeated uint64 written_block_idxs  = 5;
  int64           mtime_s             = 6;
  uint32          mtime_ns            = 7;
}
message CloseResp { Attr attr = 1; }

message AllocReq {
  uint64          ino        = 1;
  repeated uint64 block_idxs = 2;
}
message AllocResp { repeated BlockLoc block_map = 1; }

message HeartbeatReq {
  string node_id        = 1;
  string addr           = 2;
  uint64 used_bytes     = 3;
  uint64 capacity_bytes = 4;
  uint64 epoch_s        = 5;
}
message HeartbeatResp { NodeList cluster = 1; }

service Meta {
  rpc Lookup        (LookupReq)      returns (LookupResp);
  rpc GetAttr       (GetAttrReq)     returns (Attr);
  rpc SetAttr       (SetAttrReq)     returns (SetAttrResp);
  rpc Create        (CreateReq)      returns (CreateResp);
  rpc Mkdir         (MkdirReq)       returns (Attr);
  rpc Unlink        (UnlinkReq)      returns (Empty);
  rpc Rmdir         (RmdirReq)       returns (Empty);
  rpc Rename        (RenameReq)      returns (Empty);
  rpc ReadDir       (ReadDirReq)     returns (ReadDirResp);
  rpc Open          (OpenReq)        returns (OpenResp);
  rpc Close         (CloseReq)       returns (CloseResp);
  rpc AllocateBlocks(AllocReq)       returns (AllocResp);
  rpc HeartbeatNode (HeartbeatReq)   returns (HeartbeatResp);
  rpc ListNodes     (Empty)          returns (NodeList);
}
```

---

## 5. `../../proto/store.proto`

```proto
syntax = "proto3";
package dtmpfs.store.v1;

message Empty {}

message BlockKey {
  uint64 ino        = 1;
  uint64 block_idx  = 2;
  uint64 generation = 3;
}

message ReadBlockReq  { BlockKey key = 1; uint32 offset = 2; uint32 len = 3; }
message ReadBlockResp { bytes data = 1; uint32 len = 2; }

message WriteBlockReq  { BlockKey key = 1; bytes data = 2; }
message WriteBlockResp { uint32 len = 1; }

message DeleteBlockReq { BlockKey key = 1; }

message ReplicateReq {
  string   source_node_id = 1;
  string   source_addr    = 2;
  BlockKey key            = 3;
}

message StoreStat {
  string node_id          = 1;
  uint64 used_bytes       = 2;
  uint64 capacity_bytes   = 3;
  uint64 block_count      = 4;
  uint64 read_bytes_total = 5;
  uint64 write_bytes_total = 6;
}

service Store {
  rpc ReadBlock   (ReadBlockReq)   returns (ReadBlockResp);
  rpc WriteBlock  (WriteBlockReq)  returns (WriteBlockResp);
  rpc DeleteBlock (DeleteBlockReq) returns (Empty);
  rpc Replicate   (ReplicateReq)   returns (Empty);
  rpc Stat        (Empty)          returns (StoreStat);
}
```

---

## Build command

```bash
# From workspace root
cargo build -p dtmpfs-proto
```

## Done when

`cargo build -p dtmpfs-proto` completes without errors. The generated types
`dtmpfs_proto::meta::Attr` and `dtmpfs_proto::store::BlockKey` must be accessible.
