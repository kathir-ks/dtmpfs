# Wire-Protocol Consistency Audit

Audit of `protocol.md` (canonical) against `HLD.md`, `architecture.md`, and `LLD.md`.

## Summary
- **9 CRITICAL** contradictions (semantic disagreement that would change behavior)
- **8 MINOR** inconsistencies (naming/typing drift, no observable behavior change)
- **3 COSMETIC** issues (stylistic; safe to ignore)

---

## CRITICAL (semantic disagreement that would change behavior)

### C1. Cluster-token metadata header name disagrees four ways

**Files**: `protocol.md` §1.2 vs `HLD.md` §2 / `architecture.md` §1, §2, §6.2 vs `configuration.md` / `failure-model.md` / `operations.md`

**Issue**: protocol.md (the normative spec) says the gRPC metadata header is named **`cluster-token`** (lines 46, 53, 144, 993, 1017). The other docs disagree:

- `HLD.md` line 56:
  > "**Cluster token** — a static shared secret carried in a gRPC metadata header (`x-dtmpfs-token`)"
- `architecture.md` line 45 (diagram), line 100, line 512:
  > `metadata.x-dtmpfs-token = <cluster_token>`
- `configuration.md` line 81: `x-cluster-token`
- `failure-model.md` line 444: `x-cluster-token`
- `operations.md` lines 467, 513, 759, 769: `x-cluster-token`

A real client implementing per architecture.md will send `x-dtmpfs-token`, the server (per protocol.md) will read `cluster-token`, and **every RPC will fail with `UNAUTHENTICATED`**.

**Recommended fix**: keep `protocol.md`'s `cluster-token`. Update HLD/architecture/configuration/failure-model/operations to match. (Or, if the `x-` prefix convention is desired, update protocol.md to `x-cluster-token` and propagate.) But pick exactly one.

---

### C2. `CloseReq` field set diverges between protocol.md and LLD.md

**Files**: `protocol.md` §2.1 (lines 360-377) vs `LLD.md` §4.4 (lines 788-806) and §6.5 (lines 1600-1612)

**Issue**: protocol.md defines `CloseReq` with these fields:

```
CloseReq {
  uint64 fh = 1;
  uint64 ino = 2;
  uint64 expected_generation = 3;     // for stale-close rejection
  uint64 new_size = 4;
  repeated uint64 written_block_idxs = 5;
  int64  mtime_s = 6;
  uint32 mtime_ns = 7;
}
```

LLD.md uses different names AND adds/removes fields:

- LLD line 795: `req.dirty_block_idxs` (protocol calls this `written_block_idxs`)
- LLD line 801: `req.allocated_blocks` — a field of `(BlockIdx, BlockPlacement)` pairs that is **not in protocol.md at all**.
- LLD line 999: `Close(fh, new_size, dirty_block_idxs, allocated_blocks) -> CloseResp` — no `expected_generation`, no `mtime_*`.
- LLD's flush code (line 1600) builds:
  ```rust
  let close_req = CloseReq {
      fh,
      ino: ino.0,
      new_size: of.size_hint,
      dirty_block_idxs: written_idxs.iter().map(|i| i.0).collect(),
      ..Default::default()
  };
  ```
  This omits `expected_generation` entirely, which means **the LLD implementation cannot perform the stale-close detection that protocol.md §8.4 step 3 and consistency.md §5.5 explicitly rely on**.

**Recommended fix**: keep protocol.md's shape (it matches consistency.md and the worked example in §8.4). Update LLD.md to:
1. Rename `dirty_block_idxs` → `written_block_idxs`.
2. Add `expected_generation: of.generation.0` to the `CloseReq` builder.
3. Remove `allocated_blocks` from `CloseReq`. The placement bookkeeping happens during `AllocateBlocks`; meta merges placements there, not at Close. (LLD's own §6.5 comment at line 1605 already admits "allocated_blocks were already committed inside meta on AllocateBlocks" — meaning the field is redundant.)
4. Add `mtime_s` / `mtime_ns`.

---

### C3. `CloseResp` field set diverges

**Files**: `protocol.md` §2.1 (lines 379-381) vs `LLD.md` §4.4 (line 805)

**Issue**: protocol.md:

```
message CloseResp {
  Attr attr = 1;     // post-close, post-bump attributes
}
```

LLD.md line 805:
```rust
Ok(CloseResp { new_generation: inode.generation, attr: build_attr(inode) })
```

LLD.md adds a top-level `new_generation` field that protocol.md does not have. Note that `Attr` *already* contains `generation` (protocol.md line 176), so `new_generation` is redundant *and* not on the wire.

The client side reads it back at LLD.md line 1612:
```rust
of.generation = Generation(resp.new_generation);
```
— which would not compile against protocol.md's `CloseResp`.

**Recommended fix**: drop `new_generation` from `CloseResp` in LLD.md. Read the new generation from `resp.attr.generation` instead. (`architecture.md` already implicitly agrees: §3.2 line 173 shows `CloseResp{generation=3}` as a single thing, but it's loose enough to read either way.)

---

### C4. `WriteBlockResp` shape disagreement

**Files**: `protocol.md` §2.2 (lines 494-497) vs `LLD.md` §5.3 (line 1122)

**Issue**: protocol.md:
```
message WriteBlockResp {
  uint32 len = 1;     // bytes written; equals data.len() on success
}
```

LLD.md line 1122 returns:
```rust
Ok(WriteBlockResp { ok: true })
```

— a boolean field that does not exist in protocol.md, and the `len` that *does* exist is missing.

**Recommended fix**: keep protocol.md as-is. Update LLD.md §5.3 to populate `len: data.len() as u32`.

---

### C5. `HeartbeatReq` field names and field-set differ

**Files**: `protocol.md` §2.1 (lines 399-407) vs `LLD.md` §5.4 (lines 1145-1150) vs `architecture.md` §3.8 (lines 388-396)

**Issue**: protocol.md `HeartbeatReq`:
```
string node_id = 1;
string addr = 2;
uint64 used_bytes = 3;
uint64 capacity_bytes = 4;
uint64 epoch_s = 5;
```

LLD.md line 1145:
```rust
HeartbeatReq {
    node_id:   state.node_id.0.clone(),
    addr:      advertise_addr.clone(),
    ram_used:  state.ram_used.load(Ordering::Relaxed),
    ram_total: state.ram_budget,
}
```
— uses `ram_used` / `ram_total` (protocol uses `used_bytes` / `capacity_bytes`), and **omits `epoch_s` entirely**, despite protocol.md's comment (lines 405-406) stating epoch is what lets meta detect a store restart vs steady-state heartbeat.

architecture.md §3.8 line 391-392 lists fields as:
```
HeartbeatNode(NodeId, capacity_mb, used_mb, version)
```
— `version` (not `epoch_s`), `capacity_mb` (not `capacity_bytes`, and the unit changes from bytes to mebibytes!), `used_mb` (same unit issue).

**Recommended fix**: keep protocol.md. Fix LLD field names to `used_bytes` / `capacity_bytes`, add `epoch_s` (e.g., from `std::time::SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()` captured once at startup). Fix architecture.md §3.8 to match field names and unit (bytes, not MiB).

---

### C6. `BlockKey` is the on-the-wire 3-tuple in protocol.md, but LLD's store keys the DashMap by 2-tuple

**Files**: `protocol.md` §2.2 (lines 461-468) vs `LLD.md` §5.1 (lines 1025-1037), §5.2 (line 1067), §5.3 (line 1102)

**Issue**: protocol.md `BlockKey` includes `generation` as part of the key (and the comment at lines 458-460 explicitly says: "Three-tuple because we keep a single block index across multiple generations alive briefly (until GC) so that a stale writer's WriteBlock can be rejected without colliding with the live data.").

LLD.md keys its DashMap by `(InodeId, BlockIdx)` only:
```rust
pub blocks: DashMap<(InodeId, BlockIdx), Versioned<Bytes>>,
```
storing the generation separately as a `Versioned<T>::generation`. The store therefore holds **at most one version per `(ino, idx)` at any moment**, contradicting protocol.md's "multiple generations alive briefly" rationale.

This has a behavioural consequence: the worked example in protocol.md §8.5 (lines 928-946) describes the store at `block_idx:1` having **a gen-8 entry installed by the winner** while a stale gen-7 writer arrives. With LLD's data structure the gen-8 entry overwrites the gen-7 entry in place; this still gives stale-write rejection (LLD §5.3 line 1112 checks `cur.generation > gen`), but it diverges from the "two generations live at once" model in protocol.md.

Note that this also breaks I3 in `HLD.md` line 502:
> "**I3 (block-key uniqueness within a generation).** For a given `(ino, block_idx, generation)` the store holds at most one `Bytes` value per replica."

I3 reads naturally if the key is the 3-tuple; with LLD's 2-tuple scheme the invariant is "one value per `(ino, idx)`, period", which is stricter and slightly different.

**Recommended fix**: pick one model and update both docs.
- Easier: keep LLD's 2-tuple keying, update protocol.md's `BlockKey` rationale (lines 457-460) to say "we keep generation in the key for *future* multi-version support and for the freshness check, but v1 stores hold only the latest gen per `(ino, idx)`."
- Stricter: change LLD §5.1 `DashMap` to be keyed by `BlockKey` proper (3-tuple), with `DeleteBlock`/GC sweeping older generations.

The first option is much smaller and matches what's actually tested.

---

### C7. `ReadBlockResp.data` is `bytes` on the wire but LLD returns `Vec<u8>`

**Files**: `protocol.md` §2.2 (lines 479-482) vs `LLD.md` §5.2 (lines 1067-1075) and §9.1 (lines 1872-1879)

**Issue**: protocol.md declares `bytes data = 1;` and explicitly notes (in the §1.3 build setup and §9.4 of LLD itself) that `tonic_build::configure().bytes(["."])` is set, so the field decodes as `bytes::Bytes` for zero-copy. But LLD §5.2 line 1072:

```rust
data: g.value.value.clone().to_vec(),  // tonic wants Vec<u8> for bytes field
```

is the opposite of what `tonic_build::configure().bytes(["."])` does — with that flag, `bytes` proto fields become `Bytes`, not `Vec<u8>`. The comment is wrong and the `to_vec()` introduces an unnecessary copy.

LLD §9.1 line 1873 acknowledges this:
> `ReadBlockResp { data: cloned.to_vec() }  // one Vec alloc`
> `OR with prost bytes-mode: zero-copy Bytes on the wire too.`

— so the doc itself flags the inconsistency but doesn't fix it.

**Recommended fix**: update LLD §5.2 to return `data: g.value.value.clone()` (a `Bytes`), drop the `to_vec()`. Same for the client-side construction at line 1583-1585 (`data: frozen.to_vec()`).

---

### C8. AllocateBlocks response field name disagreement (`block_map` vs `placements`)

**Files**: `protocol.md` §2.1 (lines 393-395) vs `LLD.md` §6.5 (line 1549)

**Issue**: protocol.md:
```
message AllocResp {
  repeated BlockLoc block_map = 1;
}
```

LLD.md line 1549:
```rust
for loc in resp.into_inner().placements {
```

The field is called `placements` in LLD, `block_map` in protocol.md. Code written against LLD will not compile against the proto spec.

**Recommended fix**: keep `block_map` (matches protocol.md and OpenResp/CreateResp which also use `block_map`). Update LLD.md.

---

### C9. ListNodes response field disagreement (`id` vs `node_id`)

**Files**: `protocol.md` §2.1 (lines 219-237) vs `LLD.md` §11.3 (line 2065)

**Issue**: protocol.md `NodeInfo` has `string node_id = 1;`. LLD.md line 2065 reads:
```rust
addrs.insert(NodeId(n.id), n.addr);
```
— uses `n.id`, not `n.node_id`. The same mismatch is implicit in §4.6 (line 1009) which describes the field as `id`. Code will not compile.

**Recommended fix**: keep `node_id` (protocol.md). Update LLD.md.

---

## MINOR (naming/typing drift, no observable behavior change)

### M1. `OpenFile.generation` vs `OpenFile.open_gen`

**Files**: `architecture.md` §3.5 (line 284: `open_gen_A`, line 287: `open_gen_B`) and §3.4 (line 224: `open_gen:0`) vs `LLD.md` §6.1 (line 1265: `pub generation: Generation`)

**Issue**: architecture.md uses `open_gen` as the field name in pseudocode; LLD's actual struct calls it `generation`. Same concept, but a reader cross-referencing the two will be briefly confused.

**Recommended fix**: rename architecture.md to `generation` (or rename LLD to `open_gen` — `open_gen` is arguably more self-documenting). Pick one.

---

### M2. AttrCache vs HeartbeatResp difference: `last_heartbeat_s` vs `last_seen`

**Files**: `protocol.md` §2.1 (line 232) vs `LLD.md` §4.1 (line 669) and §4.5 (line 825)

**Issue**: protocol.md `NodeInfo.last_heartbeat_s` is `int64` "monotonic seconds since meta start". LLD's `MetaState.last_heartbeat: HashMap<NodeId, Instant>` stores a `tokio` Instant. The internal type can stay `Instant` (fine — internal-only field), but the wire-emitted value must be derivable. LLD doesn't say how — there's no `start: Instant` recorded anywhere in `MetaState` to anchor "seconds since meta start".

**Recommended fix**: add a `pub start: Instant` to `MetaState` in LLD §4.1 and document the conversion in §4.5. (Or change protocol.md to UNIX seconds.)

---

### M3. Replicate RPC's caller listed inconsistently

**Files**: `protocol.md` §3 RPC reference table (line 571) vs `architecture.md` §2 (line 96) vs `HLD.md` §7.3 (line 215)

**Issue**:
- `protocol.md` line 571: caller "meta/store" (i.e., either)
- `architecture.md` line 96: caller "store"
- `HLD.md` line 215 says store sends `Replicate` to peer stores ("Receives `Replicate` RPCs from peer stores.")

architecture.md and HLD agree (store→store). protocol.md adds "meta" as a possible caller, but no other doc shows meta initiating Replicate. HLD §7.2 line 207 explicitly says: "Talks to. Receives RPCs from clients. Receives heartbeats from stores. **Does not initiate any RPCs in v1**."

**Recommended fix**: drop "meta" from the protocol.md §3 caller column for `Store.Replicate`. Caller is "store" only.

---

### M4. `StoreStat.block_count` is in protocol.md but never produced or referenced

**Files**: `protocol.md` §2.2 (line 516)

**Issue**: protocol.md defines `StoreStat.block_count`, but LLD.md doesn't show how stores compute or emit it (StoreState carries no counter and `blocks.len()` would scan the DashMap). Not a contradiction — just an under-specified field. Minor because v1 may simply emit `state.blocks.len() as u64`.

**Recommended fix**: add a one-liner in LLD §5 saying `block_count = state.blocks.len() as u64`, served by a new `Stat` handler. (No `Stat` handler currently exists in LLD.)

---

### M5. `Mkdir` returns `Attr` (protocol) but no doc shows the Mkdir parent's `nlink` bump on the wire

**Files**: `protocol.md` §2.1 (line 425) vs `LLD.md` §4.6 (lines 898-905)

**Issue**: protocol.md says `rpc Mkdir(MkdirReq) returns (Attr);` — i.e., returns the *child*'s Attr only. LLD's pseudo-code at lines 898-905 also bumps `parent's nlink += 1` but never returns the new parent attr. That's fine (clients see updated nlink on the next `GetAttr`/`Lookup`), but a strict reader expecting the `Attr` return to reflect the parent state will be confused.

**Recommended fix**: add a one-line clarification in protocol.md §2.1 saying the returned Attr is the child's, not the parent's. Already implied; just spell it out.

---

### M6. `OpenHandleSt.opener_node` field has no source on the wire

**Files**: `LLD.md` §4.1 (line 693: `pub opener_node: NodeId`) vs `protocol.md` §2.1 `OpenReq`

**Issue**: LLD's `OpenHandleSt` records the `opener_node`, but protocol.md's `OpenReq { ino, flags }` carries no node identifier. Where does meta get the value? It would have to come from a per-channel auth/identity hook (the `cluster-token` doesn't carry per-host identity). LLD doesn't explain how `opener_node` is populated.

This is internal-only state (it's not on the wire), so it's not a wire-protocol contradiction. It is, however, a load-bearing internal field that's not derivable from anything protocol.md says is sent.

**Recommended fix**: either (a) add a `node_id` field to `OpenReq` in protocol.md, or (b) drop `opener_node` from `OpenHandleSt` in LLD §4.1 (it's never read anywhere in the LLD pseudo-code anyway), or (c) document in LLD that it's set from the gRPC peer address obtainable via `tonic::Request::remote_addr()`. Option (b) is least disruptive.

---

### M7. `Inode` (LLD) vs `Attr` (wire) — internal-only fields not flagged

**Files**: `LLD.md` §4.1 (lines 672-686) vs `protocol.md` §2.1 (lines 161-202)

**Issue**: `Inode` carries fields that are **not** on the wire and rightly so:
- `kind: InodeKind` — derivable from `mode & S_IFMT`, not duplicated in `Attr`.
- `blocks: BTreeMap<BlockIdx, BlockPlacement>` — exposed via `OpenResp.block_map`, not in `Attr`.
- `symlink_target: Option<String>` — would be needed for `readlink`, but no `Symlink` RPC exists in protocol.md (HLD §11.3 line 109 "Symlink RPCs" deferred to Phase 3).

These are "internal-only fields that shouldn't be on the wire" per the audit ask. Flagging here so a future contributor knows these are deliberate omissions, not protocol gaps.

**Recommended fix**: add a paragraph in LLD §4.1 explicitly listing which `Inode` fields are wire-exposed (via `Attr` / `OpenResp.block_map`) and which are internal. No code change.

Special case: `symlink_target` plus the line in `LLD.md` §6.6 (line 1645) "`symlink` implemented" is **inconsistent with the absence of any `Symlink`/`Readlink` RPC in protocol.md**. The client cannot create a symlink without a wire RPC. Could be promoted to MINOR-CRITICAL but it's flagged as a non-goal in v1 (HLD §1.4 line 36 doesn't mention symlinks; §11.3 calls them Phase 3). Decision needed.

---

### M8. AllocReq parameter naming (`block_idxs`) vs LLD comments using `indices`

**Files**: `protocol.md` §2.1 (line 390) and §2.1 service decl (line 434) vs `HLD.md` §10.4 (line 461) and `architecture.md` §3.4 (line 235)

**Issue**: protocol.md uses `block_idxs`. HLD §10.4 says `Meta.AllocateBlocks { ino, indices }`. architecture.md §3.4 says `AllocateBlocks(ino=99, indices=[0,1,2])`. LLD line 1546 correctly uses `block_idxs`.

**Recommended fix**: HLD and architecture.md should say `block_idxs` to match the wire field. Pure naming drift.

---

## COSMETIC (stylistic; safe to ignore)

### S1. Sequence-diagram CloseResp "shape" varies stylistically

**Files**: `architecture.md` §3.2 line 173 (`CloseResp{generation=3}`), §3.4 line 256 (`CloseResp{generation=1}`), §3.5 lines 305, 316 (`CloseResp{gen=g+1}`)

**Issue**: architecture.md sometimes writes `generation`, sometimes `gen`. The actual protocol shape (per protocol.md §2.1 line 379) is `CloseResp { Attr attr }`, and `attr.generation` is what carries the value. Sequence diagrams elide this for readability.

**Recommended fix**: optional. Could write `CloseResp{attr.generation=...}` for precision, but the abbreviated form is fine in a diagram.

---

### S2. Heartbeat coupling: protocol's "default 5 → 5 s grace" vs HLD's `heartbeat_timeout_ms` (default 5000)

**Files**: `protocol.md` §6.4 (lines 763-768) vs `HLD.md` §10A I8 (line 506)

**Issue**: Same number, different ways of describing it. protocol.md says "miss threshold 5 with 1 s interval = 5 s"; HLD says "`heartbeat_timeout_ms` default 5000". Both arrive at 5 seconds; the names of the knobs differ slightly (`heartbeat_miss_threshold` + `heartbeat_interval_ms` in protocol.md vs `heartbeat_timeout_ms` in HLD). Minor; configuration.md is the source of truth. Worth aligning if you regenerate the docs.

---

### S3. `architecture.md` §3.6.1 R=1 read-failure narrative says replicas list "is empty" while `BlockLoc.replicas` is `repeated string` (always present)

**Files**: `architecture.md` §3.6.1 line 339

**Issue**: Trivial wording: "block_map[k].replicas is empty (R=1)" — protobuf `repeated` fields are always present (length 0 means empty). The statement is correct but a pedantic reader could nit it. Cosmetic.

---

## Verified consistent

The following items were checked and found to align across all four documents:

- **Service split**: All four docs (protocol.md §1.1, HLD §F6/F7, architecture.md §1, LLD §4-5) agree the two services are `dtmpfs.meta.v1.Meta` and `dtmpfs.store.v1.Store`, hosted by separate processes. Meta and Store RPCs are not crossed (e.g., `WriteBlock` is never accidentally listed under Meta).
- **HeartbeatNode direction**: All docs (protocol.md §3 line 567, HLD §F6, architecture.md §2 line 95 and §3.8, LLD §5.4) agree heartbeat is **store → meta**, not the reverse.
- **`AllocateBlocks` is its own RPC, not folded into `Open`**: All four docs treat `Meta.AllocateBlocks` as a separate RPC called on first dirty-write of a new index (protocol.md §2.1 line 434, architecture.md §2 line 90, HLD §10.4 / §10A.I6, LLD §6.5).
- **`Close` bumps generation only-if-dirty**: This invariant is consistent everywhere — protocol.md §2.1 line 371-372 (`written_block_idxs` empty ⇒ no bump), §8.4 step 4, HLD §10.8 step 7, architecture.md §3.4 implicit, LLD §4.4 line 795-797 (`if dirty { ... bump ... }`), `consistency.md` §4. (Field-name disagreement noted above in C2, but the *semantics* match.)
- **`BlockKey` includes generation on the wire**: protocol.md §2.2, LLD's wire-side `BlockKey` matches (the in-memory store map is what differs — see C6).
- **Stale-write rejection on store maps to `FAILED_PRECONDITION`**: protocol.md §3.1 / §4 / §8.5, LLD §5.3, consistency.md, architecture.md §3.5 all agree.
- **`UNAUTHENTICATED` reserved for token failures, `PERMISSION_DENIED` for POSIX denials**: protocol.md §1.2 / §4.1, HLD §F9, consistent.
- **`Open` is *not* idempotent; client must remember the `fh`**: protocol.md §3.1, HLD §10.1, LLD §6.5, all aligned.
- **HRW placement is computed on `(ino, block_idx)`, generation is zeroed for placement key**: HLD §9.4, architecture.md §4.2, LLD §3 / §4.3 line 766-768. Consistent.
- **R replicas returned in score-sorted order; element 0 is primary**: HLD §2 (Primary), architecture.md §4.3, LLD §3.2 doc-comment line 549. Consistent.
- **gRPC default deadlines**: protocol.md §1.5 vs architecture.md §6.1 line 507 (5 s control / 30 s data) — match.
- **Block size 1 MiB default**: HLD §6, protocol.md Appendix D, architecture.md §4.1, LLD §2.3 (`d_block_size = 1 << 20`). Consistent.
- **The canonical close-to-open race walkthrough** (`architecture.md` §3.5 vs `consistency.md` vs protocol.md §8.4-8.5) is told consistently across docs.
- **Default replication factor R=1, max R=3**: HLD §F5, LLD §2.3 (`d_replication = 1`), protocol.md Appendix D. Consistent.
