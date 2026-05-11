# dtmpfs — Design Decisions Record

> **Status**: Authoritative. When any other doc disagrees with this one, this doc wins and the other doc must be updated.
> **Created**: 2026-04-29 in response to the cross-doc consistency audit.
> **Audit reports**: `docs/_audit/{protocol,consistency,config,failure,test-coverage}-audit.md`.

This document resolves every contradiction surfaced by the audit. Decisions are split into two categories:

- **D-series (User-decided)**: genuinely contested choices that required input from the project owner.
- **R-series (Audit-resolved)**: choices the audit made unambiguous (clear majority across docs, or clear conflict with the originating plan). Recorded here so the propagation agent has a single source of truth.

Each decision has: **the chosen value**, **the alternatives considered**, **the rationale**, and **the docs that need to be updated** to match.

---

## D-series: User-decided

### D1. gRPC cluster-auth metadata header name

**Chosen**: `x-cluster-token`

**Alternatives considered**: `x-dtmpfs-token` (2 docs), `cluster-token` (1 doc).

**Rationale**: Most-supported name across docs (3-vs-2-vs-1). The `x-` prefix matches the common gRPC convention for application-specific headers and avoids future collision with reserved gRPC headers. Greppable.

**Docs to update**:
- `protocol.md` — change `cluster-token` → `x-cluster-token` everywhere it appears (RPC framing, examples, error responses).
- `HLD.md` — change `x-dtmpfs-token` → `x-cluster-token`.
- `architecture.md` — change `x-dtmpfs-token` → `x-cluster-token`.

**Implementation note**: header value is the verbatim `cluster_token` config string. Server validates with constant-time comparison.

**Revision**: the implementation used `cluster-token` (no `x-` prefix) — `tonic`'s `MetadataKey::from_static` requires lowercase ASCII with no `_`, and `x-cluster-token` was not applied in the initial agent code. All live docs now reflect `cluster-token` as the canonical wire name.

---

### D2. Config-key naming convention

**Chosen**: `configuration.md` style.

**Canonical key names**:
| Key | Value semantics |
|---|---|
| `listen` | bind address, format `ip:port` |
| `heartbeat_interval_ms` | how often a store sends a heartbeat |
| `heartbeat_timeout_ms` | meta declares a store Down after this elapses with no heartbeat |
| `debug_http_listen` | optional debug HTTP server bind address (store only) |
| `ram_budget_bytes` | per-store RAM cap (units: bytes, not MiB) |
| `block_cache_capacity_mb` | client BlockCache LRU cap (units: MiB) |
| `block_size` | client block size (units: bytes) |
| `attr_cache_ttl_ms` | client AttrCache TTL and FUSE entry/attr_timeout |
| `replication_factor` | u8, 1..=3 |
| `cluster_token` | shared auth secret, ≥16 chars |
| `node_id` | unique cluster-wide ID, `[a-z0-9-]+`, ≤63 chars |
| `mount_point` | client only; absolute path |
| `meta_addr` | URL, e.g., `http://10.0.0.10:7100` |

**Alternatives considered**: LLD's `bind_addr`, `heartbeat_dead_ms`, `debug_http_bind` style; or hybrid.

**Rationale**: `configuration.md` is the user-facing reference; example TOMLs are written against it; most cross-doc references already use it. Implementation structs should rename to match.

**Docs to update**:
- `LLD.md` — rename struct fields:
  - `MetaConfig::bind_addr` → `listen`
  - `MetaConfig::heartbeat_dead_ms` → `heartbeat_timeout_ms`
  - `StoreConfig::bind_addr` → `listen`
  - `StoreConfig::debug_http_bind` → `debug_http_listen`
  - `ClientConfig::bind_addr` → (n/a, client doesn't bind)
- `acceptance-tests.md` — rename `ram_budget` → `ram_budget_bytes` in test configs.
- `HLD.md` — change `store.capacity_mb` references to `ram_budget_bytes` (with bytes units).

---

### D3. `fsync(2)` semantics

**Chosen**: equivalent to `flush()` — pushes dirty blocks, calls `Meta.Close`, bumps `Inode.generation`. With `R≥2`, waits for replica ACKs in addition to primary ACKs.

**Alternatives considered**: local-only flush without generation bump; no-op.

**Rationale**: Provides cross-host visibility on `fsync` (databases and build tools rely on this). Matches HLD/LLD/architecture/testing (5 docs). The "R≥2 wait for replicas" detail is the one extra guarantee `fsync` provides over `flush`.

**Spec**:
- `fsync(fd)` — wait for primaries; with R≥2, wait for ALL replicas; then `Meta.Close` (which bumps generation if dirty).
- `fdatasync(fd)` — same as `fsync` in v1 (no separate "data only" path; metadata always flushed).
- `flush()` (called on `close(2)`) — wait for primaries only; `Meta.Close` (bumps generation if dirty). Replicas may still be in-flight when `close()` returns; this is fine for close-to-open semantics because subsequent opens hit the meta which already has the new placement.

**Docs to update**:
- `consistency.md` §7.5 — rewrite to say fsync DOES bump generation.
- `LLD.md` §6.5 — clarify replica-wait policy: `flush` waits primaries-only; `fsync` waits all replicas with R≥2.
- `architecture.md` §4.6 — same clarification.

---

### D4. v1 performance targets (NFRs)

**Chosen**: lower, achievable-on-commodity-LAN targets.

**Committed NFRs (v1)**:
| Metric | Target |
|---|---|
| Loopback sequential read (1 MiB blocks, 1 GB file) | ≥ 500 MiB/s |
| Loopback sequential write (1 MiB blocks, 1 GB file) | ≥ 300 MiB/s |
| 10 GbE LAN sequential read | ≥ 800 MiB/s |
| 10 GbE LAN sequential write | ≥ 400 MiB/s |
| Meta.Lookup p99 latency | < 5 ms |
| Meta.Open p99 latency | < 10 ms |
| Store.ReadBlock p99 latency (warm) | < 2 ms |
| Mount-to-first-syscall | < 1 s |

**Alternatives considered**: aspirational HLD targets (3 GiB/s loopback); two-tier must/should.

**Rationale**: Aspirational targets all fail acceptance tests today. Honest targets are testable. The roadmap can introduce a `[performance-stretch]` doc later if io_uring / zero-copy work happens.

**Docs to update**:
- `HLD.md` §6.1 — replace 3 GiB/s loopback with 500 MiB/s; replace 800 MiB/s LAN with the above table.
- `acceptance-tests.md` perf tests — verify the target numbers in tests match this table.
- `testing.md` §4.7 — remove the "tests target less than HLD" admission; it's no longer true.

---

## R-series: Audit-resolved (no user input needed)

### R1. fsync ↔ flush ↔ generation bump

Resolved by D3 above; no separate decision needed.

---

### R2. Stale-write rejection: which phase?

**Chosen**: **Phase 6** (deferred).

**Rationale**: Original plan explicitly deferred to Phase 6. HLD §11, failure-model.md §7.3, architecture.md §3.5 all say Phase 6. LLD §5.3 is the outlier (described as v1).

**Docs to update**: `LLD.md` §5.3 — move stale-write rejection out of v1 implementation; mark as Phase 6 hardening. Stores in v1 accept writes blindly (still pass `BlockKey.generation` on the wire so the field exists for Phase 6).

---

### R3. R≥2 read failover: which phase?

**Chosen**: **v1 / Phase 5** (when replication itself ships).

**Rationale**: Plan + 5 docs vs failure-model.md as sole outlier. Read failover is the entire point of replication; deferring it to Phase 6 makes Phase 5's R≥2 useless.

**Docs to update**: `failure-model.md` §3.1 — change "Phase 6" → "Phase 5 (when replication ships)" for read failover. Re-replication after a node death is a separate feature (see R4).

---

### R4. Re-replication after store death: which phase?

**Chosen**: **Phase 8 stretch** (NOT Phase 6).

**Rationale**: HLD §12 explicitly says Phase 8 stretch. Re-replication requires a background scheduler, replica-source selection, and bandwidth throttling — non-trivial. Phase 6 only does eager unlink-time deletion and orphan GC.

**Docs to update**:
- `failure-model.md` §3.1 — change "Phase 6" → "Phase 8 stretch" for re-replication.
- `consistency.md` §5.6 — same.
- `configuration.md` — remove or mark `gc_interval_ms` as Phase 6 only (orphan sweep, not re-replication).

---

### R5. Errno mapping for `Status::resource_exhausted`

**Chosen**: `Status::resource_exhausted → ENOSPC`.

**Rationale**: HLD/failure-model/operations/protocol all promise this mapping. LLD's `tonic::Status → libc errno` table simply omits it.

**Docs to update**: `LLD.md` — add `ResourceExhausted => libc::ENOSPC` arm to the status-to-errno mapping function.

**Full canonical mapping** (LLD must match):
| `tonic::Code` | `libc errno` |
|---|---|
| `Ok` | (success; no errno) |
| `Cancelled` | `EINTR` |
| `InvalidArgument` | `EINVAL` |
| `NotFound` | `ENOENT` |
| `AlreadyExists` | `EEXIST` |
| `PermissionDenied` | `EACCES` |
| `Unauthenticated` | `EACCES` (token mismatch surfaces as access denied) |
| `ResourceExhausted` | `ENOSPC` |
| `FailedPrecondition` | `EINVAL` (or `ESTALE` for stale-generation Close in Phase 6) |
| `Aborted` | `EIO` |
| `OutOfRange` | `EINVAL` |
| `Unimplemented` | `ENOSYS` |
| `Internal` | `EIO` |
| `Unavailable` | `EIO` |
| `DataLoss` | `EIO` |
| `DeadlineExceeded` | `EIO` (note: NOT `EAGAIN`; we don't retry transparently in v1) |
| `Unknown` | `EIO` |

---

### R6. `Meta.Close` request shape

**Chosen**: protocol.md is canonical.

**Canonical fields**:
```proto
message CloseReq {
  uint64 ino                = 1;
  uint64 fh                 = 2;
  uint64 expected_generation= 3;  // load-bearing: stale-close detection
  uint64 new_size           = 4;
  int64  new_mtime_s        = 5;
  uint32 new_mtime_ns       = 6;
  repeated uint64 written_block_idxs = 7;
}
message CloseResp {
  Attr attr = 1;   // includes the new generation; no separate field
}
```

**Rationale**: `expected_generation` is the load-bearing field that lets meta reject stale closes (the `gen-N` close arriving after some other client closed at `gen-N+1`). LLD's draft drops this field — that breaks the close-to-open invariant.

**Docs to update**:
- `LLD.md` — rename `dirty_block_idxs` → `written_block_idxs`. Add `expected_generation` field. Remove `allocated_blocks` from the Close request (allocation already happened in `AllocateBlocks`). Remove `new_generation` from CloseResp (it's already in `attr.generation`).

---

### R7. `BlockKey` carries generation; store DashMap keys it too

**Chosen**: store stores blocks keyed by `(ino, block_idx, generation)`, NOT just `(ino, block_idx)`.

**Rationale**: Two open generations of the same block can be live at the same time (writer-on-A still holding gen-N while gen-N+1 has been published). Storing them in different keys lets the store retain both until GC reaps the older one.

**Docs to update**:
- `LLD.md` — change `DashMap<(InodeId, BlockIdx), Bytes>` → `DashMap<BlockKey, Bytes>` where `BlockKey = (InodeId, BlockIdx, Generation)`.
- HLD invariant I3 already implies this; verify wording.

---

### R8. Store debug HTTP port

**Chosen**: `7300 + N` (separate from gRPC port `7200 + N`).

**Rationale**: The two services are separate processes within the store binary; co-locating them on the same port would require an HTTP/2-h2c → gRPC upgrade dance we don't need.

**Docs to update**:
- `acceptance-tests.md` — fix tests A-070, A-071, A-090, A-184, A-204 (and any others) to hit `:7300` not `:7200`. Remove or rewrite tests that hit non-existent meta debug endpoints (`/debug/inode`, `/debug/nodes`).
- `configuration.md` — verify `debug_http_listen` example uses `0.0.0.0:7300`.

---

### R9. Binary names

**Chosen**: `metasrv`, `storesrv`, `dtmpfs-mount`.

**Rationale**: Matches operations.md, the original plan, and the `[[bin]]` targets we'll declare in each crate's Cargo.toml.

**Docs to update**:
- `README.md` L10 — fix `dtmpfs-meta`/`dtmpfs-store` → `metasrv`/`storesrv` (L72-74 already correct).

---

### R10. Heartbeat numbers

**Chosen**: `heartbeat_interval_ms = 1000`, `heartbeat_timeout_ms = 5000`. Store declared Down after 5 missed (= 5 seconds since last heartbeat).

**Docs to update**:
- `testing.md` §6.1 — fix "5 s interval, 25 s timeout" to "1 s interval, 5 s timeout".

---

### R11. Acceptance-test FR numbering

**Chosen**: HLD §5's `F1..F12` is canonical.

**Rationale**: The "Coverage map" in acceptance-tests.md invented its own `F1..F30` with different meanings, breaking traceability.

**Docs to update**:
- `acceptance-tests.md` — rewrite the Coverage map table so `F1..Fn` row labels reference HLD §5's actual FR list. If acceptance-tests.md needs additional categories beyond HLD's FRs, use a different prefix (e.g., `T-PERF-1`, not `F13`).

---

### R12. Acceptance-test cluster_token length

**Chosen**: tests must use `cluster_token` ≥ 16 chars (matches the validation rule).

**Docs to update**:
- `acceptance-tests.md` — replace `"test-token"` (10 chars) with e.g. `"test-token-abcdef-1234"` (≥16) wherever it appears.

---

### R13. Replica-wait policy on flush vs fsync

**Chosen** (clarifies D3):
- `flush` (on `close(2)`): wait for **primaries only**.
- `fsync(2)`: wait for **all replicas** (when R≥2).

**Docs to update**:
- `LLD.md` §6.5 — currently waits all replicas via `try_join_all` for both paths. Split: `flush_path()` waits primaries; `fsync_path()` waits all.
- `consistency.md` §1.1 — verify it states the split.
- `architecture.md` §4.6 — same.

---

### R14. Two-writer overlapping-block loss must be documented in failure-model

**Chosen**: add a section.

**Rationale**: consistency.md describes it; failure-model.md does not. Operators need to know this is a documented limitation, not a bug.

**Docs to update**:
- `failure-model.md` — add a new subsection under §3 or §4 titled "Two writers, overlapping blocks (close-to-open lost-update)" with the worked example and the link to `consistency.md` for full semantics.

---

### R15. Missing runbooks in operations.md

**Chosen**: add runbooks for:
- Network partition (failure-model.md §3.4)
- Client-crash → ENOTCONN on mountpoint (failure-model.md §3.3)

**Docs to update**:
- `operations.md` §7 — add 2 runbooks; update the troubleshooting decision tree to include "is the mount returning ENOTCONN?" and "are heartbeats failing on a subset of nodes?".

---

### R16. Untested correctness invariants

**Chosen**: add acceptance tests for:
1. **Stale-generation Close**: client A opens at gen-N; client B opens at gen-N, writes, closes (gen-N+1); client A closes (with expected_generation=N) — must receive `FAILED_PRECONDITION` and the data must NOT regress.
2. **BlockCache invalidation on generation bump**: client A reads block 0 (cached at gen-N); some other writer publishes gen-N+1; client A reopens and reads block 0 — must miss cache and fetch fresh, must see new bytes.

**Docs to update**:
- `acceptance-tests.md` — add `AT-CONS-001` (stale-close rejection) and `AT-CONS-002` (BlockCache invalidation). Both are Phase-4 tests.

---

### R17. Config keys that don't exist in v1

**Chosen**: each questionable key gets one of three fates — **keep as v1**, **mark Phase 6+**, or **delete**.

| Key | Fate | Rationale |
|---|---|---|
| `gc_interval_ms` | **Phase 6+** | orphan sweep is Phase 6 work |
| `max_open_handles` | **v1, but soft cap with warn-log only** | trivial to implement; no enforcement panic in v1 |
| `tokio_worker_threads` | **v1** | one-line builder arg; trivial |
| `keepalive_interval_secs` | **v1** | tonic supports it natively |
| `rpc_timeout_ms` | **v1** | per-RPC deadline; default 5000 |
| `write_rpc_timeout_ms` | **v1** | per-RPC deadline override for write path; default 30000 |
| `[client.mount_options]` | **v1** | each subkey is a fuser MountOption mapping |

**Docs to update**:
- `configuration.md` — annotate each key with `(v1)` or `(Phase 6+)` tag in the section heading.
- `LLD.md` — add the v1-tagged fields to the `MetaConfig`/`StoreConfig`/`ClientConfig` structs. Phase-6 keys may be present in the struct but parsed-and-warned-if-set in v1.

---

### R18. Store readiness for fsync-with-replicas

(Implementation detail captured for completeness.)

The store's `WriteBlock` returns success once the block is in its DashMap. Replica writes are issued by the **client** in parallel to all R replicas — there is no store-to-store `Replicate` RPC in the v1 happy path. (The `Replicate` RPC in protocol.md is reserved for Phase 8 re-replication.) Update `protocol.md` to mark `Store.Replicate` as `// Phase 8+`.

---

## Summary of file-level changes the propagation agent will make

| File | # changes | Severity |
|---|---|---|
| `protocol.md` | ~3 (header rename, mark Replicate as Phase 8+, verify CloseReq fields) | Low |
| `HLD.md` | ~5 (header rename, perf targets, capacity_mb→bytes, etc.) | Med |
| `architecture.md` | ~3 (header rename, fsync clarification) | Low |
| `LLD.md` | ~10 (config field renames, Close fields, errno mapping, replica-wait split, BlockKey, stale-write phase) | High |
| `consistency.md` | ~2 (fsync rewrite, replica-wait clarification) | Med |
| `failure-model.md` | ~5 (R≥2 phase, re-replication phase, runbooks for partition/client, two-writer race section) | Med |
| `operations.md` | ~3 (2 new runbooks, decision-tree update) | Low |
| `configuration.md` | ~4 (phase tags on keys, debug_http_listen examples, capacity units) | Low |
| `testing.md` | ~2 (heartbeat numbers, perf targets cross-ref) | Low |
| `acceptance-tests.md` | ~10 (FR renumbering, ports, token length, two new AT-CONS tests, perf targets) | High |
| `README.md` | ~1 (binary names) | Trivial |

**Total**: ~48 edits across 11 files.

---

## Out of scope for this decision doc

- Anything not flagged by the audit (it's not contested).
- Any feature that's not in v1 or in the documented roadmap (would be a new design decision, not a reconciliation).
- Implementation detail beyond what's required to disambiguate doc statements (the LLD remains the source of truth for code-level details).
