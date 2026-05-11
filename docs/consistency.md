# dtmpfs Consistency Model — Deep Dive (v1)

This document is the normative reference for what dtmpfs v1 guarantees, what
it does *not* guarantee, and exactly which observable behaviors fall out of
the close-to-open model. It is paired with `docs/protocol.md` (the wire
definitions), `docs/failure-model.md` (fault categories), `docs/HLD.md`,
`docs/LLD.md`, and `docs/architecture.md`.

If you only have time to read one section: read Section 1 for the model,
Section 5.4 for the documented loss case, and Section 6 for cache trade-offs.

## 1. Model statement

> **dtmpfs provides close-to-open consistency, modeled on NFSv3.** After a
> successful `close()` on file F by client A, any subsequent `open()` of F
> by any client B observes the bytes A wrote.

Each clause matters; we define them in turn.

### 1.1 "successful close"

A close is **successful** when (a) every dirty block on the handle was
flushed (`Store.WriteBlock` returned OK on the primary, and on `R-1`
replicas if the client is configured to wait — v1 default is
primary-only); (b) `Meta.Close` was called with the correct
`expected_generation` (or its disjoint-block relaxation, see 5.3) and
the meta returned a `CloseResp`; and (c) the `close(2)` syscall
returned 0 to user-space.

If any step fails, the close is **not** successful and the visibility
guarantee does not apply. A close that returns 0 with only some blocks
flushed cannot occur by construction: the client only calls `Meta.Close`
after every `WriteBlock` returned OK. See Section 5.6 for crash
semantics.

### 1.2 "subsequent"

"Subsequent" is **causal**, not wall-clock. B's open is subsequent to
A's close iff B's `Meta.Open` arrives at the meta *after* A's
`Meta.Close` linearized into the meta's RwLock write history. Two
events the user thinks of as simultaneous can resolve in either order
at the meta; whichever wins the lock wins the ordering. Both are
correct.

### 1.3 "observes"

B "observes" A's bytes if a subsequent `read()` on the handle returned
by that `open()` returns A's bytes byte-for-byte, in the byte ranges A
overwrote, absent other writers. This includes cache-hit reads — the
cache is generation-keyed and B's open returned the new generation, so
old-generation entries are unreachable. The cache cannot mask a
successful close.

### 1.4 What's modeled on NFSv3 specifically

NFSv3's close-to-open is built around `GETATTR`-on-open invalidation:
opens fetch attrs and use mtime/size/change to decide cache staleness.
dtmpfs plays the same trick with an explicit `generation` integer
instead of `mtime` — same idea, no clock-skew complications. Like
NFSv3 (and unlike NFSv4), we provide no protocol-native locking; v4's
delegations require a state machine (callbacks, leases, revocation)
that we defer to Phase 8+.

## 2. What this guarantees

The guarantees below follow from the model and from the way `generation` is
threaded through the RPCs. Each is testable; see `docs/acceptance-tests.md`.

### 2.1 Cross-host visibility on close

After a successful close on host X, an `open` on host Y observes the
written bytes. No additional `sync`/`fsync` is required from the user. This
is the headline feature.

Concretely: the close RPC's commit point is when the meta releases its
RwLock write guard with the new `generation` installed. Any later `Open`
RPC takes the read guard, sees the new generation, and returns it.

### 2.2 Per-file ordering of close events

Every `Meta.Close` is serialized at the meta's RwLock, giving a total
order over closes for any single file. The first `Close` to land wins
the ordering point; its writes are present in the post-close attr and
its `generation` is what subsequent opens see (until the next close
bumps further). "First to land" is decided by the meta, not by which
client called it first; two concurrent closes from separate hosts
linearize in some order. Both succeed unless one carries a stale
`expected_generation` (Sections 5.4 / 5.5).

### 2.3 Single-block atomicity at the store

A single `Store.WriteBlock` is atomic: the store performs one
`DashMap::insert` of the new `Bytes` payload, guarded by a per-shard
RwLock. A concurrent `ReadBlock` sees either the entire old payload or
the entire new one — never a mixture. Mixing versions within a block
is impossible at the store layer.

### 2.4 Self-consistent reads

A single `read(file, off, len)` returns bytes from a single block image
at a single generation. The client fixes the generation at `open()`
time on its `OpenFile`, and every cache lookup and `ReadBlock` RPC for
that handle uses that fixed generation. We never serve a "Frankenstein"
read of block 0 at gen 7 stitched onto block 1 at gen 8.

### 2.5 Atomic rename and at-most-once block placement

`Meta.Rename` is atomic on the meta side (single RwLock write
critical section, both directories in the same `MetaState`).
`Meta.AllocateBlocks` is idempotent — repeated calls with the same
`(ino, block_idxs)` return the same `BlockLoc`s; placements are never
duplicated.

## 3. What this does NOT guarantee

### 3.1 Read-your-writes across hosts before close

If client A on host X writes to F and, before A calls close, client B on
host Y reads F, B will **not** see A's bytes. A's writes are buffered in
A's per-fd `dirty_blocks` map; nothing has gone over the wire to a store
yet, and A has not bumped the generation. B's `open()` returns the prior
generation's `block_map`; B's `read()` returns the prior generation's
bytes.

This is by design and is the entire reason the model is *close*-to-open
and not stronger. If you need read-your-writes across hosts mid-flush, use
a database — or wait for the per-file lease feature in Phase 8 (Section 9).

### 3.2 Linearizability

dtmpfs is **not** linearizable. Two reads from two clients concurrent with
a close can see different (but each internally consistent) views: the one
who reaches the meta before the close-write-guard wins sees gen N; the one
who reaches it after sees gen N+1. There is no operation order that
"explains" the system as if it were running on a single host.

### 3.3 Atomicity of multi-block writes

A logical `write(2)` of a multi-MiB buffer touches multiple blocks; we
do **not** write them atomically. If the client crashes after 3 of 5
dirty blocks but before `Meta.Close`, the meta still points at the
OLD generation; the 3 partial blocks are orphans (Section 5.6) and
GC'd. Observers on other hosts see the pre-write state because the
generation never bumped. The atomicity granularity is **the close** —
either the whole batch is published or none of it is.

### 3.4 POSIX file locking

`flock(2)`, `fcntl(F_SETLK)`, and mandatory locks are **not
implemented in v1**. FUSE locking handlers are not wired up; the
kernel falls back to host-local handling (host A's locks are invisible
on host B). Code depending on cross-host POSIX locking will misbehave
exactly as it did on NFS-without-NLM.

### 3.5 `mmap` writeback

`MAP_SHARED` writeback is **not supported in v1**. We don't implement
writepage; the kernel cannot push dirty pages back through the FUSE
channel. Reads via `mmap` work because the kernel falls back to FUSE
`read()`s, but writes through a mapping are visible only on the mapping
host until `msync` plus an explicit `write` forces them through.
`MAP_SHARED` writes not `msync`'d before unmount are **lost**. See
`docs/limitations.md`.

### 3.6 Hardlinks

Not supported. `link(2)` returns `EPERM`. `nlink` is therefore always 1
for files in v1.

### 3.7 Symlinks

Deferred to Phase 3. v1's `Inode` carries a `symlink_target: Option<String>`
slot for forward-compat, but the FUSE handlers for `symlink`/`readlink`
return `ENOSYS`.

### 3.8 Quotas, snapshots, ACLs, xattrs

None of these exist in v1. See `docs/limitations.md` for the full list.

## 4. The generation mechanism

This is the core mechanism that makes close-to-open visibility cheap and
correct. Read this section carefully.

### 4.1 Definition

```rust
pub struct Inode {
    // ...
    pub generation: u64,   // bumped on Close-with-dirty
    // ...
}
```

`generation` is a `u64` counter starting at 1 at inode creation. It is
**monotonically increasing**: never decremented, never reset. (Wraparound
is theoretically possible at 2^64 closes; if you flush 1 million times per
second it takes ~580 thousand years to wrap. We're not handling wrap.)

### 4.2 Where it bumps

`Meta.Close` bumps `generation` **iff** the client's `CloseReq` reports a
non-empty `written_block_idxs`. The meta's close handler takes the
state's RwLock for write, validates `expected_generation` (or applies
the disjoint-block relaxation from Section 5.3), then atomically:

- bumps `inode.generation += 1`,
- updates `inode.size = req.new_size`,
- installs `BlockPlacement`s for each written idx,
- sets `inode.mtime = chosen_mtime(req, now)` and `inode.ctime = now`,
- removes the open handle.

Every one of those mutations is under the **same write guard**. There
is no observable intermediate state where (e.g.) the new size is
published but the new block map is not.

### 4.3 Where it's visible

Every RPC that returns `Attr` returns the current generation:

- `Meta.Lookup`
- `Meta.GetAttr`
- `Meta.SetAttr` (return path)
- `Meta.Open`
- `Meta.Close` (return path)
- `Meta.Mkdir` (always 1 for new dirs; immutable)
- `Meta.Create` (always 1 for new files; immutable until first write+close)

### 4.4 Where it's used

#### 4.4.1 Client `BlockCache` keying

The block cache is a `moka::sync::Cache<(u64, u64, u64), Bytes>` keyed by
`(ino, generation, block_idx)`. When the client receives a new `Attr`
showing a higher `generation` for an inode, every cached entry under the
old generation becomes **unreachable**: nothing will look them up anymore.
The cache evicts them lazily as new entries fight for capacity. There is
no explicit invalidation step.

This is the magic. We don't broadcast "invalidate" messages; we simply
make the old data unreachable from the cache key space.

#### 4.4.2 Client `AttrCache`

The attr cache is keyed by `ino` and TTL-evicted at `attr_cache_ttl_ms`
(default 1 s). Crucially, `Open` **bypasses** the attr cache: every open
issues a fresh `Meta.Open`, which is what re-fetches the generation. This
is the close-to-open invalidation point. Routine `getattr` calls between
opens may still return a stale generation; the next open corrects it.

#### 4.4.3 `BlockKey.generation` in `WriteBlock`

The store records writes under the *writer's* generation. If a writer
opened at gen 7, it writes its blocks under gen 7. Two consequences:

1. A stale writer that hasn't noticed gen 8 exists can still complete
   writes at gen 7 — but those writes are recorded under a key
   `(ino, idx, 7)` that is distinct from the live `(ino, idx, 8)` data.
   The live data is unaffected. The stale data becomes a placement
   orphan and is GC'd.
2. The store's freshness check for refusing stale writes can use the
   `generation` field directly. If the store has already stored
   `(ino, idx, gen=N)` and gets a `WriteBlock` with `(ino, idx, gen=M)`
   where `M < N`, it returns `FAILED_PRECONDITION`. (The check is local
   to the store; it doesn't need a meta round-trip.)

#### 4.4.4 Diagnostics

Linux exposes `st_gen` via `statx(STATX_GEN)`. v1 does not surface this;
the FUSE protocol does not have a clean path for `STATX_GEN` and we'd
rather not piggyback. If users want to debug they can read the meta's
admin API (Phase 6).

### 4.5 What does NOT bump generation

- A `Close` with empty `written_block_idxs` (read-only open): no bump.
- `SetAttr`: bumps `ctime` but not `generation`. Truncate via SetAttr
  changes file size but not generation; observers re-fetch attr through
  the normal cache TTL path. **This is a deliberate design choice**:
  truncate is expected to be infrequent and the TTL re-fetch is fast.
- `Rename`, `Unlink`, `Rmdir`: do not affect any inode's generation other
  than possibly removing the inode entirely.
- Heartbeats, allocations: do not bump generation.

The only thing that bumps generation is "a writer published their work via
Close".

### 4.6 Why not use mtime?

NFSv3 conflates the close-to-open invalidation signal with mtime. That
works but is fragile under clock skew. With explicit `generation`:

- We never need to compare two clocks.
- We never have to worry about a clock running backwards.
- "Did anything change since I last looked?" reduces to integer compare.

The cost is one `u64` per inode and one field on every Attr response.
Worth it.

## 5. The race scenarios — exhaustive case analysis

Each scenario shows a time diagram with ASCII bars, then the observed
behaviour, then the reason it falls out of the model. Time flows
left-to-right; `|` events are server-observed wall-clock instants.

### 5.1 Sequential close-then-open across hosts

```
A:  open  --|  write  --|  close (gen 7 -> 8)
                                                B:  open (sees gen=8)  --|  read (sees A's bytes)
```

**Behaviour**: B sees A's bytes. ✓

**Why**: A's `Meta.Close` linearized at the meta and bumped the generation
to 8 before B's `Meta.Open` arrived. B's open response carries gen 8 and
the new `block_map`; B's reads either go to the store (which has the new
data because A's flush completed before close) or hit a cache that does
not have gen-8 entries yet (so it misses and goes to the store).

This is the headline guarantee. If this doesn't work, the system is broken.

### 5.2 Concurrent open, sequential close

```
A:  open(gen=7)  --|  write  --|  close (gen 7 -> 8)
B:  open(gen=7)  --|           |                       read  --|
```

This is the trickiest case. Walk through each piece:

1. Both A and B open. Both observe gen=7. Both cache attr+block_map.
2. A writes locally (no RPC).
3. A flushes. Each `Store.WriteBlock` carries `BlockKey{gen=7}`. The store
   accepts because no later generation has been recorded for those blocks
   yet. The store now has `(ino, idx, gen=7) -> A's new bytes`.
4. A closes. The meta lifts gen 7 → 8; placements installed; success.

Now what does B see when it reads?

There are two sub-cases:

#### 5.2.a B has the relevant block cached

B's `BlockCache` already has `(ino, 7, idx) -> old_bytes` from a prior
read in step 1's wake. B's read hits the cache and returns the *old*
bytes. B did **not** see A's writes. ✓ (per the model: B opened at gen 7;
that's the world B sees until B re-opens or `attr_cache_ttl` expires and
forces a re-Open).

#### 5.2.b B does not have the block cached

B's `OpenFile.generation` is still 7, so B issues
`Store.ReadBlock(BlockKey{gen=7, ...})`. Crucial fact: A's WriteBlock
recorded its bytes under exactly that key. The store does not GC on
generation bump — the entry it has is the bytes A wrote while opened at
gen 7, which the meta then *promoted* to gen 8 by installing placement.
So B's `ReadBlock(gen=7)` returns... A's new bytes. **Even though B
opened at gen 7, B sees A's data.**

That sounds wrong but is exactly the documented NFSv3 behaviour
(RFC 1813): the guarantee is one-way (close → visible to subsequent
opens), not two-way. If A wrote only block 1 and B reads block 0, B
sees pre-A bytes on block 0 — also fine, B's gen-7 snapshot is
consistent.

#### 5.2.c Bottom line for 5.2

- B never sees a stitched view.
- Whether B sees A's bytes depends on whether B's local cache had the
  block.
- This matches NFSv3 exactly.
- If your application cannot tolerate the ambiguity, force B to re-Open
  before the read (or set `attr_cache_ttl_ms = 0`; see Section 6).

### 5.3 Two writers, disjoint blocks, both close

```
A: open(7) --|  write block 0  --|  close (gen 7 -> 8)
B: open(7) --|  write block 1  --|                       close (gen 8 -> 9)
```

**Behaviour**: gen-9 inode points at A's block 0 and B's block 1. Both
writes survive. ✓

**Why**: A flushes block 0 at gen-7 keys, then closes (gen → 8). B
flushes block 1 at gen-7 keys, then closes — but B's
`expected_generation = 7` while `inode.generation = 8`. A strict check
would fail B with `FAILED_PRECONDITION`.

v1 therefore relaxes the close precondition: if B's expected gen is
older than current **but** B's `written_block_idxs` are disjoint from
what got installed since gen 7, the close still succeeds:

```rust
// in Meta.Close:
if inode.generation != req.expected_generation {
    let installed_since = inode.recent_writes_since(req.expected_generation);
    let mine: HashSet<u64> = req.written_block_idxs.iter().copied().collect();
    if !mine.is_disjoint(&installed_since) {
        return Err(Status::failed_precondition("stale close: block conflict"));
    }
    // else: proceed; bump gen by 1 from current, install our blocks.
}
```

`recent_writes_since` reads from a small per-inode ring buffer
(`recent_block_writes: VecDeque<(gen, Vec<idx>)>`, capped at 16
entries). Closes referring to transitions older than the ring's oldest
entry are rejected as too stale.

B's close bumps gen 8 → 9 and installs block 1 placement. ✓

### 5.4 Two writers, overlapping block, both close (the loss case)

```
A: open(7) --|  RMW block 0 --|  close (gen 7 -> 8)
B: open(7) --|  RMW block 0 --|                      close
```

**Behaviour (v1 default)**: A's close succeeds; B's close fails with
`FAILED_PRECONDITION` (because the conflict check in Section 5.3 finds
block 0 in `installed_since`). User-visible:

- A: `close()` returns 0; A's bytes survive.
- B: `close()` returns -1, errno `EIO`. B's writes are lost; user must
  reopen and retry.

Both RMW'd from the same gen-7 image; B's flush had clobbered A's flush
at the store level (DashMap `insert` replaces) before A's close, but
A's close was already in flight at the meta and won the meta's lock
race. The block content at gen 7 in the store is now whatever B wrote;
A's close promoted that content to gen 8 anyway. Actually verifying
which bytes ended up in the published gen-8 image requires looking at
the relative wall-clock order of A's WriteBlock and B's WriteBlock at
the store, not just the meta close order. *Either way*, B's close is
rejected so B knows its writes are not the published version.

This is **better** than NFSv3 — NFSv3 silently loses one writer's data.
dtmpfs surfaces the conflict to whichever close-loser arrives second.
The title "loss case" is retained because *somebody's* bytes never
reach the published gen-8; the question is who and whether they know.

For NFS-classic last-close-wins compatibility, the operator can set
`meta.allow_overwriting_close = true` in `meta.toml`. v1 default is the
safer option above.

### 5.5 Stale writer

```
A:  open(7) --|  write --|  (network stalls)  ...           (resumes) --|  flush --|  close
B:                                  open(7) --|  write --|  close (gen 7 -> 8)
```

A pauses mid-flush. B opens, writes, closes; gen → 8. A's network then
recovers and its flush + close finally land.

The store's freshness check uses the generation embedded in BlockKey.
The store keeps a per-`(ino, idx)` highest-seen-gen counter. Two sub-policies:

- **Strict (v1 default)**: a `WriteBlock` whose generation is less than
  the highest seen for that `(ino, idx)` is rejected with
  `Status::failed_precondition("stale generation")`. A's `WriteBlock`
  fails before it ever touches the DashMap.
- **Lenient**: writes at the *same* generation are accepted (last write
  to the slot wins). A's write succeeds and clobbers B's.

In both modes A's `Meta.Close{expected_generation=7}` then fails:
`inode.generation == 8` and block 0 is in `installed_since` → 
`FAILED_PRECONDITION`. Any data A's write managed to leave at the
store under gen 7 is an **orphan** (no inode points at it) and is
reclaimed by GC.

Strict is the default because it surfaces the staleness during the
WriteBlock RPC instead of waiting for close, and it prevents A's bytes
from temporarily corrupting block 0 between A's WriteBlock and A's
failed Close.

### 5.6 Writer crash before close

A's process dies after writing 3 of 5 dirty blocks, before calling
`Meta.Close`.

```
A:  open(7) --|  WriteBlock idx=0 --|  WriteBlock idx=1 --|  WriteBlock idx=2 --|  *crash*
                                                                                       (no Close)
```

**Behaviour**:

- The meta still believes the inode is at gen 7. No bump.
- The store has `(ino, 0, gen=7)`, `(ino, 1, gen=7)`, `(ino, 2, gen=7)`
  entries holding A's partial write.
- The live inode's block map at gen 7 still points to whatever was there
  before A opened (the "before" state). Critically, those placements may
  be at different keys — a previously-untouched file's blocks haven't
  been written yet, so there are no `(ino, 0, gen=7)` entries from
  before; A's writes are the *first* such entries. They simply lie
  around unreferenced.
- Observers: any subsequent `Open` returns gen 7 with the pre-A inode's
  block map; reads return pre-A bytes. **A's partial state is invisible.**

The orphaned blocks are reclaimed by **nightly GC** (Phase 6): the meta
walks all inodes, builds a set of in-use `(ino, idx, gen)` keys, and the
GC sweeper asks each store to delete keys it holds that are not in the
in-use set. Until GC runs, the orphans waste RAM but cause no
correctness problem.

Inode generation stays at 7. ✓

This is the "atomicity granularity is the close" property in action.

### 5.7 Reader during writer's flush

```
A:  open(7) --|  WriteBlock idx=0 --|  WriteBlock idx=1 --|  close (gen 7 -> 8)
B:                                          open(7) --|  read block 0 --|
```

B opens *during* A's flush. A has not yet called Close; meta is still at
gen 7. B's open returns `Attr{generation:7}` and the gen-7 block_map.

B's read of block 0 issues `Store.ReadBlock(BlockKey{gen=7, idx=0})`.
Because A's WriteBlock has already overwritten that DashMap entry, B may
read A's mid-flush bytes. Is that a problem?

**No.** B sees either entirely A's new bytes or entirely the pre-A
bytes (DashMap insert is atomic), never a mix. Both are valid
gen-7-snapshot answers because A's RMW preserves bytes outside its
overwritten range — A read the gen-7 image, modified some bytes, wrote
the result back. So any byte position B reads is byte-for-byte equal to
some legitimate gen-7 content.

The only subtlety is the tail block when A is *growing* the file.
Suppose pre-A `size = 5.0 MiB` and A wants to grow to 5.5 MiB. A's RMW
on block 4 reads the existing 1 MiB block, modifies bytes
`[5 MiB, 5.5 MiB)`, writes the full 1 MiB back. Meta still says
`size = 5 MiB` until A's close. B reads block 4 of a 5-MiB file:
client truncates the read at `size = 5 MiB`, so B never sees A's
trailing not-yet-published bytes. Self-consistent. ✓

**General principle**: writers always RMW from a consistent snapshot,
so the byte ranges B can legitimately read at gen 7 are byte-for-byte
the same as before A started writing. This is why we don't bump
generation until close.

### 5.8 Writer crash mid-WriteBlock

```
A:  open(7) --|  WriteBlock idx=0 (in flight) --| *crash*
```

The store may have received a partial RPC. tonic's HTTP/2 layer requires
the entire DATA framing to be received before the unary handler is
invoked, so a partial RPC is *not* surfaced to the handler — it is
discarded at the transport layer when the connection drops. The store's
DashMap is not touched. ✓

If the connection survives the crash long enough for the RPC to complete
(rare), the store will record the data — and we're back in 5.6.

### 5.9 Reader crash

No consequences. The reader's open handle leaks at the meta until
client-side heartbeats notice (v1 does not implement them — Phase 6) or
until reconnect. Leaked handles cost a small HashMap entry each.

### 5.10 Two appenders (`O_APPEND`)

```
A:  open(O_APPEND, gen=7) --|  GetAttr(size=N)  |  write@N --|  close
B:  open(O_APPEND, gen=7) --|  GetAttr(size=N)  |  write@N --|  close
```

Both observe `size = N`, both write at offset N. At close time the
second close hits the conflict check (Section 5.4) and fails. **v1 does
not serialize O_APPEND across hosts.** Don't use `O_APPEND` for
cross-host log writers — explicit non-goal, see Section 7.

## 6. Cache TTL trade-offs

The two TTLs that matter to consistency are `attr_cache_ttl_ms` and the
implicit "lifetime of an open" for the BlockCache. Trade-offs below.

### 6.1 `attr_cache_ttl_ms`

Default: `1000` (1 second). Controls how long the client caches `Attr`
responses in its `AttrCache`. Also pushed to the kernel as the FUSE
`attr_timeout`.

| Value         | Effect on staleness                 | Effect on meta load                          |
|---------------|-------------------------------------|----------------------------------------------|
| 0 (no cache)  | `ls -la` always sees latest         | every `getattr` hits meta — high QPS         |
| 100 ms        | `ls` is at most 100 ms stale        | up to 10 getattr/sec/file                    |
| 1000 ms (def) | `ls` is at most 1 s stale           | 1 getattr/sec/file under steady poll         |
| 60000 ms      | `ls` can be a minute stale          | very low                                     |

`open()` always bypasses the cache (re-issues `Meta.Open`), so the
attr cache never affects correctness *within* the close-to-open
guarantee. It affects the staleness of `stat`/`fstat` in between opens.

### 6.2 `entry_timeout` (FUSE kernel name cache)

Default 1 s (mirrors `attr_cache_ttl_ms`). Caches `name → inode`
bindings in the kernel. Long: snappy `ls`, but cross-host rename/delete
can leave this host reaching for an old inode for up to `entry_timeout`
before the meta-side `ENOENT` triggers a re-lookup. Short: every name
resolution is a meta round-trip.

### 6.3 BlockCache: no TTL, only LRU + generation invalidation

`moka` cache with capacity `block_cache_capacity_mb` (default 1024).
LRU + size-based eviction; **no TTL**. The key includes `generation`,
so a close that bumps generation makes every old-generation entry
unreachable — no background invalidation needed. Unreachable entries
linger in the LRU until something fresher evicts them; only-cost-RAM,
zero-cost-correctness.

Rename keeps the inode, so cache keys stay valid across rename.
`unlink` + `creat` of the same name allocates a fresh inode (no inode
recycling within a meta run); old `(ino, gen, idx)` keys are simply
orphaned and age out.

### 6.4 Recommended tunings

| Workload                                         | `attr_cache_ttl_ms` | `entry_timeout` | `block_cache_capacity_mb` |
|--------------------------------------------------|---------------------|------------------|----------------------------|
| Build cache (write-once, many reads)             | 5000                | 5000             | 8192                       |
| Cross-host log tail                              | 100                 | 100              | 256                        |
| ML scratch (large sequential read/write)         | 1000                | 1000             | as much RAM as you can     |
| Many tiny files (`make`, `find`)                 | 1000                | 1000             | 1024                       |

## 7. POSIX corner cases

A precise account of how each POSIX file-API peculiarity is handled.

### 7.1 `O_APPEND`

Client calls `Meta.GetAttr` to discover current size, then writes at
`size`. There is no atomic "append" at the wire level. Race: two
appenders observe the same `size = N`, both write at offset N; the
second close hits the conflict check (Section 5.4) and fails. **v1 does
not serialize O_APPEND across hosts.** Document loud and clear.

### 7.2 `O_DIRECT`

Honoured by setting `FOPEN_DIRECT_IO` in `OpenResp.flags` so the kernel
skips its page cache. Does **not** change consistency — the data path
is the same close-to-open one. `O_DIRECT` is a performance lever, not a
consistency lever.

### 7.3 `O_TRUNC`

Kernel forwards `O_TRUNC` as a `SetAttr(size=0)`. The meta:

1. Async-fires `Store.DeleteBlock` to each placement (best-effort
   cleanup, doesn't block `open()`).
2. Sets `inode.size = 0`, clears `inode.blocks`, updates timestamps.
3. Does **not** bump `generation` (SetAttr never does).

A reader caching `(ino, gen=7, idx=k)` may briefly hold stale block
bytes after a cross-host truncate; within `attr_cache_ttl_ms` (1 s
default) the next getattr corrects it. Accepted as part of
close-to-open semantics.

### 7.4 `O_SYNC`

Maps each FUSE `flush` to a full flush+close+re-open sequence. RPC per
write — kills throughput. Documented perf cost; users opting into
`O_SYNC` know what they're asking for.

### 7.5 `fsync(2)` (without `O_SYNC`)

Same as `flush`: emit pending writes, do **not** call `Meta.Close`.
`fsync` makes the data durable on the **primaries** but does **not**
publish a new generation; other hosts still see the pre-fsync state.
Matches NFSv3's `fsync`. To wait for replicas, use a future
`client.fsync_replicas = true` toggle (not wired in v1; TODO).

### 7.6 `rename(2)`

Atomic on the meta side: `Meta.Rename` takes the RwLock for write and
swaps dirents in one critical section. Cross-directory rename works
because both dirs are in the same `MetaState`. Error mapping for
`dst`-exists cases follows POSIX (`EEXIST`, `ENOTEMPTY`,
file-replaces-file allowed).

### 7.7 Hardlinks

Not supported. `link(2)` returns `EPERM` (POSIX-permitted refusal;
tools handle it more gracefully than `ENOSYS`).

### 7.8 Symlinks

Deferred to Phase 3. `symlink(2)` and `readlink(2)` return `ENOSYS`.

### 7.9 Timestamps

- `mtime`: updated on every metadata-mutating Meta call that affects the
  file's content (Close-with-dirty, SetAttr-with-utimens, Create,
  Mkdir).
- `ctime`: updated on **any** inode mutation including chmod, chown,
  rename, link-count changes.
- `atime`: read-time-updated by POSIX, but suppressed via
  `MountOption::NoAtime` to avoid write amplification (every `read`
  becomes a meta RPC).

Operators wanting `atime` updates can mount without `NoAtime`; v1 will
honour it but at significant meta-load cost. We do not implement
`relatime` heuristics.

### 7.10 Sparse files

`SetAttr(size = N)` where N exceeds the current size creates a hole.
Reads of the hole return zeros without contacting any store. Writes into
the hole follow the standard path: `Meta.AllocateBlocks` for the new
indices, then `Store.WriteBlock`.

### 7.11 `statfs(2)`

Returns aggregated cluster stats: total capacity = sum of stores'
configured budgets; free = sum of (capacity - used); files = inode
count from the meta. Cached in the client for 5 s to avoid hammering the
meta on `df` polls.

## 8. Compared-to table

A quick orientation for engineers familiar with other shared filesystems.

| Property                | NFSv3                | dtmpfs v1              | NFSv4                          | Ceph (CephFS)                         |
|-------------------------|----------------------|------------------------|--------------------------------|---------------------------------------|
| Consistency model       | close-to-open        | close-to-open          | open-to-close + delegations    | strong via OSD locks + capabilities   |
| Cross-host visibility   | on close             | on close               | proactive via callbacks        | immediate                             |
| Locking                 | NLM sidecar          | none                   | mandatory native               | RADOS locks + MDS capabilities        |
| Caching                 | attr/data TTL        | attr/data TTL + gen    | delegation-driven              | various; capability-driven            |
| Replication             | none                 | configurable R         | none (server's job)            | configurable; via OSDs                |
| Backing store           | disk                 | RAM                    | disk                           | disk / NVMe                           |
| Auth                    | AUTH_SYS / Kerberos  | shared cluster token   | AUTH_SYS / Kerberos / RPCSEC   | CephX                                 |
| `mmap` writeback        | yes                  | no (v1)                | yes                            | yes                                   |
| Hardlinks               | yes                  | no (v1)                | yes                            | yes                                   |
| Symlinks                | yes                  | no (v1)                | yes                            | yes                                   |
| `atime` semantics       | configurable         | NoAtime by default     | configurable                   | configurable                          |
| Linearizability         | no                   | no                     | no (yes-with-lease)            | yes                                   |
| Single-host throughput  | NIC-bound            | NIC-bound              | NIC-bound                      | NIC-bound                             |
| Failure domain          | server               | meta node              | server                         | OSDs + MDSs                           |
| Power-loss durability   | yes (disk)           | NONE (RAM only)        | yes                            | yes                                   |

dtmpfs is in the same consistency family as NFSv3, with mechanically
better internals (explicit generation, atomic store inserts, conflict
detection at close).

## 9. Future directions for stronger consistency

Post-v1 paths.

- **Read-through-meta strong consistency**: an `Inode.flags` bit
  `STRONG`; `Open(STRONG)` returns no `block_map`; reads hit a new meta
  RPC that proxies the block. Linearizable for the marked file but
  meta-bottlenecked. Phase 8+.
- **Leases / delegations** (NFSv4-style): client gets an exclusive
  lease, reads/writes locally with no per-op RPC, lease revoked by the
  meta on another client's open. Requires a second gRPC service from
  meta to client (callbacks), a lease state machine, and lease records
  on the meta. Phase 8+.
- **Raft for meta**: doesn't change the consistency model, only
  availability. Clients re-target a leader on `UNAVAILABLE`. Phase 7;
  see `docs/HLD.md` and `docs/operations.md`.
- **CRDT-like merge for concurrent writes**: irrelevant for byte
  arrays — bytes have no commutative operation other than last-write-
  wins, which is what we already do. Not pursued.
- **Op-based replication**: replicate operations (ranges) instead of
  bytes. Makes replicas concurrent rather than primary-with-pull.
  Significant complexity for marginal gain. Not pursued.

## 10. Summary

If you remember three facts about dtmpfs consistency:

1. **Close publishes; nothing else does.** Until you call `close()` on a
   file, no other host sees your writes. After a successful `close()`,
   every subsequent `open()` on every other host sees them.
2. **`generation` is the cache-coherence primitive.** It bumps exactly
   once per close-with-dirty, under the meta's RwLock. The client's
   block cache is keyed by `(ino, gen, idx)`, so generation bumps are
   self-invalidating.
3. **Concurrent writes to overlapping blocks lose the loser.** Either
   silently (NFS-classic, opt-in) or noisily with a `FAILED_PRECONDITION`
   close (default). Plan your application around it: write to disjoint
   files or disjoint blocks, or use the upcoming lease feature.

Cross-references: `docs/protocol.md` for the wire definitions, `docs/HLD.md`
and `docs/architecture.md` for the layout, `docs/failure-model.md` for the
crash and partition matrices, `docs/operations.md` for runbooks,
`docs/configuration.md` for tunable keys, `docs/testing.md` and
`docs/acceptance-tests.md` for how to verify these properties.

End of consistency model deep-dive.
