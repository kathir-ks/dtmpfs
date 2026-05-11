# Consistency & Data-Model Audit

Read-only audit of `/home/kathirks_gc/dtmpfs/docs/{consistency,LLD,protocol,failure-model,HLD}.md`
against the criteria in the audit brief. Cross-checks against `architecture.md`
and `testing.md` are noted where one of those documents corroborates or
contradicts the in-scope set.

## Summary

- 3 CRITICAL
- 5 MINOR
- 3 COSMETIC

## CRITICAL

### C1. fsync semantics — three docs publish a new generation, one explicitly says it does not

**Files**: consistency.md §2.1 / §7.5 vs HLD.md §F3 / §9.3 / §10.4 / §10.8 vs LLD.md §6.5–§6.6 vs testing.md §A (corroborating LLD)

**consistency.md §2.1 says** (line 72):

> "After a successful close on host X, an `open` on host Y observes the
> written bytes. **No additional `sync`/`fsync` is required from the user.**"

**consistency.md §7.5 says** (lines 672–678):

> "`fsync(2)` (without `O_SYNC`)
> Same as `flush`: emit pending writes, **do not** call `Meta.Close`.
> `fsync` makes the data durable on the **primaries** but does **not**
> publish a new generation; other hosts still see the pre-fsync state."

**HLD.md §F3 says** (line 109):

> "Writes performed by a client become visible to subsequent `open()` calls
> on any client once `close()` (or `fsync()`) on the writer's file
> descriptor has returned successfully (close-to-open)."

**HLD.md §9.3 says** (line 316):

> "close-to-open. Writes visible cluster-wide on `close()`/`fsync()`"

**HLD.md §10.4 says** (line 458): *both `close(fh)` and `fsync(fh)` "land in
the same flush logic"* — and that logic ends with "the client issues
`Meta.Close { ... }`" + "bumps generation" (line 463).

**LLD.md §6.6 says** (line 1653):

> "`fsync` | implemented | Same as `flush`; RAM-only."

…and `flush_internal` (LLD §6.5, line 1609) calls `Meta.Close`, which under
LLD §4.4 (line 797) **bumps `inode.generation`**. So in the LLD code path,
`fsync` does publish a new generation, contradicting consistency.md §7.5.

**testing.md §A line 357–360 corroborates LLD**, not consistency.md:

> "`fsync(fd)` forces `Meta.Close`-equivalent. Because dtmpfs maps `fsync`
> to 'flush dirty blocks + bump generation under the meta lock', `fsync`
> is the right primitive for 'make this write visible'."

**failure-model.md §1.4 (line 26–28)** is hedged:

> "`fsync(2)` is not a barrier against power loss; it is only a barrier
> against close-to-open visibility."

…which only makes sense if `fsync` *is* the visibility barrier (i.e., does
publish). So failure-model.md tacitly sides with HLD/LLD/testing.

**The audit brief** itself says: "fsync should be equivalent to flush (per
the chosen design): wait for primaries to ACK; with R≥2, wait for replicas
too. The plan says 'wait for replica ACKs' for fsync."

**Recommended fix**: Pick one. The HLD/LLD/testing trio (fsync ≡ flush ≡
publish a new generation) is the dominant story in the corpus, matches the
audit brief's description of the chosen design, and matches the way `dd
conv=fsync` is used in `acceptance-tests.md` (lines 1129, 1744). Update
consistency.md §7.5 to say:

> "`fsync` is the same as `flush` and as the visibility-publishing tail of
> `close`: it flushes dirty blocks to primaries (and to replicas if
> `R>=2`), then issues `Meta.Close` which bumps generation. The only
> difference vs `close(2)` is that the file descriptor remains open."

…and remove the "does not publish a new generation" sentence.

If the *consistency.md* story is the one we actually want (fsync flushes
without bumping generation, matching strict NFSv3), then HLD §F3, HLD §9.3,
LLD §6.6's "Same as `flush`" entry, the LLD `flush_internal` body, and the
testing.md §A passage all need to be rewritten, and `acceptance-tests.md`
needs an audit pass.

The audit brief frames this as "wait for primaries to ACK; with R≥2, wait
for replicas too" — i.e. fsync **does** publish, and the only `fsync` knob
is replica-wait. Architecture.md §4.6 (line 456) matches this:

> "on `fsync`/`close`, the meta receives the `written_idxs` after primaries
> acknowledge. Replicas converge in the background unless
> `client.fsync_wait_replicas = true`"

So architecture.md, HLD, LLD, testing.md, and the audit brief agree;
**consistency.md §7.5 is the single outlier** and should be rewritten.

---

### C2. Stale-write rejection — LLD ships it in v1, every other doc says Phase-6

**Files**: LLD.md §5.3 vs HLD.md §11 / §10A.I6 vs failure-model.md §7.3 vs
architecture.md §3.5 vs protocol.md §3.1

**LLD.md §5.3 (lines 1083–1131)** implements stale-write rejection in the
v1 store handler:

> ```rust
> Occupied(mut e) => {
>     let cur = e.get();
>     if cur.generation > gen {
>         return Err(tonic::Status::failed_precondition("stale write"));
>     }
> ```

The section heading is literally "**5.3 Write path with stale-write
rejection**". LLD §3 line 383 even defines a dedicated error variant
`#[error("block generation mismatch (write rejected as stale)")]`.

**HLD.md §11 phased roadmap (line 519)** says:

> "P6 | Heartbeats, **stale-write rejection**, GC, retries"

**HLD.md §10A.I6 (line 505)** says:

> "Stores that receive a `WriteBlock` for an unknown placement simply
> accept it (**Phase 6 stale-rejection extends this to verify the
> generation**), but meta will not point future readers at the data unless
> it has issued the placement."

**failure-model.md §7.3 (lines 533–537)** says:

> "Partial mitigation: `BlockKey` carries `generation`, so writes at G and
> G+5 land in different DashMap cells … The store does **NOT** yet reject
> writes for a too-old generation (`Status::failed_precondition`); that is
> Phase 6."

**architecture.md §3.5 (line 327)** says:

> "**Phase 6 hardening introduces stale-write rejection on the store**:
> if `WriteBlock` arrives with a `generation` older than what the store
> last saw for that `(ino, idx)`, it is rejected with
> `Status::failed_precondition`."

**protocol.md §3.1 (line 597–599)** is on the fence: it documents the
behavior as if it works in v1 ("The generation in `BlockKey` ensures stale
writes … are rejected with `Status::failed_precondition`"). protocol.md
§8.5 (line 928–946) walks through a full stale-rejection example without
labeling it Phase 6. consistency.md §4.4.3 (lines 252–266) describes the
mechanism normatively, also without a Phase-6 caveat — though it stays
just shy of saying "v1 does this".

**Recommended fix**: Phase 6 is the right answer per the project plan. Two
edits:

1. **LLD §5.3**: keep the code as a forward-looking sketch, but retitle to
   "Write path with **planned (Phase 6) stale-write rejection**" and add a
   v1-behavior box noting that the v1 store accepts the write and
   overwrites; the `if cur.generation > gen` arm is wired in P6.

2. **protocol.md §3.1 (last bullet on `WriteBlock`) and §8.5**: add a
   one-line "(Phase-6 hardening; v1 stores accept the stale write and the
   loser learns about the conflict at `Meta.Close` time, see
   consistency.md §5.4)".

**Or** — if the LLD code is the truth and the project has decided to ship
stale-write rejection in v1 — update HLD §11, HLD §10A.I6, failure-model
§7.3, and architecture.md §3.5 to remove the "Phase 6" label. The audit
brief is explicit: "the plan says Phase-6; check no doc claims it works in
v1 unless it's been deliberately upgraded." We did not see a deliberate
upgrade note anywhere, so the brief's preferred direction is to keep
Phase-6 and demote LLD §5.3.

---

### C3. CloseReq field is named `dirty_block_idxs` in LLD but `written_block_idxs` in protocol

**Files**: LLD.md §4.4 / §6.5 / §4.6 vs protocol.md §2.1 / §8.4 vs
consistency.md §4.2 / §5.3

**protocol.md §2.1 (line 372)** is canonical wire:

> ```proto
> // Block indices the client wrote during this open. Empty list means
> // no dirty blocks => Close MUST NOT bump generation.
> repeated uint64 written_block_idxs = 5;
> ```

**consistency.md §4.2 (line 203)** uses the same name:

> "`Meta.Close` bumps `generation` **iff** the client's `CloseReq`
> reports a non-empty `written_block_idxs`."

**consistency.md §5.3 (line 392, line 399)** also uses
`written_block_idxs`.

**LLD.md §4.4 (line 795)**:

> ```rust
> let dirty = !req.dirty_block_idxs.is_empty();
> ```

**LLD.md §4.6 (line 999)**:

> "`Close(fh, new_size, **dirty_block_idxs**, allocated_blocks) -> CloseResp`"

**LLD.md §6.5 (line 1604)**:

> ```rust
> dirty_block_idxs: written_idxs.iter().map(|i| i.0).collect(),
> ```

— this last line is where the rename gets concrete: the local Rust
variable is `written_idxs`, the proto field per protocol.md is
`written_block_idxs`, but the LLD code attempts to assign to a struct field
called `dirty_block_idxs`. Compiled against the protocol.md proto, this
fails to build.

There is also a downstream effect in `CloseResp`: LLD.md §4.4 (line 805)
returns `CloseResp { new_generation, attr }`, but protocol.md §2.1 (lines
379–381) defines `CloseResp` as `{ Attr attr = 1; }` — no
`new_generation` field. The generation is supposed to be read from
`attr.generation`. LLD.md §6.5 (line 1612) further uses
`resp.new_generation` on the client side, which would also fail to
compile against the protocol.md proto.

**Recommended fix**: protocol.md is canonical wire, so LLD.md should be
edited:

- Rename every `dirty_block_idxs` to `written_block_idxs` in §4.4, §4.6,
  §6.5.
- Drop the `new_generation` field from `CloseResp`; read it from
  `resp.attr.generation` instead. Fix the line at §6.5 line 1612 to
  `of.generation = Generation(resp.attr.unwrap().generation);` (or
  whatever the LLD's fuser-attr conversion is).

A side benefit: `WriteBlockResp` in LLD §5.3 (line 1122) returns
`{ ok: true }`, but protocol.md §2.2 defines `WriteBlockResp { uint32
len = 1; }` — same class of bug, same fix direction (LLD follows
protocol).

## MINOR

### M1. consistency.md §3.2 says "not linearizable", HLD.md §6 NFR-avail-1 says "CP" — wording fine but failure-model.md §1.2 is loose

**Files**: HLD §6 (NFR-avail-1) vs consistency.md §3.2 vs failure-model.md
§1.2

**HLD.md §6 NFR-avail-1 (line 127)**: "**CP** in CAP terms during a
network partition." Clean.

**consistency.md §3.2 (lines 130–135)**: "dtmpfs is **not** linearizable."
Clean and orthogonal (CAP-CP ≠ linearizable; CP just means we don't serve
on the no-meta side).

**failure-model.md §1.2 (line 22–24)**: "**CP under partitions.** When the
network splits, the side that contains `dtmpfs-meta` continues to operate
…". This is fine but slightly muddled: CP is usually phrased "consistency
+ partition tolerance, sacrificing availability"; the doc instead phrases
it as "the meta side wins". Same semantics, but a reader expecting the
canonical wording may stumble. **Recommended fix**: prepend one sentence —
"In CAP terms, dtmpfs sacrifices availability under partition (CP)." —
then the rest of §1.2 reads as the explanation.

No doc claims AP. This passes the brief's check.

---

### M2. Two-writer overlapping-block loss is described differently in consistency.md §5.4 vs failure-model.md (no dedicated section)

**Files**: consistency.md §5.4 vs failure-model.md §4.3

**consistency.md §5.4 (lines 414–446)** is the canonical narrative: A
closes first (gen 7→8), B's close fails with `FAILED_PRECONDITION`, B
maps to `EIO`, B's writes are lost; the "loss case" name is retained
because somebody's bytes never reach gen 8. There is also an opt-in
`meta.allow_overwriting_close = true` for NFS-classic last-close-wins.

**failure-model.md §4.3 (lines 333–346)** covers "Network partition during
write" but **not** the same-block two-writer race in any scenario. The
document never describes the close-conflict outcome; readers looking for
"what happens when two clients write the same block" in failure-model.md
won't find it. failure-model.md only mentions the outcome by
cross-reference in §"see also" (line 805).

**Recommended fix**: add a §4.5 "Two writers, same block" to
failure-model.md that mirrors consistency.md §5.4 in 5–10 lines, with the
errno mapping (`EIO` on the loser's `close`), and the
`meta.allow_overwriting_close` opt-in. Not a contradiction — just a gap
that the brief's check #5 expected to be filled.

Architecture.md §3.5 (lines 280–325) covers a different framing of the
same race ("each client's RMW is consistent with the version it opened,
but the on-store value is whichever finished writing last"); that text is
*not* contradicted by consistency.md §5.4 once you read both, but a casual
reader will think they disagree because architecture.md describes a
silent-overwrite outcome and consistency.md describes a noisy one. The
reconciliation lives in the `meta.allow_overwriting_close` toggle —
architecture.md should mention it inline.

---

### M3. consistency.md §4.5 says SetAttr does not bump generation; HLD.md §10.7 says it does

**Files**: consistency.md §4.5 vs HLD.md §10.7

**consistency.md §4.5 (lines 277–281)**:

> "`SetAttr`: bumps `ctime` but **not** `generation`. Truncate via SetAttr
> changes file size but not generation; observers re-fetch attr through
> the normal cache TTL path. **This is a deliberate design choice**"

**HLD.md §10.7 (lines 476–477)**:

> "`Meta.SetAttr { ino, size: Some(new_size), .. }`. The meta server
> adjusts `Inode.size`, drops `Inode.blocks` entries with `block_idx >=
> ceil(new_size / block_size)`, **and bumps `generation`**."

This is a direct contradiction between two canonical-track docs. LLD.md
§4.6 SetAttr handler (line 875–880) is silent on generation, just says
"ctime = now"; that aligns with consistency.md.

consistency.md §7.3 (line 660) is also explicit: "Does **not** bump
`generation` (SetAttr never does)."

protocol.md does not take a position; it just defines `SetAttrReq`/`Resp`.

**Recommended fix**: keep consistency.md's rule (SetAttr does not bump
generation; cross-host truncate visibility relies on AttrCache TTL).
Update HLD.md §10.7 to remove "and bumps `generation`" — replace with
"**does not** bump `generation`; cross-host visibility of the truncate is
bounded by `attr_cache_ttl_ms` (see consistency.md §7.3)".

---

### M4. Inode struct in LLD vs Attr message in protocol — small drift

**Files**: LLD.md §4.1 (lines 672–686) vs protocol.md §2.1 (lines 161–202)

LLD `Inode` fields: `ino, kind, mode, uid, gid, size, nlink, atime, mtime,
ctime, generation, blocks (BTreeMap), symlink_target`.

protocol `Attr` fields: `ino, size, blocks (count, u64), generation, mode,
nlink, uid, gid, atime_{s,ns}, mtime_{s,ns}, ctime_{s,ns}`.

Mismatches:

- **`blocks`**: in `Inode` it is `BTreeMap<BlockIdx, BlockPlacement>` —
  the actual placement map. In `Attr` it is a `uint64` count of
  blocks. Same name, different type and meaning. This is a footgun for
  anyone doing literal field-by-field translation in a serializer; worth
  a one-line note in either doc that `Attr.blocks` is the count and
  `Inode.blocks` is the placement map.
- **`kind`**: present on `Inode` (`InodeKind`), absent on `Attr` — the
  POSIX bit is folded into `Attr.mode` via `S_IFREG`/`S_IFDIR`/`S_IFLNK`.
  Fine, but noting it explicitly in LLD.md §4.1 would prevent confusion.
- **`symlink_target`**: present on `Inode`, absent from `Attr`. Symlink
  resolution is via a separate `Meta.Readlink` (deferred per protocol.md
  §9). Fine; just confirm the next pass on protocol.md leaves room (field
  numbers 15–19 are reserved per §7.1, line 794).
- **timestamps**: `Inode` uses `SystemTime`, `Attr` uses split `_s`/`_ns`
  pairs. The translation rule is documented in protocol.md §2.1 header
  (line 145–149). Fine.

**Recommended fix**: in LLD.md §4.1 add 3 lines after the `Inode` struct:

> "When converting `Inode` → wire `Attr`: `Inode.kind` collapses into
> `Attr.mode` via the `S_IFMT` high bits; `Inode.blocks.len()` becomes
> `Attr.blocks` (count, not the map); `SystemTime` splits into the
> `*_s`/`*_ns` pair; `symlink_target` is not surfaced on `Attr`."

---

### M5. consistency.md §1.1 wording on "primary-only is v1 default" vs HLD §10.4 wording on "all primaries ack"

**Files**: consistency.md §1.1 vs HLD.md §10.4 / §10.8

**consistency.md §1.1 (lines 22–25)**:

> "(a) every dirty block on the handle was flushed (`Store.WriteBlock`
> returned OK on the primary, **and on `R-1` replicas if the client is
> configured to wait — v1 default is primary-only**)"

**HLD.md §10.4 step 2 (line 462)**:

> "The client constructs a stream of `Store.WriteBlock` futures, **one per
> dirty block per replica**, and runs them with `buffer_unordered(16)`."

**HLD.md §10.8 step 5 (line 487)**:

> "Client fans out `Store.WriteBlock` to primaries **(and replicas if
> `R>=2`)** with `buffer_unordered(16)`."

**HLD.md §10.4 step 3 (line 463)**:

> "On success, the client issues `Meta.Close { fh, new_size, mtime,
> written_idxs }`."

— "on success" is ambiguous about whether replicas had to ack.

**LLD.md §6.5 (lines 1577–1591, 1625–1628)** is the implementation: each
flush writes to *every* node in `[primary] ++ replicas` via
`try_join_all`, and Close runs after all of those joins. Then LLD says:

> "If the user asks for 'primary acks only' semantics later, swap
> `try_join_all` for `select_ok` returning after the primary write."

So **LLD's v1 default is "wait for all replicas"**, not primary-only.
That contradicts consistency.md §1.1 ("v1 default is primary-only").

architecture.md §4.6 (line 456) lands a third time on this: "primaries
acknowledge … Replicas converge in the background unless
`client.fsync_wait_replicas = true`" — i.e. v1 default is primary-only,
agreeing with consistency.md but disagreeing with LLD §6.5.

HLD.md §13 OQ-3 (lines 569–572) frames the question as still open:
"Proposed: primaries-only by default; expose `client.fsync_wait_replicas:
bool`."

**Recommended fix**: pick one. The HLD §13 OQ-3 proposal is
"primaries-only", which aligns with consistency.md §1.1 and architecture.md
§4.6. The audit brief itself says: "wait for primaries to ACK; with R≥2,
wait for replicas too" — i.e., wait for replicas. So the brief leans
toward LLD §6.5's behavior.

If the chosen design is "wait for replicas" (per the audit brief), update
consistency.md §1.1 and architecture.md §4.6 to match LLD. If the chosen
design is "primaries-only" (per HLD OQ-3 / architecture.md §4.6 / current
consistency.md), update LLD §6.5 to use `select_ok` and update HLD §10.8
step 5 + §10A.I7 wording.

This intersects C1 (fsync semantics) and should be resolved together.

## COSMETIC

### K1. consistency.md §4.1 starts generation at 1; LLD.md initializes at 0

**Files**: consistency.md §4.1 vs LLD.md (new-inode initialization)

**consistency.md §4.1 (line 195)**: "`generation` is a `u64` counter
**starting at 1** at inode creation."

**LLD.md** initializes Inode `generation: Generation(0)` in two places —
line 587 (for a test fixture) and line 766 (for the placement key). The
production path doesn't show a literal "the new inode starts at gen=1"
line, but `Generation::default()` derives `Default` and would yield `0`.

**protocol.md §4.3** in consistency.md (line 226–227) says new files get
`generation: 1`. Tiny inconsistency; either start at 0 (and bump to 1 on
first close) or start at 1 and bump to 2. The user-facing semantics are
identical because nobody can observe gen 0 (it has no published
`block_map`), but the **initial value** should be one number across docs.

**Recommended fix**: initialize new inodes at `Generation(1)` per
consistency.md, and update LLD.md's `Inode { ..., generation:
Generation(0) }` initial-value examples or note in-line that the constant
is just for `BlockKey` placement-hashing, not for the inode's actual
starting generation.

---

### K2. `cluster_token` vs `cluster-token` — header name

**Files**: HLD.md §10A.I9, failure-model.md §6.1 vs protocol.md §1.2 /
Appendix A,B

protocol.md §1.2 line 46, §6.4 not relevant; Appendix A line 994 and
Appendix B line 1018 use `"cluster-token"` (hyphen) for the gRPC metadata
header.

failure-model.md §6.1 line 444 says: "the token in metadata header
`x-cluster-token`" — wrong; protocol.md does not use `x-` prefix.

HLD.md §2 (Glossary) line 56 says: "shared secret carried in a gRPC
metadata header (`x-dtmpfs-token`)" — also wrong; the canonical name is
`cluster-token`.

**Recommended fix**: align failure-model.md §6.1 and HLD.md glossary
entry on `cluster-token` (the protocol.md spelling).

---

### K3. consistency.md §1.2 phrasing of the headline guarantee

**Files**: brief #4 wants: "after `close()` returns successfully on host
A, any subsequent `open()` on any host sees the new bytes"

**consistency.md §1 model statement (lines 14–17)**:

> "After a successful `close()` on file F by client A, any subsequent
> `open()` of F by any client B observes the bytes A wrote."

Matches.

**HLD.md F3 (line 109)**: "Writes performed by a client become visible to
subsequent `open()` calls on any client once `close()` (or `fsync()`) on
the writer's file descriptor has returned successfully (close-to-open)."

Matches the model, with the `fsync` extension that intersects C1.

**HLD.md §1.3 (line 24)**: "Cross-host visibility on close (close-to-open
consistency)."

Fine.

**failure-model.md** does not contain a one-line statement of the
guarantee; it cross-references consistency.md. That's acceptable —
failure-model is descriptive of failures, not of the model. The brief's
check #4 ("every doc that states the guarantee should say the same
thing") is satisfied by the docs that *do* state it.

No doc states a stronger guarantee (no "linearizable" or "any read sees
latest" claims) and no doc states a weaker one ("eventually consistent");
consistency.md §3.2 is explicit that linearizability is **not**
guaranteed.

**Recommended fix**: none required. Optional polish: paste the one-line
guarantee verbatim into HLD §1.3 and failure-model.md §1 so a reader
landing in either doc can see it without bouncing to consistency.md.

## Verified consistent

These properties were checked across all in-scope docs and found to agree:

- **Generation type is `u64` everywhere.** consistency.md §4.1 line 190
  (`pub generation: u64`), protocol.md `Attr.generation` (line 176) and
  `BlockKey.generation` (line 467) and `CloseReq.expected_generation`
  (line 367) — all `uint64`. LLD.md `Generation(pub u64)` (line 316). No
  `u32` slips found.

- **What carries the generation.** `Inode.generation` (LLD §4.1 line 683,
  consistency.md §4.1, HLD glossary). `BlockKey.generation` (protocol.md
  §2.2 line 467, LLD §3 line 341, used in WriteBlock and ReadBlock).
  `OpenResp.attr.generation` (protocol.md §2.1 line 355). `Attr` carries
  it on every meta response (protocol.md §2.1 line 176). No doc adds it
  to FUSE-side attrs improperly; HLD §4 line 100 explicitly notes "dtmpfs
  treats kernel caching as advisory; correctness comes from generation
  bumps", with no implication that `attr.generation` from the kernel
  (FUSE inode-reuse field) is the same as our `Inode.generation`. None of
  the docs conflate them.

- **Cache layers — exactly three.** consistency.md §6 enumerates three
  (kernel page cache + FUSE entry/attr cache; client AttrCache
  bypassed-on-open; client BlockCache LRU keyed on
  `(ino, gen, idx)`). HLD §2 glossary lists AttrCache and BlockCache;
  HLD §6.2 mentions both. LLD §6 lists the same two client caches and
  treats the FUSE kernel cache as the third (§6.6). architecture.md §4.5
  has the canonical three-layer ASCII box (lines 460–490). No doc adds a
  fourth or omits one.

- **Generation bump rule.** Every doc that states it agrees:
  - consistency.md §4.2 (line 203): "`Meta.Close` bumps `generation`
    **iff** the client's `CloseReq` reports a non-empty
    `written_block_idxs`."
  - protocol.md §2.1 (line 176, line 372): "Bumped by Meta.Close iff the
    close flushed dirty blocks." / "Empty list means no dirty blocks =>
    Close MUST NOT bump generation."
  - HLD.md §10.4 step 3 (line 463): meta only bumps when there are
    `written_idxs`.
  - LLD.md §4.4 (line 795–805): `if dirty { inode.generation =
    inode.generation.bump(); ... }`.
  - failure-model.md §4.3 (line 336): "The client did not bump
    generation (never reached `Meta.Close`)."
  - No doc bumps on read, no doc bumps on `write` (write is buffered, no
    RPC). The fsync issue under C1 changes *whether fsync triggers the
    Close*, not the rule that "publishing-via-Close + dirty" is what
    bumps.

- **Single-block atomicity at the store** (consistency.md §2.3 vs LLD
  §5.3). Consistent: DashMap insert is atomic per-bucket; readers see
  the entire old or new payload.

- **Generation as cache-coherence primitive (BlockCache key).** All four
  in-scope docs plus architecture.md describe it identically: client
  keys cache on `(ino, generation, block_idx)`; on close, the meta bumps
  generation; the next `Open` returns the new generation and old
  entries become unreachable in the cache key space. consistency.md §10
  states it as the second of three takeaways.

- **AttrCache TTL = `attr_cache_ttl_ms`, default 1000 ms, bypassed on
  `open`.** consistency.md §4.4.2 line 244–249, HLD glossary line 54
  ("short-TTL (1 s) per-client cache of `Attr` keyed by `ino`. Bypassed
  on `open`"), LLD §6 line 1432–1437 (`time_to_live(...)`), protocol.md
  Appendix D line 1060. All agree.

- **BlockCache: LRU, size-bounded, no TTL, capacity =
  `block_cache_capacity_mb` default 1024.** consistency.md §6.3 lines
  611–620, HLD §6.2 line 152, LLD §6.3 line 1439–1454, protocol.md
  Appendix D line 1062. All agree.

- **Per-file ordering of close events** (consistency.md §2.2 vs LLD §4.4
  RwLock-write critical section). Consistent.

- **Stale-writer/zombie-writer scenario outcome.** consistency.md §5.5
  vs failure-model.md §7.3 vs architecture.md §4.6.4 — all describe the
  same dual outcome (orphan blocks at the store + close fails with
  `FAILED_PRECONDITION`). The Phase-6 caveat about *when the store
  rejects* the WriteBlock vs accepts it is in C2 above; the close-time
  fail path is consistent across docs.

- **CAP stance.** HLD §6 NFR-avail-1 says CP. failure-model.md §1.2
  says CP. consistency.md §3.2 says "not linearizable" but does not
  contradict CP (CP is about partition behavior; linearizability is a
  stronger ordering property). No AP claim found anywhere.

- **Field numbers in `Attr` reserved for future generation-related
  fields** (protocol.md §7.1 line 794) — does not interact with the
  current generation field.

- **`O_APPEND` cross-host serialization.** consistency.md §5.10 / §7.1
  vs HLD §10.5 (silent on it). consistency.md is the only doc that
  takes a position; no contradicting doc.

- **Single-source-of-truth namespace** (HLD §10A.I1) — no contradiction.

- **Orphan-block GC on writer crash before close.** consistency.md §5.6
  vs failure-model.md §3.2 (paragraph at line 204) vs HLD §12. All
  agree: orphan blocks linger until Phase-6 GC; nightly sweep planned.

End of audit.
