# dtmpfs

A distributed in-memory filesystem written in Rust. Mount the same namespace on multiple Linux hosts; file data lives in RAM, sharded (and optionally replicated) across a set of storage nodes. Drop a file on host A, read it on host B once the writer closes.

**Status:** alpha / prototype. RAM-only. Single-meta SPOF in v1. Trusted-LAN only — no TLS, no auth beyond a shared cluster token. Not production storage. Power loss and process exit are equivalent: data is gone.

## What it is

- A **FUSE filesystem** (user-space, via `libfuse3` and the `fuser` Rust crate).
- A small Rust workspace producing three binaries: `metasrv`, `storesrv`, `dtmpfs-mount`.
- Sharded by **rendezvous (HRW) hashing** on `(inode, block_idx)`, with 1 MiB blocks.
- **Close-to-open consistency**, the same model NFS uses: `close()` on the writer makes the new contents visible to subsequent `open()` calls anywhere.

## What it isn't

- **Not a Linux tmpfs.** The name borrows the *properties* of tmpfs (RAM-backed, ephemeral, fast) but dtmpfs is a distributed FUSE filesystem. It does not interact with the kernel's `tmpfs` driver. The `t` in the name is for "transient", not "tmpfs(5)".
- **Not durable.** There is no disk. There is no journal. There is no fsync-to-platter. `fsync()` flushes dirty blocks across the network to peer RAM and bumps a generation counter — that's it. If every node loses power, every file is gone.
- **Not strongly consistent.** Two writers on different hosts opening the same file will not see each other's in-flight writes. Last-close-wins per block. See `docs/consistency.md`.
- **Not a replacement for NFS, Ceph, or Lustre.** It targets a narrower slice: scratch space across a small trusted cluster where RAM is cheap and disk is too slow.
- **Not Raft-replicated for metadata in v1.** The meta server is a single point of failure. Phase 7 stretches into `openraft`; v1 does not.

## Target use cases

- ML training scratch shared across worker hosts (intermediate tensors, dataset shards, checkpoints that will be re-uploaded elsewhere).
- Distributed build cache with hot working set in RAM.
- Generic POSIX share for short-lived artifacts on a small trusted LAN (2–8 nodes).

## Concepts at a glance

Five terms recur throughout the docs and the codebase. Skim these before reading further.

- **Inode** — the metadata record for one file or directory, owned by `dtmpfs-meta`, identified by a 64-bit `ino`.
- **Block** — a 1 MiB chunk of file data stored on a `dtmpfs-store` node, keyed by `BlockKey { ino, block_idx, generation }`.
- **Generation** — a per-inode counter bumped on every `close()` that flushed dirty blocks. Acts as the cache-coherence epoch on the client.
- **Primary / replica** — for each block, HRW hashing chooses an ordered list of `R` nodes. The first is the primary; the rest are replicas used for read failover.
- **HRW (rendezvous hashing)** — the placement function that picks which storage nodes hold a given block.

The full glossary is in [`docs/HLD.md` §2](docs/HLD.md#2-glossary).

## Quickstart

### Prerequisites

- Linux with FUSE 3.10+ kernel support
- `libfuse3-dev` (Debian/Ubuntu) or `fuse3-devel` (RHEL/Fedora)
- `protobuf-compiler` (`protoc`) for the build
- Rust 1.94 (pinned via `rust-toolchain.toml`)
- `pkg-config`

### One-time host setup

Run as root:

```bash
sudo apt-get install -y libfuse3-dev pkg-config protobuf-compiler
sudo modprobe fuse
sudo sh -c 'echo user_allow_other >> /etc/fuse.conf'
mkdir -p /mnt/dtmpfs && sudo chown $USER /mnt/dtmpfs
```

The `user_allow_other` line in `/etc/fuse.conf` is required so the FUSE mount can be accessed by users other than the one running `dtmpfs-mount`. If you only want the mounting user to see the filesystem, drop `MountOption::AllowOther` in the client config and skip that step.

### Build

```bash
git clone https://github.com/<org>/dtmpfs && cd dtmpfs
cargo build --release --workspace
```

Artifacts land in `target/release/`:

- `metasrv` — the metadata server
- `storesrv` — a storage node
- `dtmpfs-mount` — the FUSE client

### Run a 3-process local cluster (one VM, four tmux panes)

Copy the example configs and edit ports / paths:

```bash
cp config/meta.toml.example   config/meta.toml
cp config/store.toml.example  config/store0.toml
cp config/store.toml.example  config/store1.toml
cp config/client.toml.example config/client.toml
# Edit store0.toml -> listen 0.0.0.0:7200, node_id "store-0"
# Edit store1.toml -> listen 0.0.0.0:7201, node_id "store-1"
```

Then in four panes:

```bash
RUST_LOG=info ./target/release/metasrv     --config config/meta.toml      # pane 1
RUST_LOG=info ./target/release/storesrv    --config config/store0.toml    # pane 2
RUST_LOG=info ./target/release/storesrv    --config config/store1.toml    # pane 3
RUST_LOG=info ./target/release/dtmpfs-mount --config config/client.toml   # pane 4
```

### Smoke test

```bash
echo hi > /mnt/dtmpfs/x
cat /mnt/dtmpfs/x                        # -> hi

mkdir /mnt/dtmpfs/d
echo bye > /mnt/dtmpfs/d/y

dd if=/dev/urandom of=/mnt/dtmpfs/big bs=1M count=64
md5sum /mnt/dtmpfs/big
```

To exercise cross-host visibility on one VM, run a second `dtmpfs-mount` against the same `meta_addr` with a different `mount_point` (e.g. `/mnt/dtmpfs-b`), then write on one mount and read on the other after the writer closes.

To unmount:

```bash
fusermount3 -u /mnt/dtmpfs
```

### Cross-mount visibility test

The marquee test for dtmpfs is that a write on one mount becomes visible on a second mount after `close()`:

```bash
echo "from-a" > /mnt/dtmpfs-a/cross && sync
cat /mnt/dtmpfs-b/cross                    # expect: from-a

dd if=/dev/urandom of=/tmp/src bs=1M count=200
cp /tmp/src /mnt/dtmpfs-a/big
md5sum /tmp/src /mnt/dtmpfs-b/big          # md5 must match
```

If the second `cat` returns stale data or `ENOENT`, see [`docs/consistency.md`](docs/consistency.md) — the canonical race is when a reader's `open` interleaves with a writer's still-running `close`.

### Troubleshooting

- **`Transport endpoint is not connected`** when accessing `/mnt/dtmpfs`: `dtmpfs-mount` died or never came up. Check its log; if it's gone, run `fusermount3 -u /mnt/dtmpfs` and start it again.
- **`fusermount3: option allow_other only allowed if 'user_allow_other' is set in /etc/fuse.conf`**: the one-time setup step was skipped. Add the line and retry.
- **Reads return `EIO` after killing a store**: expected at `R=1`; the data on that store is gone. Use `R=2` (set `replication_factor = 2` in `client.toml` and recreate the file) for failover.
- **`Status::unauthenticated`** in client logs: `cluster_token` mismatch between the role configs. All four TOMLs must agree.
- **Meta restarts and everything returns `EIO`**: by design in v1. The meta service has no persistence; a restart is a cluster reset. Re-mount and retry.

## Repository layout

```
dtmpfs/
  Cargo.toml                       # workspace
  rust-toolchain.toml              # channel = "1.94.0"
  README.md                        # this file
  proto/
    meta.proto                     # client <-> meta service
    store.proto                    # client <-> store, store <-> store
  crates/
    dtmpfs-proto/                  # tonic_build compiles the .proto files
    dtmpfs-common/                 # hashing (HRW), config, errors, ID newtypes
    dtmpfs-meta/                   # bin: metasrv (single-node metadata)
    dtmpfs-store/                  # bin: storesrv (RAM block store)
    dtmpfs-client/                 # bin: dtmpfs-mount (FUSE client)
  config/
    meta.toml.example
    store.toml.example
    client.toml.example
  tests/
    smoke.sh                       # single-host black-box smoke test
    integration/                   # spawns a full cluster as child processes
  docs/
    HLD.md                         # high-level design
    architecture.md                # diagrams + dataflow walkthroughs
    LLD.md                         # low-level design (data structures, algorithms)
    protocol.md                    # gRPC protocol spec
    consistency.md                 # close-to-open semantics deep-dive
    failure-model.md               # failure modes and recovery
    operations.md                  # deployment, monitoring, troubleshooting
    configuration.md               # config reference
    testing.md                     # test strategy
    acceptance-tests.md            # concrete acceptance tests
```

## Documentation index

- [`docs/HLD.md`](docs/HLD.md) — high-level design: goals, architecture overview, key decisions, roadmap.
- [`docs/architecture.md`](docs/architecture.md) — system diagram, sequence diagrams for every flow, threading model.
- [`docs/LLD.md`](docs/LLD.md) — low-level design: structs, algorithms, locking, data layouts.
- [`docs/protocol.md`](docs/protocol.md) — full gRPC service definitions and field semantics.
- [`docs/consistency.md`](docs/consistency.md) — close-to-open consistency, generation counters, the canonical race.
- [`docs/failure-model.md`](docs/failure-model.md) — what happens when a store dies, when meta dies, on partition.
- [`docs/operations.md`](docs/operations.md) — deployment, log levels, metrics, troubleshooting.
- [`docs/configuration.md`](docs/configuration.md) — every config field, per role.
- [`docs/testing.md`](docs/testing.md) — unit, integration, soak, fault-injection.
- [`docs/acceptance-tests.md`](docs/acceptance-tests.md) — concrete pass/fail tests per phase.

## Phased roadmap

Each phase is independently testable; the smoke test above grows monotonically.

| Phase | Scope                                                                   | Effort      | Pass test                                              |
|-------|-------------------------------------------------------------------------|-------------|--------------------------------------------------------|
| P1    | Single-process FUSE shim, `Mutex<HashMap>` storage                      | ~1 weekend  | Local smoke test                                       |
| P2    | Split client / store; gRPC; data on one store                           | ~1 week     | Same smoke; bytes visible in store process             |
| P3    | Multi-store sharding via HRW; static node list                          | ~1 week     | Two stores, blocks split roughly evenly                |
| P4    | Metadata service, generation, AttrCache                                 | ~1.5 weeks  | Two clients see each other's writes after close        |
| P5    | Replication R >= 2                                                      | ~1 week     | Kill one store with R=2, reads succeed                 |
| P6    | Heartbeats, stale-write rejection, GC, retries                          | ~1 week     | Soak test with random store kills                      |
| P7    | (stretch) Raft for meta via `openraft`                                  | weeks       | 3-meta cluster survives 1 meta kill                    |

## Contributing

Patches welcome. Before sending one, please read [`docs/HLD.md`](docs/HLD.md) and [`docs/consistency.md`](docs/consistency.md) so we share vocabulary. The cache-coherence and generation-bump protocol is subtle; changes there require a corresponding update to `docs/consistency.md` and at least one new acceptance test in `docs/acceptance-tests.md`.

## License

TBD — placeholder until the project chooses one. Until then, treat all sources as "all rights reserved" by the contributors.
