# Failure-Model & Error-Handling Consistency Audit

Audit of `failure-model.md` (canonical) cross-checked against `HLD.md`,
`LLD.md`, `protocol.md`, `consistency.md`, `operations.md`, and
`configuration.md`.

## Summary
- **6 CRITICAL** contradictions (semantic disagreement that would change behavior or surface a bug)
- **7 MINOR** inconsistencies (naming/scope drift, mostly recoverable)
- **4 COSMETIC** issues (wording/cross-ref nits)

---

## CRITICAL (semantic disagreement that would change behavior)

### C1. R≥2 read fallback: `failure-model.md` says Phase 6, every other doc says v1.

**Files**: `failure-model.md` §3.1 (L107–110, L125–126), §3.4 (L286), §9.1 step 6 (L632–634) vs `HLD.md` §F5 (L111), §NFR-avail-2 (L128), §11 Phase 5 row (L518) vs `LLD.md` §8 (L1796–1805) vs `architecture.md` §3.6 (L361) vs `consistency.md` §1.1 (L26–28).

**Issue**: `failure-model.md` claims the v1 client only contacts the primary and that replica failover is a Phase-6 feature:
> "**R=2 / R=3:** another copy exists, but **in v1 the client only contacts the primary**, so a primary death looks the same as R=1 until Phase 6 ships replica-failover on read."
> "Client does **not** retry against a replica (Phase 6 adds this)."
> "If R≥2 with Phase 6 client, reads should now succeed"

Every other doc states the opposite — read failover ships in v1 with R≥2:

- `HLD.md` F5 (L111): "With `R >= 2`, a read can fall back from primary to a replica on transport failure to the primary."
- `HLD.md` NFR-avail-2 (L128): "Loss of a single store with `R=1` causes EIO ... ; with `R>=2` reads transparently fail over."
- `HLD.md` §11 Phase 5 acceptance (L518): "Kill one store with R=2, reads succeed".
- `LLD.md` §8 (L1796–1805): the unavailable-arm explicitly does `// try next replica (R>=2)` and `continue`.
- `architecture.md` §3.6 (L361): "The retry uses the next replica from `block_map[k].replicas`".

This is a direct contradiction of a load-bearing availability claim. Either v1 has R≥2 read failover (and `failure-model.md` overstates the limitation) or it does not (and the HLD/LLD/architecture promises are vacuous). The user-facing prompt for this audit lists "R≥2: transparent fallback to replica" as the expected v1 behavior, so the failure-model wording is the outlier.

**Recommended fix**: Remove the "Phase 6 adds replica-failover" hedges from `failure-model.md` §3.1, §3.4 blast-radius, and §9.1 Step 6. The remaining v1 limitation to call out is that **re-replication of the lost copy** is not in v1 — read failover *is*.

---

### C2. `Status::resource_exhausted` → `ENOSPC` is promised everywhere except in the actual errno mapping in LLD.md.

**Files**: `failure-model.md` §5.1 (L361–369), §9.5 (L728–731), `operations.md` §7 (L540), `architecture.md` §6.4 (L642), `HLD.md` §6.2 (L151), `protocol.md` §4 (L631) vs `LLD.md` §1 (L411–429), §8 (L1815–1833).

**Issue**: Five docs document that a store hitting its RAM budget surfaces to userspace as `errno == ENOSPC`:

- `HLD.md` §6.2 (L151): "Exceeding this returns `Status::resource_exhausted` on `WriteBlock`; the client surfaces `ENOSPC`."
- `failure-model.md` §5.1: "`flush`/`close` returns -1, `errno == ENOSPC`."
- `operations.md` §7: "write returns 0 (success) but close returns ENOSPC".

`LLD.md`'s `DtmpfsError` enum has **no** variant for resource-exhausted, and its `From<DtmpfsError> for libc::c_int` mapping (reproduced twice — §1 L411 and §8 L1815) only enumerates `NotFound/AlreadyExists/NotADirectory/IsADirectory/NotEmpty/PermissionDenied` and routes everything else (including `Rpc(_)`, which is what `Status::resource_exhausted` becomes via `from_status`) to `libc::EIO`.

Tracing protocol.md → LLD.md:
- `from_status` (LLD L435–448) does not match `Code::ResourceExhausted`, so it falls through to `_ => DtmpfsError::Rpc(s)`.
- `Rpc(_) => libc::EIO` (LLD L426, L1830).

As written, the user will see `EIO`, not `ENOSPC`. The test-coverage audit (`_audit/test-coverage-audit.md` CRITICAL-6) already flags that no test exercises the past-100% path; this audit explains *why* that test would fail if it existed.

**Recommended fix**: Add a `DtmpfsError::ResourceExhausted` variant in `LLD.md`, match `Code::ResourceExhausted` in `from_status`, and map it to `libc::ENOSPC` in the `From<DtmpfsError> for libc::c_int` (in BOTH §1 and §8 copies of the mapping).

---

### C3. Re-replication: `failure-model.md` says Phase 6, `HLD.md` §12 says Phase 8.

**Files**: `failure-model.md` §3.1 (L126), §10 (L794), runbook 9.1 step 6 (L633), Manual recovery (L146) vs `HLD.md` §11 Phase 6 row (L519) vs `HLD.md` §12 (L555).

**Issue**:
- `failure-model.md` §10 (L794): "Roadmap, in order: Phase 6 background re-replication; Phase 7 Raft for meta..."
- `failure-model.md` §3.1 manual recovery: "R≥2: Phase 6 will re-replicate".
- `HLD.md` §11 phase table Phase 6 (L519): "Heartbeats, stale-write rejection, GC, retries" — no re-replication listed.
- `HLD.md` §12 (L555): "**No re-replication after a store dies.** With `R=1` the data is gone; with `R>=2` reads continue but the lost replica is not regenerated. **Phase 8 stretch.**"

So failure-model.md hard-codes "Phase 6 re-replicates" as a roadmap promise; HLD.md §12 explicitly defers this to **Phase 8 stretch**. The acceptance bar for Phase 6 in the HLD does not mention re-replication. Pick one.

**Recommended fix**: Align failure-model.md with HLD.md §12: re-replication is **Phase 8 stretch**, not Phase 6. Update §3.1 manual recovery, §3.1 automated recovery, §9.1 Step 6, and §10 roadmap accordingly.

---

### C4. Orphan-block GC after meta restart: `failure-model.md` says Phase-6 GC sweep cleans them up; `HLD.md` §12 says they survive until cluster restart.

**Files**: `failure-model.md` §3.2 (L204–205), §4.2 (L329–330), §5.6 (L506) vs `HLD.md` §12 (L556).

**Issue**:
- `failure-model.md` §3.2 (L204–205): "The stores still hold blocks — those blocks are now orphans (no inode references them). **Phase 6 GC will sweep them**".
- `failure-model.md` §4.2: "The orphans on surviving stores are reclaimed on the next store restart (or by Phase-6 GC)."
- `consistency.md` §5.6 (L506): "The orphaned blocks are reclaimed by **nightly GC** (Phase 6)".
- `HLD.md` §12 (L556): "**No background block GC for orphaned blocks.** Phase 6 implements **eager delete on `unlink`**; orphaned blocks (e.g. from a client that crashed mid-flush) are tolerated **until cluster restart**."
- `configuration.md` §3.2 (L134–137): describes `gc_interval_ms` as "consumed by Phase-6 GC" but says "v1 parses and ignores".

So `HLD.md` §12 explicitly carves out the "Phase 6 = eager unlink delete only, **not** orphan GC" position; failure-model.md and consistency.md promise orphan-sweeping in Phase 6. configuration.md splits the difference (the knob exists but is no-op in v1, with Phase 6 wiring it). These three positions are not all simultaneously true.

**Recommended fix**: Either upgrade `HLD.md` §12 to acknowledge that Phase 6 includes a periodic orphan sweep, or downgrade `failure-model.md` §3.2 / §4.2 / §5.6 (and `consistency.md` §5.6) to match HLD: Phase 6 = eager unlink delete; orphan reclamation requires a store restart in v1 and is post-v1 (likely a Phase 6.5 or Phase 8 item alongside re-replication).

---

### C5. `LLD.md` config struct uses `heartbeat_dead_ms` while every other doc uses `heartbeat_timeout_ms`.

**Files**: `LLD.md` §2 (L478–479, L514, L1997) vs `configuration.md` §3.1 (L114–125), `failure-model.md` §3.1 (L97–101), §3.4 (L279), §8.1 (L553), `operations.md` §3.1 (L138), `protocol.md` §6.4 (L765), `HLD.md` §I8 (L507).

**Issue**: The TOML key is `heartbeat_timeout_ms` per `configuration.md` (and the example TOML in `operations.md`). `LLD.md` defines the Rust struct field as `heartbeat_dead_ms` with `serde(default = "d_heartbeat_dead_ms")` — there is no `serde(rename)`. As written, the TOML key would not deserialize.

This is also called out in `_audit/config-audit.md` C3 — included here because it directly impacts the failure-model heartbeat semantics (a misnamed field means heartbeat detection cannot be tuned at all from `meta.toml`, leaving the timeout pinned to the 5000-ms default).

**Recommended fix**: rename the LLD struct field to `heartbeat_timeout_ms` (and the default fn to `d_heartbeat_timeout_ms`). Single canonical name across all docs.

---

### C6. `heartbeat_interval_ms` is documented as a tunable in 4 docs but hardcoded in `LLD.md`.

**Files**: `configuration.md` §4.3 (L185–194), `failure-model.md` §3.1 (L97–98), §8.1, `operations.md` §3.1 (L154, L169), `protocol.md` §6.4 (L764) vs `LLD.md` §4.5 (L819) and §5.4 (L1138).

**Issue**: `configuration.md` lists `heartbeat_interval_ms` as type `u64`, default 1000, range 100..60000. failure-model.md, operations.md, and protocol.md all assume the user can tune it. `LLD.md`'s `StoreConfig` (L483–492) has no such field — both heartbeat-related places hardcode `tokio::time::interval(Duration::from_secs(1))`.

Failure-model §3.1's detection paragraph specifically says "every `heartbeat_interval_ms` (default 1000 ms)" — that wording promises tunability the LLD does not deliver.

This is also flagged in `_audit/config-audit.md` C4; surfaced here because it is co-load-bearing with the failure-model heartbeat-timeout claim.

**Recommended fix**: Add `heartbeat_interval_ms: u64` to `LLD.md`'s `StoreConfig` with `#[serde(default = "d_heartbeat_interval_ms")] = 1000`, and replace the hardcoded `Duration::from_secs(1)` with `Duration::from_millis(cfg.heartbeat_interval_ms)`.

---

## MINOR (drift; correctness questionable but recoverable)

### M1. `protocol.md` introduces a `heartbeat_miss_threshold` knob that no other doc defines.

**Files**: `protocol.md` §6.4 (L765–766) vs `failure-model.md` §3.1 (L96–101), `configuration.md` §3.1.

**Issue**: protocol.md describes the down-detection as "`heartbeat_miss_threshold` consecutive misses (default 5 → 5 s grace)". `failure-model.md` and `configuration.md` describe the same fact as a wall-clock test: `now − last_seen > heartbeat_timeout_ms`. The LLD watcher (L817–840) implements the wall-clock test, not a miss counter. Either model arrives at "5 s" with the defaults, but the knob name (`heartbeat_miss_threshold`) does not exist in `configuration.md`.

The user-prompt phrasing — "interval 1s, timeout 5s, declare Down after 5 missed" — is itself a hybrid that protocol.md alone honors literally. failure-model.md and the LLD honor only the wall-clock half.

**Recommended fix**: Drop `heartbeat_miss_threshold` from protocol.md §6.4 and rephrase to match the wall-clock model in configuration.md. The "5 missed × 1 s = 5 s" arithmetic can stay as expository text but should not introduce a new config name.

---

### M2. `failure-model.md` operator-error §6.1 references a different metadata header name than `protocol.md`.

**Files**: `failure-model.md` §6.1 (L444) vs `protocol.md` §1.2 (L46, L53, L144) vs `HLD.md` §2 (L56) vs `architecture.md` §1, §2, §6.2.

**Issue**: failure-model says the wire header is `x-cluster-token`. protocol.md (canonical) says `cluster-token` (no `x-` prefix). HLD.md and architecture.md say `x-dtmpfs-token`. Already documented in `_audit/config-audit.md` C1 and `_audit/protocol-audit.md` C1. Mentioned here because the "Wrong cluster_token" runbook reproduces a name the server, per protocol.md, will not actually look at.

**Recommended fix**: Defer to whatever protocol-audit.md C1 resolves; mechanically propagate to failure-model.md §6.1.

---

### M3. failure-model.md §3 has 4 single-fault scenarios; operations.md §7 / failure-model §9 runbooks cover only 3 of them — partition has no runbook.

**Files**: `failure-model.md` §3.4 (network partition, L269–308) vs `failure-model.md` §9 (runbooks 9.1–9.6) vs `operations.md` §7 (triage tree).

**Issue**: §3.4 is a documented single-fault scenario but neither `failure-model.md` §9 nor `operations.md` §7 contains a "partition" runbook or triage branch. The §3.4 prose says "On heal: heartbeats resume; meta marks the previously-Down stores `Up`" — that's recovery in passing, not a runbook. The user's audit prompt explicitly asks for this gap.

`operations.md` §7's triage tree forks on "every op EIO" / "ls hangs" / "close returns ENOSPC" / "cross-host visibility takes >1s" / "md5 mismatch". None of these branches mention partition; an operator on the no-meta side will reach the "every op EIO" branch and end up running the meta-down runbook on a healthy meta.

**Recommended fix**: Add a §9.7 runbook in `failure-model.md` ("partition") and a triage branch in `operations.md` §7 ("If meta is reachable but only some stores are Up: §9.7 partition; vs §9.1 store-down").

---

### M4. Runbook for §3.3 (client crash) is only partially mirrored by `operations.md`'s mount-stuck triage.

**Files**: `failure-model.md` §3.3 (L216–268), §9.3 (L668–698) vs `operations.md` §7 (L500–538).

**Issue**: failure-model.md has §3.3 (client crashes → ENOTCONN) and a runbook §9.3 (mount-stuck → kill+remount). operations.md §7 troubleshoots two related symptoms ("every op EIO" and "ls hangs forever") but never directly references the ENOTCONN signature from §3.3. An operator who sees ENOTCONN in `journalctl --user -u dtmpfs-client` will not find that exact string in operations.md's troubleshooting flowchart.

**Recommended fix**: Add an explicit "Symptom: every op returns ENOTCONN" branch in operations.md §7 that points at failure-model.md §3.3 / §9.3.

---

### M5. Heartbeat parameter agreement: per-doc framing.

**Files**: `failure-model.md` §3.1, `operations.md` §3.1 TOML, `configuration.md` §3.1 / §4.3, `HLD.md` §I8, `protocol.md` §6.4, `testing.md` §6.1.

**Issue**: The numbers (interval 1000 ms, timeout 5000 ms) agree across `configuration.md`, `operations.md`, `failure-model.md`, `HLD.md`, and `protocol.md`. The framing differs (wall-clock vs miss-count, see M1). `testing.md` §6.1 (L520–523) further muddies things by claiming "Production heartbeat is 5 s; a store goes Down after 5 misses (25 s)" — this is wrong (5 × 1 s = 5 s, not 25 s) and is already flagged by `_audit/config-audit.md` C10. failure-model.md does *not* contain this arithmetic error itself, but anyone cross-referencing testing.md will hit a contradiction.

**Recommended fix**: Fix `testing.md` §6.1 (out of scope for this audit; flagged for awareness).

---

### M6. failure-model.md §5.4 inode budget is not consistent with `HLD.md` §6.2.

**Files**: `failure-model.md` §5.4 (L420–436) vs `HLD.md` §6.2 (L153).

**Issue**:
- HLD §6.2: "256 bytes per inode plus ~64 bytes per directory entry plus ~48 bytes per `BlockPlacement` per block".
- failure-model.md §5.4: "~200 B per empty file plus size of `blocks: BTreeMap<...>` per regular file. Soft limit ~200M inodes on a 200 GB-RAM meta host".

200 B vs 256 B is a 28% discrepancy and the units differ (failure-model uses "per empty file"; HLD uses "per inode"). Neither is wildly wrong but the alarm threshold ("Alert at >80% of host RAM") in failure-model.md depends on which constant is correct. Operations.md §9.3 (L641–649) reuses the HLD constants: "200 B × num_inodes + 32 B × num_blocks + 200 B × num_open_handles" — yet a third set of constants (200 B per inode, 32 B per block, vs HLD's 256/48).

**Recommended fix**: Pick one constant table (HLD's looks the most concrete with separate per-block/per-dirent costs) and propagate.

---

### M7. failure-model.md §5.3 mentions `fuse_threads` default 4 vs the LLD's `d_fuse_threads` default 4 — but the docs disagree on whether queue-full is "rare".

**Files**: `failure-model.md` §5.3 (L410–418), `operations.md` §6.4 (L478–482), `LLD.md` §2 (L508–509, L517).

**Issue**: Numbers agree (default 4). Wording: failure-model.md says "Rare on a healthy cluster"; operations.md §6.4 gives a concrete threshold "If `waiting` sits at 12+, raise `fuse_threads`". Defensible split but the failure-model framing as "rare" might lead operators to dismiss queue saturation. Cosmetic, but worth aligning.

**Recommended fix**: Adopt operations.md's "12+ in `/sys/fs/fuse/connections/*/waiting` is the trigger" wording in failure-model.md §5.3 too.

---

## COSMETIC (stylistic; safe to ignore)

### X1. failure-model.md uses "EIO" both with and without backticks.

**Files**: `failure-model.md` throughout.

**Issue**: §3.1 alternates between `EIO` (backticked, when discussing the errno value) and EIO (unquoted, in §1 stance). Stylistic.

---

### X2. failure-model.md §3.2 vs §4.2 disagree on whether meta-restart orphans require a store restart.

**Files**: `failure-model.md` §3.2 (L204–205), §4.2 (L329–330).

**Issue**:
- §3.2: "Phase 6 GC will sweep them; in v1 they consume RAM until the store restarts."
- §4.2: "The orphans on surviving stores are reclaimed on the next store restart (or by Phase-6 GC)."

Both agree on the v1 mechanism (store restart) and on Phase-6 (GC). §3.2 is slightly clearer that the wait is potentially indefinite. Cosmetic; no behavior difference. (See C4 for the larger Phase-6 vs Phase-8 question.)

---

### X3. failure-model.md §3.1 manual recovery references `:7300` but §8.1 / §8.2 also reference `:7300`. Operations.md §6.2 also uses `:7300`. failure-model.md §9.5 step 1 mistakenly uses `:7200`.

**Files**: `failure-model.md` §9.5 step 1 (L737) vs §8.1 / §8.2 / §3.1 / `operations.md` §6.2 / `configuration.md`.

**Issue**: §9.5 step 1 (L737): `curl -s http://$s:7200/debug/blocks` — that's the gRPC port, not the debug HTTP port. Every other reference uses `:7300+N`. Already flagged by `_audit/config-audit.md`. Cosmetic to this audit (since `:7200` will not respond to `/debug/blocks` curl, the operator will notice immediately), but worth fixing to keep the runbook copy-paste-correct.

---

### X4. failure-model.md §10 calls Raft "Phase 7"; some prose elsewhere says "Phase 7+/8" or "Phase 7+".

**Files**: `failure-model.md` §10 (L794) "Phase 7 Raft for meta; Phase 7+/8 WAL; Phase 8+ snapshot-to-object-store" vs `HLD.md` §11 (Phase 7 row, L520) "(stretch) Raft for meta via openraft". Multiple docs hedge with "Phase 7+" or "Phase 8+".

**Issue**: failure-model.md attaches concrete phases to features (Raft = 7, WAL = 7+/8, snapshot = 8+). HLD.md §11 has only 7 rows and Phase 7 is itself "stretch". WAL is not in the HLD phase table at all. Cosmetic; the directional ordering matches.

**Recommended fix**: Either add Phase 8 / 8+ rows to HLD §11's table or soften failure-model §10 to "post-v1 in roughly this order".

---

## Cross-checks summary

| Property                              | Verdict | Notes |
|---------------------------------------|---------|-------|
| `tonic::Status` → libc errno mapping in one place | **FAIL** | LLD has the canonical mapping; protocol.md has the canonical Status-code list. They drift on `RESOURCE_EXHAUSTED` (C2). |
| `unavailable → EIO` everywhere       | OK      | All docs agree. No `unavailable → EAGAIN` was found anywhere. |
| Re-replication phase                  | **FAIL** | Phase 6 (failure-model) vs Phase 8 (HLD §12). See C3. |
| Orphan GC phase                       | **FAIL** | Phase 6 sweep (failure-model, consistency.md) vs Phase 8/never (HLD §12). See C4. |
| HLD CP-under-partition stance         | OK      | All docs agree. No accidental "AP"/"eventual" claims found. consistency.md §3.2 is explicit ("**not** linearizable, **not** eventually consistent in any non-CP sense"). |
| Data-loss claims (meta-restart wipes metadata; store-restart wipes its blocks) | OK | failure-model §3.1 / §3.2, HLD §12, operations.md §11, consistency.md all agree dtmpfs is RAM-only and any restart loses local state. |
| failure-model §3 (4 scenarios) covered by operations.md runbooks | **PARTIAL** | Partition (§3.4) has no runbook anywhere; client-crash (§3.3) only partially in operations.md. See M3 / M4. |
| Heartbeat parameters consistent across failure-model / operations / configuration | **PARTIAL** | Numbers agree; field names disagree (`heartbeat_timeout_ms` vs `heartbeat_dead_ms` in LLD; `heartbeat_interval_ms` not in LLD struct at all). See C5 / C6 / M1. |
| Resource limits (FD / inode / FUSE queue) | OK | Numbers agree across failure-model §5.2–§5.4, operations.md §6.4 / §9.3, with the small inode-cost discrepancy in M6. |
| User-visible errno per scenario       | OK except C2 | EIO for store/meta/partition/token mismatch; ENOTCONN for client crash; ENOSPC for RAM full (broken — see C2). |

---

## Suggested fix order

1. **C2** (ResourceExhausted → ENOSPC missing in LLD) — highest priority; a real behavior bug.
2. **C1** (R≥2 read failover) — load-bearing claim; easy edit to failure-model.md.
3. **C3 / C4** (Phase 6 vs Phase 8 for re-replication and orphan GC) — pick one phase and propagate; coordinate with whoever owns the roadmap.
4. **C5 / C6** (heartbeat field naming and missing config field) — small LLD edits; unblocks honest operator tuning.
5. **M3 / M4** (partition + client-crash runbooks in operations.md) — content additions, not contradictions.
6. **M1, M2, M5–M7, X1–X4** — cleanup pass, can be batched.
