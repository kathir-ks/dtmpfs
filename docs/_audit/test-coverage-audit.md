# dtmpfs — Test-vs-Requirement Traceability Audit

**Auditor**: Claude (read-only audit)
**Date**: 2026-04-29
**Scope**: HLD.md, protocol.md, failure-model.md, consistency.md, testing.md, acceptance-tests.md

This audit cross-checks the test artifacts (testing.md, acceptance-tests.md)
against the requirements stated in HLD.md, the RPC surface in protocol.md, and
the failure scenarios in failure-model.md.

---

## 1. Functional-requirement (FR) coverage matrix

HLD.md §5 enumerates F1..F12. Note: the FR numbering used by the existing
"Coverage map" in acceptance-tests.md (F1..F30) is a **different, ad-hoc
numbering** that does not match HLD.md §5. That mismatch is itself a finding
(see §9 CRITICAL-1). The matrix below is keyed by the **HLD §5 numbering**.

| FR (HLD §5) | Description | Covering test ID(s) | Gap? |
|---|---|---|---|
| F1 | Mountable on N hosts simultaneously, single namespace at ino=1 | A-001, A-005, A-050, A-055 | OK |
| F2 | POSIX ops: open, read, write, close, fsync, flush, release, mkdir, rmdir, unlink, rename, lookup, getattr, setattr (truncate/chmod/utimens), readdir, opendir, releasedir, mknod, create, statfs | A-010..A-018, A-021, A-022, A-024, A-030..A-038, A-205 (statfs); fsync/flush/release have **no explicit test**; utimens has **no explicit test**; mknod path has **no explicit test** | PARTIAL — see CRITICAL-2 |
| F3 | Close-to-open visibility (writes visible after close()/fsync() on any client) | A-050, A-051, A-054, A-055, A-134 | OK |
| F4 | Sharded by HRW on (ino, block_idx); ~1/N per node | A-070, A-071, A-072 | OK |
| F5 | Replication factor R ∈ {1,2,3}; R≥2 enables failover | A-090, A-091, A-092, A-094 (R=3 not exercised) | PARTIAL — R=3 untested (MINOR) |
| F6 | Meta supports Lookup, GetAttr, SetAttr, Create, Mkdir, Unlink, Rmdir, Rename, ReadDir, Open, Close, AllocateBlocks, HeartbeatNode, ListNodes | See RPC matrix §3 below | PARTIAL |
| F7 | Store supports ReadBlock, WriteBlock, DeleteBlock, Replicate, Stat | See RPC matrix §3 below | PARTIAL |
| F8 | Symlinks via `Inode.symlink_target`; `link` returns EPERM; xattrs return ENOSYS | A-159 (link/EPERM), A-160 (xattr ENOSYS); **symlink positive path: no automated A-test** (only manual checklist items 27, 28) | CRITICAL-3 — symlink read/create has no automated test |
| F9 | cluster_token on every RPC; mismatch → Status::unauthenticated | A-002, A-200 | OK |
| F10 | `GET /debug/blocks` on each store returns block list | A-070, A-071, A-072, A-073, A-090, A-091, A-094, A-154, A-184 (used as instrumentation) | OK (used implicitly; no dedicated test for the endpoint contract itself — MINOR) |
| F11 | Clean exit on SIGTERM/SIGINT via AutoUnmount; fusermount3 -u also supported | A-004 (SIGINT). **No SIGTERM-specific test, no `fusermount3 -u` test** | MINOR — SIGTERM/external unmount uncovered |
| F12 | Inode allocation monotonic from next_ino=2; no reuse | A-025 (covers no-reuse on delete-recreate) | OK |

**HLD §5 FR numbers explicitly without a primary test:**

- **fsync** as a named op (F2): only implicit via `conv=fsync` in A-093 / A-180 / A-181. There is **no acceptance test asserting fsync on a fd makes the write cross-host visible without close** — which is half of the stated F3 contract.
- **utimens** (F2): zero coverage.
- **mknod** for regular files (F2, used by `touch`): A-010 covers `touch` end-to-end but no test confirms mknod-vs-create distinction.
- **statfs** (F2): A-205 covers it.
- **opendir/releasedir** (F2): implicit in `ls` tests (A-035, A-036) but no explicit assertion of cookie/EOF behaviour from `Meta.ReadDir`.

---

## 2. Non-functional-requirement (NFR) coverage matrix

HLD.md §6 / §6.1 enumerate NFRs. Targets are tabulated as the v1 acceptance bar.

| NFR | HLD target | Covering test | Gap? |
|---|---|---|---|
| NFR-perf-1 (LAN) | Sequential read 1 GiB / 1 MiB block ≥ 800 MiB/s on 10 GbE LAN | **None** — A-181 only asserts ≥ 200 MB/s cold and ≥ 300 MB/s cached on **localhost** | CRITICAL-4 — no LAN-perf test |
| NFR-perf-1 (loopback) | ≥ 3 GiB/s loopback read | A-181 asserts ≥ 300 MB/s only — **10× weaker than HLD target** | CRITICAL-4 |
| NFR-perf-1 (write loopback) | ≥ 1.5 GiB/s loopback seq write | A-180 asserts ≥ 200 MB/s — **7.5× weaker than HLD target** | CRITICAL-4 |
| NFR-perf-2 | getattr/lookup/mkdir p99 < 1 ms loopback / < 2 ms LAN | **None** — A-182/A-183 measure data-path p50 only; no metadata-op p99 test | CRITICAL-5 |
| NFR-perf-3 | close() flushing K dirty blocks completes in O(K/parallelism); buffer_unordered(16) | **No test** asserts parallel-flush behaviour or measures fan-out | MINOR |
| NFR-scale-1 | 2-8 nodes; meta handles low-thousands metadata ops/sec before contention | **No test** — A-158 creates 10k files but does not measure ops/sec or test 8-node topology | MINOR |
| NFR-scale-2 | Per-store memory bounded by store.capacity_mb; over-cap → ENOSPC | A-184 (fills to 80% no OOM); **no test for over-capacity → ENOSPC propagation** | CRITICAL-6 — the "exceeding returns Status::resource_exhausted" path is not asserted |
| NFR-avail-1 | CP under partition; only meta-side makes progress | A-115 | OK |
| NFR-avail-2 | R=1 store loss → EIO; R≥2 → transparent failover | A-110 (R=1 EIO), A-092 (R=2 failover) | OK |
| NFR-avail-3 | Loss of meta kills FS until restart | A-112, A-113 | OK |
| NFR-sec | Trusted-LAN; cluster token defends only against accidental cross-cluster traffic | A-002, A-200 (token check). **No test for the constant-time-comparison property**, which is fine — that's a code-level concern. | OK |

### Performance-targets sub-table (HLD §6.1)

| Metric | HLD bar (LAN) | HLD bar (loopback) | Tested at |
|---|---|---|---|
| Seq read 1 GiB/1 MiB | ≥ 800 MiB/s | ≥ 3 GiB/s | A-181 (200 MB/s cold; not LAN) |
| Seq write+close 1 GiB | ≥ 600 MiB/s | ≥ 1.5 GiB/s | A-180 (≥ 200 MB/s only) |
| getattr p99 | < 2 ms | < 1 ms | none |
| lookup p99 | < 2 ms | < 1 ms | none |
| mkdir p99 | < 3 ms | < 2 ms | none |
| open 1 MiB cold p99 | < 5 ms | < 3 ms | none |
| close (no dirty) p99 | < 2 ms | < 1 ms | none |
| close (1 MiB dirty) p99 | < 10 ms | < 5 ms | none |

**Verdict:** v1 perf bars in HLD §6.1 are largely unbacked by tests. testing.md §4.7 explicitly says "Throughput and latency floors. Not regression-tracked in v1; we just want a 'sanity' line: localhost write ≥ 200 MB/s, read ≥ 300 MB/s cached." So the test strategy *intentionally* relaxes the bar, but the relaxation is not declared in HLD.md, creating an internal contradiction (CRITICAL-4).

---

## 3. RPC coverage matrix

Cross-references protocol.md §3. Success path = at least one test that exercises a happy-path call; error path = at least one test that asserts an error code.

### 3.1 Meta service

| RPC | Success test | Error test | Gap |
|---|---|---|---|
| `Meta.Lookup` | implicit in any path resolution (A-011, A-012, A-035, A-053) | A-053 (NOT_FOUND after unlink) | OK |
| `Meta.GetAttr` | A-010 (`stat`), A-021 (`stat -c '%a'`), A-023 | A-024 (post-rm `ls` → ENOENT) | OK |
| `Meta.SetAttr` | A-017 (truncate=0), A-018 (truncate grow), A-021 (chmod) | A-022 (chown to other user → EPERM) — **only via comment, not asserted** | MINOR — EPERM on cross-uid chown is not actually triggered |
| `Meta.Create` | A-011, A-019 | A-024 follow-up: ALREADY_EXISTS not tested | CRITICAL-7 — `Status::already_exists` for Create collision has no test |
| `Meta.Mkdir` | A-030, A-031 | A-033 covers ENOTEMPTY on rmdir; **mkdir-collision (ALREADY_EXISTS) has no test** | CRITICAL-7 |
| `Meta.Unlink` | A-024, A-034 | A-024 step 4 covers ENOENT post-delete; **idempotent-second-call test (treats NOT_FOUND as success per protocol.md §3.1) has no test** | MINOR |
| `Meta.Rmdir` | A-032 | A-033 (FAILED_PRECONDITION "dir not empty") | OK |
| `Meta.Rename` | A-037, A-038, A-054 | **No test for "rename onto non-empty dir → FAILED_PRECONDITION"** | CRITICAL-8 |
| `Meta.ReadDir` | A-035, A-036, A-052 | **No test exercises the cookie/pagination contract** (max_entries, next_cookie, eof) | MINOR |
| `Meta.Open` | A-019, A-050 (implicit) | **No test for Open of unknown ino → NOT_FOUND, or O_TRUNC behaviour explicitly via Open** | MINOR |
| `Meta.Close` | A-019, A-050 (implicit) | **No test for stale `expected_generation` → FAILED_PRECONDITION** — this is the canonical race in protocol.md §8.5 / §5.5 | CRITICAL-9 |
| `Meta.AllocateBlocks` | implicit in any large-file write (A-013, A-018) | none | MINOR — implicit only |
| `Meta.HeartbeatNode` | A-204 | none (e.g., bad token from a store → UNAUTHENTICATED) | MINOR |
| `Meta.ListNodes` | A-204 (implicit via /debug/nodes) | A-201 (asserts startup ListNodes failure path) | OK |

### 3.2 Store service

| RPC | Success test | Error test | Gap |
|---|---|---|---|
| `Store.ReadBlock` | A-013, A-014, A-092, A-094 | A-110 (UNAVAILABLE-equivalent: store dead → EIO) | OK |
| `Store.WriteBlock` | A-013, A-090 | A-184 only fills to 80%; **no test for exceeding ram_budget → RESOURCE_EXHAUSTED → ENOSPC** (also flagged as NFR-scale-2 gap) | CRITICAL-6 |
| `Store.DeleteBlock` | A-017, A-154 (implicit via truncate-frees-blocks) | none — idempotent-second-call NOT_FOUND not asserted | MINOR |
| `Store.Replicate` | A-090, A-091, A-094 (implicit) | **No test for the `Replicate` RPC's pull behaviour itself** (e.g., source unreachable) | MINOR |
| `Store.Stat` | A-184 (`/debug/stat`) — but that's the HTTP debug endpoint, **not the gRPC `Store.Stat`** | none | CRITICAL-10 — `Store.Stat` gRPC has no automated test |

### 3.3 Wire/transport-level checks

protocol.md §1.5 lists per-RPC deadlines, §1.7 declares no compression in v1, §2.2.1 says oversized `WriteBlockReq.data` returns INVALID_ARGUMENT, §5.2 sets 8 MiB max message size.

- **No test asserts `INVALID_ARGUMENT` on oversize WriteBlock** — CRITICAL-11.
- **No test asserts deadline propagation / DEADLINE_EXCEEDED behaviour** — MINOR.
- **No test asserts the 8 MiB max-message-size ceiling** — MINOR.
- **No test asserts the keepalive / connect-timeout behaviour from §6.2** — MINOR.

### 3.4 Stale-generation rejection (protocol.md §8.5)

- `Meta.Close` with stale `expected_generation` → FAILED_PRECONDITION: **untested** (CRITICAL-9 above).
- `Store.WriteBlock` at stale generation → FAILED_PRECONDITION: **untested** — failure-model.md §7.3 explicitly notes this is Phase 6 work, but acceptance-tests.md has no Phase 6 test for it. CRITICAL-12.

---

## 4. Failure-mode coverage matrix

failure-model.md §3 (single-fault) and §4 (multi-fault) scenarios:

| failure-model.md scenario | § | Covering acceptance test | Gap |
|---|---|---|---|
| Single store crashes (R=1) | §3.1 | A-110 (kill, EIO), A-111 (restart, still EIO) | OK |
| Single store crashes (R=2) failover | §3.1 + §3.5 | A-092 | OK |
| Meta crashes | §3.2 | A-112 (every op EIO), A-113 (restart empty) | OK |
| Client (FUSE mount) crashes | §3.3 | A-114 (kill -9 mid-write), partial; **dirty-buffer-loss observed from another mount is asserted, but ENOTCONN → unmount → remount transition is not explicitly tested** | MINOR |
| Network partition (clean split) | §3.4 | A-115 (iptables) | OK |
| Two stores die with R=2 | §4.1 | **None** | CRITICAL-13 |
| Meta + a store die | §4.2 | **None** (ordering matters per failure-model.md but no test) | MINOR |
| Network partition during write | §4.3 | A-115 partially; **the "retry fsync on healed partition succeeds" path is not tested** | MINOR |
| Cascading store deaths | §4.4 | **None** | MINOR |
| Store RAM budget hit (5.1) | §5.1 | A-184 fills to 80%; **the actual ENOSPC path past 100% is missing** | CRITICAL-6 (dup) |
| FD limits (5.2) | §5.2 | **None** | MINOR |
| FUSE kernel queue full (5.3) | §5.3 | **None** | MINOR |
| Inode count OOM (5.4) | §5.4 | A-158 stops at 10k (well below the 200M soft limit); **no test that asserts the meta gracefully OOMs vs panicking** | MINOR |
| Operator: wrong cluster_token (6.1) | §6.1 | A-002, A-200 | OK |
| Operator: wrong meta_addr (6.2) | §6.2 | A-201 | OK |
| Operator: port collision (6.3) | §6.3 | A-202 | OK |
| Operator: duplicate node_id (6.4) | §6.4 | A-203 | OK |
| Operator: double-mount same path (6.5) | §6.5 | **None** | MINOR |
| Wire corruption (§7.1) | §7.1 | None — explicitly accepted as out-of-scope in v1 (testing.md §10) | OK by design |
| Stale-write reuse (§7.3) | §7.3 | **None** for Phase 6 stale-rejection path | CRITICAL-12 (dup) |

---

## 5. Consistency-model canonical tests

The audit prompt enumerates five canonical close-to-open tests. Verifying each:

| Canonical test | Acceptance test | Verdict |
|---|---|---|
| Cross-host visibility after close (write A, read B) | A-050, A-051, A-054, A-055 | PRESENT |
| AttrCache TTL: read same file twice within 1 s vs after 1 s | A-053 (delete + stale-stat-within-TTL + ENOENT after TTL) | PRESENT (covers TTL boundary; does **not** explicitly test "two reads within TTL hit cache vs go to meta" — MINOR) |
| BlockCache invalidation on generation bump | **No direct test.** A-134 implicitly relies on it ("if B sees gen N, every block at gen N"), but there is no test that opens at gen N, observes a bump, and asserts old-gen cache entries are no longer served | CRITICAL-14 |
| Two-writer race: lost-update / last-close-wins | A-133 | PRESENT |
| Open at gen N, write, another open at gen N+1, the gen-N close should not regress (the canonical race in protocol.md §8.5 / consistency.md §5.5) | **No test.** Neither acceptance-tests.md nor any system test exercises this exact scenario. | CRITICAL-9 (dup) |

---

## 6. Test-strategy / acceptance-tests internal consistency

### 6.1 Strategy → tests

testing.md describes:

- **Unit (§1.1)** — covered as per-crate `#[cfg(test)]`. Out of scope for this audit; not directly mapped in acceptance-tests.md and that's correct.
- **Integration (§1.2)** — `TestCluster` helper. acceptance-tests.md is shell-level, doesn't reflect this; that's fine, integration tests live in code.
- **System (§1.3)** — `tests/smoke.sh`, `tests/multi_host.sh`, `tests/sharding.sh`, `tests/chaos.sh`. acceptance-tests.md does **not** explicitly map any A-test to these scripts. MINOR.
- **Acceptance (§1.4)** — testing.md says "at least 50 numbered cases (`A-001` through ~`A-205`)". Actual count: **66 tests** (5 mount + 16 file ops + 9 dir ops + 6 cross-host + 4 sharding + 5 replication + 6 failure + 5 concurrency + 12 edge/POSIX + 5 perf + 6 config/ops). Exceeds the floor.
- **Soak / chaos (§1.3, §4.6, §5)** — testing.md says `tests/chaos.sh` runs 10 minutes and Phase 6 ships when chaos passes. acceptance-tests.md has **no A-test** representing the chaos soak. CRITICAL-15.

### 6.2 Test ID uniqueness

Verified: A-001 .. A-205 are unique. No collisions. (A-200 references A-002 by content — that's a deliberate alias note.)

### 6.3 Per-test prerequisites

Spot-checked 20+ tests: every test has a `Preconditions` block. Some inherit "std-cluster" implicitly. OK.

### 6.4 Pass/fail objectivity

Most tests have concrete criteria (exact stdout, exact exit code, specific md5). Vague-criterion offenders:

- **A-203**: "Behaviour undefined v1" — accepts "operators expected to use unique `node_id`" but the test passes if a warning is logged. The "log line exists" criterion is fine; the wording is loose. COSMETIC.
- **A-093**: "(a) success **or** (b) EIO". Disjunctive pass criteria are acceptable for documenting Phase-5 vs Phase-6 transition, but the test will pass for any of two very different observed behaviours, which makes regressions hard to detect. MINOR.
- **A-114**: "file is either missing or present with size 0" — disjunctive; same caveat. MINOR.
- **A-161**: "Do not rely on this for files larger than a few MiB" — pass criterion is unambiguous (small mmap msync visible) but the asterisk in Notes effectively narrows the supported scope without bounding it. COSMETIC.
- **A-182, A-183**: rely on `fio` JSON parsing inline; the criterion is objective (numeric threshold) but the test depends on a specific fio JSON shape that is not version-pinned. MINOR.

No test uses pure "should work / no errors" wording.

### 6.5 Testing.md says "10-minute soak"

testing.md §1.3 says `tests/chaos.sh` is 10 min, random store kills every 30 s. There is **no acceptance test** in acceptance-tests.md corresponding to a soak. CRITICAL-15.

---

## 7. Tooling consistency

testing.md §2 specifies tooling: `cargo test`, `bash tests/*.sh`, `proptest`, `cargo-fuzz`, `cargo llvm-cov`, ThreadSanitizer, loom, fio, `pjdfstest`.

acceptance-tests.md uses: shell, `dd`, `cat`, `md5sum`, `python3`, `curl`, `fio` (A-182/183), `xxd`, `find`, `ls`, `stat`, `truncate`, `getfattr`, iptables (A-115), `mountpoint`, `fusermount3`, `RUST_LOG`, the `dtmpfs-mount`/`metasrv`/`storesrv` binaries.

Mismatches:

- **`pjdfstest`** is mentioned in testing.md §4.8 with a list of subdirs (open, mkdir, rmdir, rename, unlink, chmod, truncate). acceptance-tests.md only references `pjdfstest` once, in the manual-exploratory checklist item 29. **No automated A-test invokes pjdfstest.** MINOR.
- **`bonnie++`** is in the manual checklist (item 30) but not declared in testing.md. COSMETIC.
- **`stress-ng`** ditto (item 23). COSMETIC.
- **`grpcurl`** (mentioned in protocol.md §5.3 for reflection) is **not** used in any A-test. There is no test that exercises gRPC reflection. MINOR.

---

## 8. Phase alignment

testing.md §5 has a phase-by-phase DoD table. Cross-checking acceptance-tests.md phase tags:

| Phase | testing.md DoD | A-tests carrying that Phase tag |
|---|---|---|
| P1 | smoke.sh local mount | A-001..A-005, A-010..A-022, A-024, A-030..A-038, A-130, A-132, A-150..A-161, A-180..A-184, A-200..A-202 |
| P2 | smoke + bytes-in-store assertion | **No A-test tagged P2** — gap, CRITICAL-16 |
| P3 | sharding.sh, 256 blocks across 4, ≤ 20% spread | A-070, A-071, A-072, A-073, A-205 |
| P4 | cross-host visibility (A-050) | A-023, A-025, A-050..A-055, A-131, A-133, A-134 |
| P5 | kill-one-store with R=2 (A-092) | A-090..A-094 |
| P6 | chaos.sh 10-min, random kills | A-110..A-115, A-203, A-204 — but no soak/chaos A-test (CRITICAL-15 dup) |
| P7 | (stretch) Raft on 3-meta cluster | None — acceptable, P7 is stretch in HLD §11 |

**Phase mismatches:**

- A-070..A-073 (Phase 3 sharding) correctly require ≥ 2 stores; not multi-store-meta-sharded. OK.
- A-053 is tagged P4 (cross-host) but reads as a single-mount-with-AttrCache test. Re-reading: it uses two clients (`/mnt/dtmpfs` and `/mnt/dtmpfs-b`). Correctly P4.
- A-092 is tagged P5 (replication R≥2) and A-110 P6 (kill-with-R=1). The R=1-EIO behaviour is observable from P2 onward. Tagging it P6 means it gates with chaos. Defensible but conservative. COSMETIC.
- A-184 is tagged P1, but per-store ram_budget enforcement that returns ENOSPC is a feature that arguably stabilizes around P2 (when storesrv is its own process). A-184 only checks the 80% non-OOM, not the ENOSPC propagation, so the P1 tag is fine. OK.

---

## 9. Findings

### CRITICAL

- **CRITICAL-1**: The "Coverage map" at the end of acceptance-tests.md (F1..F30) is keyed on a numbering that does **not** match HLD.md §5 (F1..F12). Reviewers cross-referencing HLD F-numbers will look at the wrong tests. The acceptance-tests.md table reuses F1..F12 with **different meanings**, e.g. its F5 ("Files of arbitrary size") is HLD F5's "Replication factor configurable". This is a documentation bug that will mis-direct PR reviewers. Renumber acceptance-tests.md's table to the HLD scheme, or rename the columns (e.g. "AT-CR-1...").
- **CRITICAL-2**: HLD F2 explicitly lists `fsync`, `flush`, `release`, `mknod`, `utimens` as POSIX ops to test. There are no dedicated acceptance tests for any of these. `fsync`-as-cross-host-barrier (a load-bearing claim for F3 and consistency.md §1.1) is **never tested without close()** — every cross-host test uses `sync` (the kernel sync) and close, not `fsync(fd)`.
- **CRITICAL-3**: Symlink **success** path (create / readlink / cross-mount visibility) has no automated A-test. HLD F8 commits to symlinks; only manual checklist items 27-28 cover them. The negative-path tests for `link` (A-159) and `xattr` (A-160) exist; the symlink positive case is missing.
- **CRITICAL-4**: HLD §6.1 declares quantitative perf targets (loopback ≥ 3 GiB/s read, LAN ≥ 800 MiB/s, etc.). Acceptance tests (A-180..A-184) target an order of magnitude lower (200–300 MB/s). testing.md §4.7 acknowledges this gap explicitly ("we just want a 'sanity' line") but HLD §6.1 does not. Either lower the HLD targets to match the actual bar, or add tests at the HLD bar with `#[ignore]` until perf work lands.
- **CRITICAL-5**: HLD §6.1 has p99 metadata-op targets (getattr/lookup/mkdir/open/close p99 < 1–10 ms). **Zero tests** measure metadata-op p99 latency. A-182/A-183 measure data-path p50 only.
- **CRITICAL-6**: NFR-scale-2 ("over-cap → Status::resource_exhausted → ENOSPC") and failure-model.md §5.1 both promise this path. A-184 stops at 80% of capacity. The 100%+ ENOSPC propagation through the stack is **untested**.
- **CRITICAL-7**: `Meta.Create` and `Meta.Mkdir` collision → ALREADY_EXISTS (protocol.md §3.1, §4) is not tested. The error-code mapping has zero coverage.
- **CRITICAL-8**: `Meta.Rename` onto a non-empty target dir → FAILED_PRECONDITION (protocol.md §4) is not tested. Rename of a regular file onto an existing regular file (which protocol.md §10.6 says unlinks the target first) is also untested.
- **CRITICAL-9**: The canonical close-to-open race — Open at gen N, another writer publishes gen N+1, the gen-N writer's `Meta.Close{expected_generation:7}` returns FAILED_PRECONDITION (protocol.md §8.5, consistency.md §5.5) — has **no acceptance test**. This is the single most load-bearing correctness invariant of close-to-open and there is no test for it.
- **CRITICAL-10**: `Store.Stat` gRPC is not exercised by any A-test. A-184 uses the HTTP `/debug/stat` endpoint (different surface). The gRPC method is required by F7.
- **CRITICAL-11**: Oversized `WriteBlockReq.data` → INVALID_ARGUMENT (protocol.md §2.2.1, §4) is not tested. Easy to add: `dd bs=2M count=1` would not actually exercise it because the client splits to block_size; needs a direct gRPC client.
- **CRITICAL-12**: Stale-generation `Store.WriteBlock` rejection (protocol.md §8.5, failure-model.md §7.3) — Phase-6 work — has no test slot reserved in acceptance-tests.md. Even a placeholder/`#[ignore]`-style entry would help.
- **CRITICAL-13**: failure-model.md §4.1 ("two stores die with R=2, ~17% of bytes lost") has no acceptance test. The whole multi-fault matrix (§4.1, §4.2, §4.4) is uncovered.
- **CRITICAL-14**: BlockCache invalidation on generation bump is the close-to-open invalidation mechanism (consistency.md §1.3, HLD I4). No test asserts that an old-gen cache entry is unreachable after a bump. A-134 says "if B sees gen N, every block at gen N" but does not exercise the bump path explicitly.
- **CRITICAL-15**: testing.md §1.3 + §5 commit `tests/chaos.sh` as the P6 DoD (10-minute soak with random store kills). acceptance-tests.md has no chaos/soak entry. The Phase-6 release gate is invisible to readers of acceptance-tests.md.
- **CRITICAL-16**: testing.md §5 says P2 DoD is "smoke.sh passes; integration test asserts bytes are present in store process memory". No A-test is tagged P2 and no test asserts "bytes visible in store" (the `/debug/blocks` endpoint, which would be the obvious vehicle, is used in A-070+ which are P3 tests). Either move one of the basic write tests to assert P2-level guarantees, or document why P2 has no A-test.

### MINOR

- R=3 replication factor is supported in HLD F5 but no test exercises R=3 (only R=1 and R=2).
- `MountOption::AutoUnmount` is mentioned in F11 and tested via SIGINT (A-004), but the `fusermount3 -u` external-unmount path is documented and untested.
- `Meta.ReadDir` cookie/pagination contract (`max_entries`, `next_cookie`, `eof`) is not tested. A-158 creates 10k files and `ls` works, which exercises the loop indirectly, but no test asserts cookie semantics.
- `Meta.AllocateBlocks` is exercised only implicitly by large writes; no test confirms its idempotency property (protocol.md §3.1: "Retrying AllocateBlocks is fine if the client uses the same inode and the same block indices").
- `Store.DeleteBlock` idempotent-second-call NOT_FOUND swallow (protocol.md §3.1) is not asserted.
- `Store.Replicate` source-unreachable error path is untested.
- gRPC keepalive / connect-timeout / DEADLINE_EXCEEDED behaviours (protocol.md §6) have no tests.
- 8 MiB `max_decoding_message_size` ceiling (protocol.md §5.2) is untested.
- Cross-uid `chown` → EPERM (acceptance-tests.md A-022 Notes) is described but the test (A-022) only does chown-to-self, not the EPERM path.
- failure-model.md §6.5 (double-mount on same path → EBUSY) is not in acceptance-tests.md.
- failure-model.md §5.2 (FD limits) and §5.3 (FUSE kernel queue) have no tests.
- failure-model.md §4.3 (network partition during write, retry on heal) has no targeted test.
- Manual-checklist items 23 (`stress-ng`), 30 (`bonnie++`) reference tools not declared in testing.md §2.
- A-093 and A-114 use disjunctive pass criteria (success OR clean failure), reducing regression-detection power.
- A-182/A-183 inline-parse `fio` JSON without pinning fio's output schema.
- testing.md §4.8 lists pjdfstest subdirectories as in-scope for v1 but no automated A-test runs them.
- HLD §13 (Open Questions) OQ-7 ("flush_parallelism configurable") and OQ-8 ("RPC deadline defaults") have no tests gating their resolution; acceptable for "open questions" but worth flagging as the design decisions land.

### COSMETIC

- A-200 is a near-duplicate of A-002 (the cross-reference is acknowledged in Notes). Could be consolidated or made into a pure pointer.
- A-203 ("Behaviour undefined v1") has a soft pass criterion. Acceptable but loose.
- testing.md §2.5 spec says property tests live alongside unit tests filtered with `cargo test --workspace --lib prop_`; no acceptance-tests.md mapping. Fine — unit/proptest is out of scope here.
- The acceptance-tests.md table at the bottom uses F1..F30 which collides numerically with HLD §5's F1..F12 even after CRITICAL-1 is addressed; consider an `AT-` or `CR-` prefix to disambiguate.
- Phase tagging: A-110 is P6; reading it, R=1 EIO is observable from P2. Choosing P6 conservatively bundles it with chaos but obscures earlier coverage.

---

## 10. Summary scoreboard

| Coverage area | Pass | Partial | Gap |
|---|---|---|---|
| HLD §5 functional requirements (F1-F12) | F1, F3, F4, F9, F10, F12 | F2, F5, F6, F7, F8, F11 | (none entirely uncovered) |
| HLD §6 NFRs | NFR-avail-1/2/3 | NFR-perf-3, NFR-scale-1, NFR-sec | NFR-perf-1, NFR-perf-2, NFR-scale-2 |
| protocol.md RPCs (15 meta + 5 store = 20) | 13 OK | 5 partial | 2 untested (Store.Stat gRPC; oversized-WriteBlock INVALID_ARGUMENT) |
| failure-model.md §3 single-fault (5 scenarios) | 5/5 | 0 | 0 |
| failure-model.md §4 multi-fault (4 scenarios) | 1/4 (partition during write partial) | 1/4 | 2/4 untested |
| Consistency canonical tests (5) | 3/5 | 1/5 | 1/5 (gen-N close after gen-N+1 publish) |
| Phase DoD coverage (P1-P6) | P1, P3, P4, P5 | P6 (no soak A-test) | P2 (no A-test at all) |

End of audit.
