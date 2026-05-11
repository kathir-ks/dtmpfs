# dtmpfs — Testing Strategy

This document describes how dtmpfs is tested: the levels of the test
pyramid, the tooling used at each level, the test environment setup,
the categories of tests, the per-phase gates, the non-determinism
budget, the bug-bash protocol, and the regression policy.

The companion document, [acceptance-tests.md](acceptance-tests.md),
contains the concrete acceptance test cases (each spelled out at
copy-paste fidelity). This document is the strategy; that one is the
checklist.

For project-wide context see:

- [HLD.md](HLD.md) — system architecture
- [LLD.md](LLD.md) — module-level design
- [architecture.md](architecture.md) — diagram-heavy overview
- [protocol.md](protocol.md) — wire protocol (gRPC `.proto`)
- [consistency.md](consistency.md) — close-to-open semantics
- [failure-model.md](failure-model.md) — what we tolerate, what we don't
- [operations.md](operations.md) — running the cluster
- [configuration.md](configuration.md) — TOML layout
- [README.md](../README.md) — top-level entry point


## 1. Testing pyramid

dtmpfs uses a four-level pyramid. Lower layers are broad, fast, and
cheap. Higher layers are narrow, slow, and expensive but more
realistic.

```
                       +-----------------+
                       |   Acceptance    |   ~50 manual/automatable
                       |   (user-facing) |   end-to-end scenarios;
                       +-----------------+   gate v1 release
                      /                   \
                     /                     \
                    +-----------------------+
                    |       System          |   tests/smoke.sh
                    |  (shell, real /mnt)   |   tests/multi_host.sh
                    +-----------------------+
                   /                         \
                  /                           \
                 +-------------------------------+
                 |         Integration           |   tests/ crate;
                 |  (full process tree, tempdir) |   spawn meta + 2
                 +-------------------------------+   stores + client
                /                                 \
               /                                   \
              +-------------------------------------+
              |               Unit                  |   #[cfg(test)] mod;
              |  (pure functions, no I/O, no net)   |   per-crate tests
              +-------------------------------------+
```

### 1.1 Unit tests

Live next to the code they test, in `#[cfg(test)] mod tests` blocks
inside each `crates/<x>/src/<file>.rs`. They cover only logic that has
no I/O and no network.

In-scope examples:

- `dtmpfs-common::hash`: HRW (Highest-Random-Weight) ranking — given a
  fixed key and a fixed `nodes: &[NodeId]`, the top-K ranking is
  stable; rotating nodes changes only the affected keys; the function
  is deterministic per-input.
- `dtmpfs-common::config`: `serde` round-trip of every `Config`
  variant (`Meta`, `Store`, `Client`); rejection of malformed TOML
  with a useful error.
- `dtmpfs-common::error`: every internal error variant maps to the
  expected `libc::E*` constant via `to_errno()`.
- `dtmpfs-common::id`: `BlockKey`, `NodeId`, `InoNum` newtype invariants
  (no zero `ino`, generation monotonic, etc.).
- `dtmpfs-meta::alloc`: inode allocation never reuses a live ino,
  generation increments on `close-with-dirty`.
- `dtmpfs-meta::inode`: `BTreeMap<BlockIdx, BlockPlacement>` mutations
  preserve sorted order; rename across parents is atomic in-memory.
- `dtmpfs-client::cache`: `BlockCache::get_or_insert_with` evicts under
  pressure; `AttrCache` honours the 1 s TTL.
- `dtmpfs-client::fs::off_to_block(off, len, bs) -> impl Iterator`:
  block-range math is correct for boundary cases (offset=0, offset=bs,
  offset=bs-1, len=0, len=1, len=bs, len=bs+1, off+len > size).

These run in well under a second each. They are the first line of
defence and the one tests run on every save during development:

```bash
cargo test --workspace --lib
```

### 1.2 Integration tests

Live in `tests/` of each crate (Rust's standard out-of-band integration
test directory) and a top-level `dtmpfs/tests/integration/` for tests
that span more than one crate. They spawn real processes — `metasrv`,
`storesrv`, `dtmpfs-mount` — on ephemeral ports, mount FUSE on a
`tempfile::TempDir`, and exercise the FS via `std::fs`.

A typical integration test looks like:

```rust
#[test]
fn write_then_read_small_file() {
    let cluster = TestCluster::builder()
        .meta(1).stores(2).clients(1)
        .heartbeat_ms(200).heartbeat_timeout_ms(1000)
        .build()
        .expect("spawn cluster");

    let mnt = cluster.client(0).mountpoint();
    std::fs::write(mnt.join("hi"), b"hello").unwrap();
    let s = std::fs::read_to_string(mnt.join("hi")).unwrap();
    assert_eq!(s, "hello");
} // TestCluster::drop kills processes and unmounts.
```

Key invariants for integration tests:

- Each test builds its own `TestCluster`. No shared global state.
- Ports are `0.0.0.0:0` and read back from the bound listener so
  parallel `cargo test` runs don't collide.
- The mountpoint is a fresh `tempfile::TempDir` per test.
- Cleanup runs even on panic (RAII via `Drop` on `TestCluster`,
  including `fusermount3 -u`).
- Heartbeat and timeout intervals are configurable per-test (defaults
  in production are 5 s; tests use 200 ms / 1 s).

Run them with:

```bash
cargo test --workspace --release
```

`--release` matters: a few timing-sensitive tests fail at debug speed
on a busy CI runner.

### 1.3 System tests

Shell scripts under `tests/`:

- `tests/smoke.sh` — single-host smoke: mounts, writes a few files,
  reads them back, unmounts. Runs in ~5 s.
- `tests/multi_host.sh` — takes `HOST_A` and `HOST_B` env vars, SSHes
  to each, mounts on both, runs the cross-host visibility scenario.
- `tests/sharding.sh` — Phase 3+: writes 256 blocks, queries each
  store's `/debug/blocks` HTTP endpoint, asserts spread is within
  ±20%.
- `tests/chaos.sh` — Phase 6+: 10-minute soak; random store kills
  every 30 s; assert `md5sum` of a continuously rewritten file is
  consistent at each closing barrier.

These run against the actual `/mnt/dtmpfs` mountpoint configured in
`config/`. They exit non-zero on failure, with a clear final line of
the form `FAIL: <reason>` or `PASS`. They are run from the repo root.

### 1.4 Acceptance tests

Specified in [acceptance-tests.md](acceptance-tests.md). These are the
contract dtmpfs commits to. They are written as exact step-by-step
recipes (`cmd`, `expected stdout`, `expected exit code`, `pass
criteria`), and most are also encoded as integration or system tests
behind the scenes — but the document is the human-readable source of
truth, with at least 50 numbered cases (`A-001` through ~`A-205`).


## 2. Tooling

### 2.1 Unit + integration runner

```bash
cargo test --workspace --release
```

This runs:

- every `#[cfg(test)] mod tests` in every crate (unit),
- every file in every crate's `tests/` directory (integration),
- and the top-level `dtmpfs/tests/integration/` if the workspace
  manifest includes it as a member.

Useful flags:

- `cargo test -- --nocapture` to see `println!` from a flaky test.
- `cargo test --workspace --release -- --test-threads=1` to serialize
  if you suspect cross-test interference (should never be needed —
  filing a bug if so).
- `cargo test --workspace --release -- --ignored` to run tests marked
  `#[ignore]` (long-running soak/chaos tests live there).

### 2.2 Shell runner

```bash
bash tests/smoke.sh
bash tests/multi_host.sh
bash tests/sharding.sh
bash tests/chaos.sh
```

Each script:

- exits non-zero on any failure (`set -euo pipefail`),
- prints a final `PASS` or `FAIL: <reason>` line,
- cleans up its mounts, processes, and tempdirs even on failure
  (`trap` on `EXIT`).

### 2.3 Test isolation: `TestCluster`

The integration tests use a helper struct `TestCluster` (in
`crates/dtmpfs-testutil/src/lib.rs`). Sketch:

```rust
pub struct TestCluster {
    tmp:        tempfile::TempDir,
    meta_proc:  Vec<Child>,
    store_proc: Vec<Child>,
    client_proc:Vec<Child>,
    mount_pts:  Vec<PathBuf>,
    meta_addr:  String,
}

impl TestCluster {
    pub fn builder() -> TestClusterBuilder { ... }
    pub fn client(&self, i: usize) -> ClientHandle { ... }
    pub fn store(&self, i: usize) -> StoreHandle { ... }
    pub fn meta(&self) -> MetaHandle { ... }
}

impl Drop for TestCluster {
    fn drop(&mut self) {
        for m in &self.mount_pts {
            let _ = std::process::Command::new("fusermount3")
                .args(["-u", "-z", m.to_str().unwrap()])
                .status();
        }
        for c in self.client_proc.iter_mut()
                  .chain(self.store_proc.iter_mut())
                  .chain(self.meta_proc.iter_mut()) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
```

Builder defaults:

- 1 meta, 2 stores, 1 client.
- Heartbeat 200 ms, timeout 1 s, attr-cache TTL 200 ms.
- Block size 1 MiB (production default).
- Replication factor 1.
- `cluster_token = "test-token"`.
- Ports: ephemeral via `0.0.0.0:0`; the binary writes the bound port
  to a tempfile, the test reads it back.

Tests should not assume process startup is instant. The builder polls
`Meta.ListNodes` until it sees the expected node count or 10 s
elapses, then returns the cluster handle.

### 2.4 Coverage

```bash
cargo install cargo-llvm-cov
cargo llvm-cov --workspace --html
```

Targets by Phase 6:

- `dtmpfs-common`: ≥ 70% line coverage (it's pure, this is achievable).
- `dtmpfs-meta`: ≥ 60%.
- `dtmpfs-store`: ≥ 60%.
- `dtmpfs-client`: ≥ 60%. FUSE callback tests are covered through
  integration; unit-level coverage of the cache and RPC layer is
  what's measured.

Coverage is informational, not a merge gate. It's a smoke signal for
"did we forget to test this module entirely". A class of bug we want
to catch — missing error path on a specific RPC failure — won't
necessarily be exercised by line coverage but will by the failure
injection tests.

### 2.5 Property-based testing

`proptest` is used for:

- HRW hashing (`crates/dtmpfs-common/src/hash.rs`):
  - **Idempotence**: `top_k(key, nodes) == top_k(key, nodes)` for any
    permutation of `nodes`.
  - **Stability**: removing a node only affects keys whose top-K
    ranking included that node. Adding a node can only relocate keys
    that now have it in the top-K (this is the property HRW gives us
    over modulo).
  - **Balanced distribution**: for `N=4` nodes and `K=10_000` keys,
    each node ranks first for `K/N ± 5%` keys.
- Inode allocation (`crates/dtmpfs-meta/src/alloc.rs`):
  - No two `alloc()` calls return the same ino.
  - `free()` followed by `alloc()` may reuse, but never returns an ino
    with a live entry in `inodes`.
- Block-range math (`crates/dtmpfs-client/src/fs.rs`):
  - For arbitrary `offset`, `len`, `block_size`, the iterator yields
    block ranges that sum to `len`, are non-overlapping, and have
    `range.0 < block_size` for the first range, `range.1 <=
    block_size` for the last.

Property tests live alongside unit tests; they are filtered with
`cargo test --workspace --lib prop_`.

### 2.6 Fuzzing

`cargo-fuzz` is set up as Phase 5+ work, not v1:

- `fuzz_targets/parse_meta_proto.rs` — feeds raw bytes to `prost`
  decoding of every `meta.proto` message; asserts no panic.
- `fuzz_targets/parse_store_proto.rs` — same for `store.proto`.
- `fuzz_targets/config_toml.rs` — feeds raw bytes to
  `dtmpfs-common::Config::parse`; asserts no panic.

Fuzzers are run nightly, 1 hour each, in a separate CI job. Crashes
are reported as P0 bugs.

### 2.7 Race / data-race detection

ThreadSanitizer:

```bash
RUSTFLAGS="-Z sanitizer=thread" \
  cargo +nightly test --workspace --release \
       --target x86_64-unknown-linux-gnu
```

This runs only on a nightly toolchain in CI, not on every PR. The
production toolchain is stable 1.94.

Loom (`loom` crate) is used selectively in `dtmpfs-client::cache` for
the few places we have lock-free invariants — specifically the
`AttrCache` swap-on-expiry path.

### 2.8 FUSE quirks

Two oddities every author of an integration test must internalize:

1. **Visibility across mounts requires `sync`.** Process A writes,
   process A's kernel buffers the write, FUSE callback `flush` on
   close pushes to the cluster. Process B's kernel still has nothing
   that triggered an `open` on B, so its `AttrCache` is what it is.
   In tests we either:
   - call `std::process::Command::new("sync").status().unwrap()` after
     the writer's `close`, or
   - write to A, `close` it, **then** call `open` on B (which
     bypasses the AttrCache by design — see
     [consistency.md](consistency.md) §"Open is the invalidation
     point").
   - Or, when we control both ends, wait `attr_cache_ttl_ms + 100 ms`.
   The first is preferred for speed.

2. **`fsync(fd)` forces `Meta.Close`-equivalent.** Because dtmpfs
   maps `fsync` to "flush dirty blocks + bump generation under the
   meta lock", `fsync` is the right primitive for "make this write
   visible". Tests should call `f.sync_all()?` (which does `fsync`)
   before reading from another mount.

These are documented again in [consistency.md](consistency.md). If a
test ignores them, it will be flaky.

### 2.9 Logging during tests

Tests set `RUST_LOG=dtmpfs=debug,info` and capture per-process
stdout/stderr to files under the cluster's tempdir. On test failure,
`TestCluster::drop` writes a `failure-summary.log` that concatenates
the last 200 lines of each. Read it before reaching for a debugger.


## 3. Test environment

### 3.1 Local development

A single VM with FUSE 3.10.5, Rust 1.94, `protobuf-compiler`. Each
integration test spawns its own cluster on ephemeral ports under
`/tmp/dtmpfs-test-XXXX`. No root required for tests (unlike the
production mount, which uses `AllowOther` and reads
`/etc/fuse.conf`). Tests use `MountOption::AutoUnmount` and a
`tempdir` mountpoint owned by the test process.

### 3.2 CI

GitHub Actions, Ubuntu 22.04 image. The job:

```yaml
- run: sudo apt-get update && sudo apt-get install -y \
       libfuse3-dev fuse3 pkg-config protobuf-compiler
- run: sudo modprobe fuse
- run: cargo test --workspace --release
- run: bash tests/smoke.sh
- run: bash tests/sharding.sh    # Phase 3+
```

CI requirements:

- The runner must expose `/dev/fuse` to the workflow. GitHub-hosted
  runners do; some self-hosted runners need `--privileged` or
  `--device /dev/fuse` if running in containers.
- `fusermount3` SUID is set on Ubuntu 22.04 by default.
- Coverage and ThreadSanitizer run as separate nightly jobs to keep
  PR feedback under 5 minutes.

### 3.3 Multi-host

Optional. `tests/multi_host.sh` is invoked as:

```bash
HOST_A=10.0.0.10 HOST_B=10.0.0.11 bash tests/multi_host.sh
```

It expects passwordless SSH between the runner and each host, both
hosts having the same release binary at `/usr/local/bin/dtmpfs-mount`,
and a meta server already running on `HOST_A`. Documented step-by-step
in [operations.md](operations.md) §"Multi-host bring-up".


## 4. Test categories

### 4.1 Functional

Every FUSE method works for the happy path. One test per method,
plus a few per-method variant tests (e.g. `setattr` with mode, with
size, with utimens). Lives in unit + integration. Covered by
acceptance tests `A-010` through `A-038`.

### 4.2 Cross-host visibility

The marquee feature: write on A, close, read on B. Lives in
integration (two clients, same VM) and system (two clients, two VMs).
Covered by `A-050` through `A-055`.

### 4.3 Concurrency

Many parallel reads, many parallel writes, mixed. Covered by `A-130`
through `A-134`. A typical concurrency test:

- Writer thread: `std::fs::write(p, data); barrier.wait();`
- 99 reader threads: `barrier.wait(); std::fs::read(p);`
- Assert all readers see the same content.

Concurrency tests are inherently flaky if your assumptions are wrong;
we make assumptions explicit in `consistency.md`. Test failures here
are usually correctness bugs in the FS, not the test.

### 4.4 Sharding

Blocks land on the right stores per HRW. After writing N blocks across
M stores, query each store's `/debug/blocks` and assert the
distribution is within ±20% of `N/M`. Covered by `A-070` through
`A-073`.

### 4.5 Replication

With `R=2`, tolerate one store death. Read still succeeds. Covered by
`A-090` through `A-094`. Phase 5+.

### 4.6 Failure injection

Kill processes mid-op; assert behaviour matches
[failure-model.md](failure-model.md). Covered by `A-110` through
`A-115`. Includes:

- Kill a store, observe `EIO` for reads of its blocks (R=1 case).
- Kill the meta, observe global `EIO`.
- Kill a client mid-write (process kill), observe dirty data is lost.
- iptables-induced partition.

### 4.7 Performance / smoke

Throughput and latency floors. Not regression-tracked in v1; we just
want a "sanity" line: localhost write ≥ 200 MB/s, read ≥ 300 MB/s
cached. Covered by `A-180` through `A-184`. Run manually before
release; numbers logged but not enforced.

### 4.8 POSIX conformance (loose)

A hand-picked subset of `pjdfstest`:

- `tests/conformance/open` — open with O_CREAT, O_EXCL, etc.
- `tests/conformance/mkdir`
- `tests/conformance/rmdir`
- `tests/conformance/rename`
- `tests/conformance/unlink`
- `tests/conformance/chmod`
- `tests/conformance/truncate`

Full `pjdfstest` is a Phase-6 goal. Many `pjdfstest` cases will fail
on a v1 dtmpfs (no hardlinks, no xattrs, no special files, no
suid/sgid bits) — those are documented as known
non-conformances in [consistency.md](consistency.md).


## 5. Phase-aligned test gates

Each phase ships when these tests pass.

| Phase | Scope                                  | Definition-of-done test                                                                  |
|-------|----------------------------------------|------------------------------------------------------------------------------------------|
| P1    | Single-process FUSE shim               | `bash tests/smoke.sh` passes on local mount.                                             |
| P2    | Split client/store, gRPC               | `smoke.sh` passes; integration test asserts bytes are present in store process memory.   |
| P3    | Sharding via HRW                       | `bash tests/sharding.sh` passes (256 blocks across 4 stores, max-min spread ≤ 20%).      |
| P4    | Meta service, generation, AttrCache    | Cross-host visibility test (`A-050`) passes between two mounts on same VM.               |
| P5    | Replication R≥2                        | Kill-one-store test (`A-092`) passes with `R=2` and 3 stores.                            |
| P6    | Heartbeats, retries, GC                | Chaos test (`bash tests/chaos.sh`, 10 min, random kills) passes.                         |

A phase doesn't merge to `main` until its DoD test is in CI and green.


## 6. Non-determinism budget

Distributed systems testing is full of timing assumptions. We budget
for them explicitly.

### 6.1 Heartbeat timing

Production heartbeat is 5 s; a store goes `Down` after 5 misses
(25 s). Tests that wait for failure detection at production timing
take ≥ 25 s. We avoid that by making heartbeats configurable per
test:

- `heartbeat_ms = 200`
- `heartbeat_timeout_ms = 1000`

So a "wait until store-2 marked Down" assertion takes ~1 s.

### 6.2 AttrCache TTL

Production `attr_cache_ttl_ms = 1000`. Tests that need cross-mount
visibility either:

- call `sync` between writer-close and reader-open,
- call `f.sync_all()` (`fsync`) before close,
- or wait `attr_cache_ttl_ms + 100 ms`.

Prefer `sync` — the tests are 1.1 s faster each.

### 6.3 DashMap iteration order

`DashMap` does not guarantee iteration order. Any test that does
`store.list_blocks()` and asserts a specific order is wrong. Tests
sort the result before comparing, or use set membership (`assert!(b
in expected)`).

### 6.4 HRW depends on node-set

The HRW result is a function of `(key, nodes)`. `nodes` is whatever
the meta currently believes is `Up`, which is a function of recent
heartbeats. A test that asserts "block X went to store Y" must:

1. Wait for all stores to register and heartbeat.
2. Drain the meta's internal heartbeat watcher to a quiescent state
   (an internal API exposed for tests, or a 2-heartbeat-interval
   sleep).
3. Then write the block and assert.

Otherwise the placement decision can race with a node going up/down
and break the assertion.


## 7. Bug-bash protocols

Once Phase 5 lands and replication is real, we run a 1-day structured
bug bash before tagging the release.

Format:

- 2-3 people, half a day each.
- Two VMs (or two mounts on one VM) preprovisioned.
- Each runs through the "Manual exploratory checklist" in
  [acceptance-tests.md](acceptance-tests.md) §"Manual exploratory
  checklist".
- Found bugs are filed with: exact reproduction (commands), expected
  vs actual, log snippets from `RUST_LOG=debug` of the relevant
  process, severity.

Bugs found in bug-bash are triaged as:

- **P0** — data loss, crash, hang, security: blocks release, fix
  before tag.
- **P1** — wrong errno, wrong size, wrong content under reasonable
  conditions: blocks release.
- **P2** — perf regression > 30% from baseline: discuss.
- **P3** — UX / log readability: backlog.

Every P0/P1 fix gets a regression test in the same PR. No exceptions.


## 8. Regression policy

1. **Every fixed bug becomes a test.** The PR that fixes a bug must
   include a test that fails before the fix and passes after. No "we
   will add a test in a follow-up". Reviewers reject PRs that fix a
   bug without a regression test.

2. **Tests are owned.** When a test breaks on `main`, the author of
   the change that broke it owns the fix. If they can't reproduce
   locally within an hour, they revert.

3. **Flaky tests are quarantined within 24 hours.** If a test fails
   intermittently in CI and the cause isn't immediately obvious:
   - `#[ignore = "flaky, see #NNN"]` it,
   - file a P0 bug,
   - fix or delete within one week.
   A flaky test in `main` is worse than no test, because it trains
   reviewers to ignore CI.

4. **No skipped tests in release.** Release branches must have zero
   `#[ignore]` and zero `# SKIP` lines. The release manager grep-checks
   for these as part of the tag procedure.


## 9. Local development workflow

The minimum loop while developing:

```bash
# unit tests, fast
cargo test --workspace --lib

# integration on the change you just made
cargo test --workspace --release <test_name_substring>

# system smoke before pushing
bash tests/smoke.sh
```

Before opening a PR:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --release
bash tests/smoke.sh
bash tests/sharding.sh   # if Phase 3+ work
```

CI runs the same set plus coverage, sanitizers, and (nightly) chaos.


## 10. What is not tested in v1

To keep scope honest, here are things this strategy explicitly does
not cover. They are tracked separately in
[failure-model.md](failure-model.md) §"Out of scope":

- Encryption / TLS / mTLS — no auth tests beyond `cluster_token`.
- Crash-consistency through power loss — dtmpfs is RAM-only by design.
- Disk-backed durability tests — no disk involvement.
- `mmap` write-through coherence — kernel falls back to `read`/`write`
  for small files; large `mmap MAP_SHARED` writes are not specified.
- Hardlinks (`link` returns `EPERM`); `getxattr` returns `ENOSYS`.
- Special files (FIFOs, sockets, char/block devices).
- Quotas, snapshots, clones.
- Re-replication after a store dies (Phase 7+).
- Raft for meta (Phase 7+).
- Background block GC (Phase 6+ partial; v1 has minimal GC).

When a user reports something in this list, we link them to this
section and to [failure-model.md](failure-model.md).
