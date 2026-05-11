# dtmpfs — Project Root

dtmpfs is a distributed RAM-backed POSIX filesystem (like tmpfs, but shared across a LAN cluster).
It uses FUSE on each host, a single metadata server, and sharded block stores — all communicating
via gRPC. The full design is in `docs/HLD.md` (architecture) and `docs/LLD.md` (implementation
details). This file is context for any Claude Code agent working on the codebase.

## Current status

**All five crates are implemented and working.** The system mounts, reads, writes, renames, and
deletes correctly. The following have been verified end-to-end:

- `cargo build --workspace` and `cargo test -p dtmpfs-common` pass cleanly.
- Single-host smoke: metasrv + storesrv + dtmpfs-mount, read/write/mkdir/rm/rename/truncate.
- Block GC: `unlink`, `rename`-onto-existing, and truncate-shrink all fire `DeleteBlock` RPCs
  to reclaim store RAM.
- Working config files live in `config/meta.toml`, `config/store.toml`, `config/client.toml`.

Phase 6 work (stale-write rejection, replica read-failover, meta debug HTTP, GC sweep) is not
yet implemented.

## Build commands

```bash
cargo build --workspace          # build all crates
cargo check --workspace          # fast type-check without linking
cargo build -p dtmpfs-proto      # single crate
cargo test -p dtmpfs-common      # unit tests for common
RUST_LOG=debug cargo run --bin metasrv -- --config config/meta.toml
```

## Crate map

| Crate            | Binary         | Purpose                                              |
|------------------|----------------|------------------------------------------------------|
| `dtmpfs-proto`   | (library)      | tonic-generated gRPC stubs for Meta + Store services |
| `dtmpfs-common`  | (library)      | Shared types, HRW hashing, config, errors            |
| `dtmpfs-meta`    | `metasrv`      | Single authoritative metadata server (port 7100)     |
| `dtmpfs-store`   | `storesrv`     | Block storage server (port 7200+)                    |
| `dtmpfs-client`  | `dtmpfs-mount` | FUSE client that mounts the filesystem               |

## Dependency order

```
dtmpfs-proto ──┐
               ├──> dtmpfs-meta
dtmpfs-common ─┤──> dtmpfs-store
               └──> dtmpfs-client
```

proto and common have no internal dependencies. Build them first.

## Key design decisions (summary)

- gRPC over HTTP/2 via tonic 0.12 / prost 0.13
- FUSE via `fuser` 0.14 (user-space, libfuse3)
- Close-to-open consistency (NFSv3 semantics)
- HRW (Rendezvous) hashing for block placement — `(ino, block_idx)` maps to a ranked list of stores
- Single `tokio::sync::RwLock<MetaState>` on the meta — one authority, no Raft in v1
- 1 MiB blocks, `bytes::Bytes` end-to-end on the data path
- Cluster auth via static `cluster-token` header on every RPC

## Important files

- `docs/HLD.md` — high-level design, requirements, data flows
- `docs/LLD.md` — low-level design with Rust types, algorithms, pseudocode
- `docs/protocol.md` — full proto3 definitions and RPC semantics
- `docs/configuration.md` — TOML config reference
- `docs/decisions.md` — 18 resolved design decisions (resolve any ambiguity here first)
- `docs/consistency.md` — close-to-open contract details
- `config/meta.toml`, `config/store.toml`, `config/client.toml` — working single-host configs

## Proto files

The `.proto` source lives in `proto/meta.proto` and `proto/store.proto`.
`crates/dtmpfs-proto/build.rs` compiles them via `tonic_build`.
Each agent working on a crate that imports proto types should NOT modify these files.
Only the `dtmpfs-proto` agent writes to `proto/`.

## Ports and defaults

| Service | Default port | Config key   |
|---------|-------------|--------------|
| meta    | 7100        | `listen`     |
| store   | 7200, 7201… | `listen`     |
| debug   | 7300+       | `debug_http_listen` |
