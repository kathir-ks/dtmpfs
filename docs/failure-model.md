# dtmpfs — Failure Model

This document enumerates the ways dtmpfs can fail, what each failure looks
like to a user holding a file descriptor, what the cluster does about it on
its own, and what an operator has to do to put things back. The design-side
counterpart is [`HLD.md`](HLD.md) §9 and [`consistency.md`](consistency.md).

---

## 1. Stance

dtmpfs v1 is a deliberately minimal distributed filesystem for a **trusted
LAN**, a **small operator team**, and a **regenerable workload**. Four
choices shape every entry below:

1. **Simplicity over availability.** The metadata service is a single
   process. No leader election, no log replication, no failover. If
   `dtmpfs-meta` is dead, the filesystem is dead.
2. **CP under partitions.** When the network splits, the side that contains
   `dtmpfs-meta` continues to operate (subject to which stores it sees).
   The other side returns `EIO`. We never serve metadata writes from two
   nodes.
3. **R=1 is data loss on store death.** The default replication factor is
   one. R=2 and R=3 are available as configuration; R=1 matches the
   "regenerable scratch" use case.
4. **No durability.** dtmpfs is RAM-only. `fsync(2)` is not a barrier
   against power loss; it is only a barrier against close-to-open
   visibility.

Raft, persistent metadata, and online recovery are explicitly deferred to
Phase 7+.

---

## 2. Fault domains

Two faults in the same domain (e.g. two processes on the same VM) are
**correlated**; two faults in different domains are **independent**.

### 2.1 Process-level

Three roles run as separate processes: `metasrv`, `storesrv` (one per
store), `dtmpfs-mount` (one per mount). Each can crash independently:
SIGSEGV, panic, OOM-kill, `kill -9`, or even orderly SIGTERM (peers see
only TCP RST or a heartbeat gap).

### 2.2 Host-level

A single VM can die — kernel panic, hypervisor eviction, lost NIC. Every
process on that VM dies together. Co-located meta+store: both go.

### 2.3 Network

- **Clean partition.** A subset of nodes can no longer reach the
  complement; each side internally healthy. Modeled as a hard cut.
- **Soft degradation.** Latency rises, packet loss climbs but stays
  below 100%. RPCs time out intermittently; heartbeats may be missed in
  bursts. Harder to diagnose because the system flaps.

### 2.4 Data corruption

dtmpfs v1 has **no application-layer checksum**. We rely on:

- TCP's 16-bit checksum (weak but adequate for a trusted LAN).
- HTTP/2 framing.
- protobuf length-prefixing.
- The trusted-LAN assumption: no malicious or hardware-broken node in path.

Enough for a quiet 1/10 GbE LAN with ECC RAM, not enough for cosmic-ray
protection across many bytes. Phase 6 adds xxhash64 per block. Until then,
**silent corruption is theoretically possible and undetected**.

### 2.5 Operator error

Misconfiguration is its own fault domain. Common shapes in §6: mismatched
`cluster_token`, wrong `meta_addr`, duplicate `node_id`, port collisions,
double-mount on the same path.

### 2.6 Resource exhaustion

RAM full → writes fail. Secondary axes — file descriptors, FUSE kernel
queue, inode count — covered in §5.

---

## 3. Single-fault scenarios

For each scenario we list **detection** (how the cluster notices), **blast
radius** (which operations fail and from where), **user-visible behavior**
(what the application sees as `errno` or text), **automated recovery** (what
the cluster does without an operator), and **manual recovery** (the runbook).

### 3.1 Single store node crashes

#### Detection

The store sends `Meta.HeartbeatNode` every `heartbeat_interval_ms`
(default 1000 ms). Meta marks the node `Down` and removes it from the
rendezvous ring if `now − last_seen > heartbeat_timeout_ms` (default
5000 ms). Clients independently see in-flight `Store.ReadBlock` /
`WriteBlock` return `Status::unavailable` (transport error /
connection refused) within the RPC deadline.

#### Blast radius

- **R=1:** every block whose primary was the dead store is unreadable —
  roughly `1/N` of cluster bytes with even distribution.
- **R=2 / R=3:** another copy exists, but **in v1 the client only contacts
  the primary**, so a primary death looks the same as R=1 until Phase 6
  ships replica-failover on read.

Blocks not owned by the dead node are unaffected (both read and write).

#### User-visible behavior

- `read(2)` returns -1, `errno == EIO`.
- `cat /mnt/dtmpfs/affected-file` → `Input/output error`.
- `write(2)` returns 0 (kernel buffered into client RAM); the next
  `flush`/`close` returns `EIO`. Dirty blocks remain in
  `OpenFile.dirty_blocks` and retry on the next flush.

#### Automated recovery

None in v1. Meta records the membership change but does **not** trigger
re-replication. Client does **not** retry against a replica (Phase 6 adds
this). No alerting fires; logs only.

#### Manual recovery

```
# 1. Confirm the store is actually down.
ssh store-1
ps -ef | grep storesrv
journalctl --user -u dtmpfs-store@1 -n 50

# 2. Restart it. The new process starts empty (RAM-only).
systemctl --user start dtmpfs-store@1
journalctl --user -u dtmpfs-store@1 -f

# 3. Verify it rejoined.
#    On the meta host:
RUST_LOG=debug journalctl --user -u dtmpfs-meta -n 200 | grep -i 'node store-1'
# Expected: "node store-1 marked Up after heartbeat".

# 4. R=1: data on store-1 is permanently lost; user-space apps must
#    regenerate. R≥2: Phase 6 will re-replicate; v1 leaves the surviving
#    replica as the only copy.
```

If the storesrv process is panic-looping on every restart, capture
`journalctl --user -u dtmpfs-store@1 --since '5 min ago'` and treat as a
software bug.

### 3.2 Meta crashes

The worst v1 single-fault scenario.

#### Detection

Clients see `Status::unavailable` (TCP RST after a fast crash) or
`Status::deadline_exceeded` (slow death) on the next `Meta.*` RPC. First
sign is usually `lookup`/`getattr` failing. Stores see their next
`HeartbeatNode` fail and retry with exponential backoff capped at
`heartbeat_interval_ms × 8`.

#### Blast radius

Total. Every meta-touching operation fails: `lookup`, `getattr`, `setattr`,
`mknod`, `create`, `unlink`, `mkdir`, `rmdir`, `rename`, `opendir`,
`readdir`, `open`, `release`, `flush`, `fsync`, `statfs`, and any
`read`/`write` that needs a fresh `Open`. Stores still hold their blocks
but they are unreachable without meta to resolve paths.

#### User-visible behavior

- New `open(2)` returns -1, `errno == EIO`.
- `ls /mnt/dtmpfs` → `ls: reading directory '/mnt/dtmpfs': Input/output
  error`.
- Already-open fds with fully-cached state may serve cached reads briefly,
  but in practice the cache covers little; misses and flush both `EIO`.

#### Automated recovery

None in v1. The client propagates `EIO` immediately; Phase 6 adds
exponential-backoff retry on `Status::unavailable`.

#### Manual recovery

```
# 1. Confirm meta is down.
systemctl --user status dtmpfs-meta
ss -tnlp | grep 7100        # should show no listener

# 2. Restart it.
systemctl --user restart dtmpfs-meta
journalctl --user -u dtmpfs-meta -f
# Expected log lines: "meta server bound :7100", "ring size: 0 nodes".

# 3. Stores will re-register on their next heartbeat (≤ heartbeat_interval_ms).
#    Watch the meta log for "node store-0 marked Up", "node store-1 marked Up".

# 4. **The filesystem is now empty.** Every inode and directory entry was held
#    in `MetaState` and is gone. The stores still hold blocks — those blocks
#    are now orphans (no inode references them). Phase 6 GC will sweep them;
#    in v1 they consume RAM until the store restarts.

# 5. Bounce clients to clear stale FH/inode cache:
ssh client-a fusermount3 -u /mnt/dtmpfs
ssh client-a systemctl --user restart dtmpfs-client
```

This is by design — the single largest known v1 limitation. Phase 7 fixes
it with Raft + WAL. Until then, **do not store anything in dtmpfs that you
cannot regenerate**.

### 3.3 Client (FUSE mount) crashes

#### Detection

The Linux kernel notices `/dev/fuse` connection drop when `dtmpfs-mount`
exits (clean or via signal).

#### Blast radius

The single mount on the single host. Other mounts on other hosts continue.
The cluster does not notice; per-fd handles on meta leak until meta
restart (Phase 6 adds `Meta.CloseHandle` cleanup).

#### User-visible behavior

- Every syscall against `/mnt/dtmpfs` returns -1, `errno == ENOTCONN`
  ("Transport endpoint is not connected").
- `ls /mnt/dtmpfs` → `ls: cannot open directory '/mnt/dtmpfs': Transport
  endpoint is not connected`.

#### Automated recovery

With `MountOption::AutoUnmount` (default; see
[`configuration.md`](configuration.md) §5), libfuse cleans up the kernel
mount on connection drop. The client does not self-restart; the systemd
unit's `Restart=on-failure` brings it back within `RestartSec=2s`.

#### Manual recovery

```
# 1. Force-unmount if AutoUnmount didn't fire.
fusermount3 -u /mnt/dtmpfs
# If that fails ("device is busy"):
fuser -km /mnt/dtmpfs        # SIGKILLs every process holding the mount
fusermount3 -u /mnt/dtmpfs
# As a last resort:
sudo umount -l /mnt/dtmpfs   # lazy unmount

# 2. Restart.
systemctl --user start dtmpfs-client
mount | grep dtmpfs          # expect: dtmpfs on /mnt/dtmpfs type fuse...

# 3. Check why it died.
journalctl --user -u dtmpfs-client -n 200
# Common causes:
#   - SIGKILL by OOM killer (look in dmesg for "Killed process .* dtmpfs")
#   - panic from an unwrap (capture the panic line)
#   - meta unreachable at startup → exit 1 (see §3.2)
```

A client crash never loses already-flushed data. It can lose **dirty
buffered writes** that hadn't reached `Meta.Close` — see §4.3.

### 3.4 Network partition (clean split)

A single bisection: cluster splits into subsets A and B, no traffic crosses,
each side internally healthy.

#### Detection

- **Side without meta:** clients fail next meta RPC with
  `Status::unavailable`; stores fail next `HeartbeatNode`.
- **Side with meta:** meta marks the other side's stores `Down` after
  `heartbeat_timeout_ms`. Reads for blocks on the partitioned-off stores
  see `Status::unavailable` as in §3.1.

#### Blast radius

- **Side WITH meta** is "good": serves metadata; serves data for blocks
  whose primary is on this side. Blocks on the other side `EIO` (R=1) or
  succeed via replica (R≥2 + Phase-6 client + replica on this side).
- **Side WITHOUT meta** is dead — every op `EIO`s. Stores on this side
  hold their blocks but no client can resolve them.

Only one side survives. There is no quorum. That is what "CP under
partition" means in v1.

#### User-visible behavior

Identical to §3.1 on the meta side; identical to §3.2 on the no-meta
side. The halves do not exchange data and do not drift further.

#### Automated recovery

On heal: heartbeats resume; meta marks the previously-Down stores `Up`
on the next successful heartbeat; clients' next RPC succeeds. **No
re-replication runs** — writes only happened on the meta-side (the
no-meta side could not write), so nothing to reconcile.

#### Manual recovery

None required after heal. Bounce stuck clients via §3.3 procedure if
their cached state is stale.

---

## 4. Multi-fault scenarios

These are scenarios where two or more independent faults coincide.

### 4.1 Two stores die with R=2

With 4 stores and R=2, a block has two placements. Of C(4,2) = 6 store
pairs, each pair owns roughly 1/6 of cluster bytes as both-replicas with
even distribution. Killing two of four stores loses ~**17%** of cluster
bytes; the remaining ~83% stays accessible. Behavior on doomed blocks is
identical to §3.1 R=1 (`EIO` on read; `EIO` on flush of any targeting
write).

### 4.2 Meta + a store die

When meta restarts, every inode is gone (§3.2), so "which blocks did the
store owe us" is moot — no inodes refer to those blocks. The orphans on
surviving stores are reclaimed on the next store restart (or by Phase-6
GC). Runbook: do §3.2 then §3.1; order does not matter.

### 4.3 Network partition during write

A client mid-flush sees N successes, M `Status::unavailable`. `flush()`
returns `EIO`; `close(2)` returns `EIO`; `dirty_blocks` is NOT cleared.
The client did not bump generation (never reached `Meta.Close`), so
`BlockCache`/`AttrCache` are not invalidated.

Application options:

- **Retry `fsync`/`close` on the same fd:** client retries dirty blocks.
  Partition healed → retry succeeds. Still partitioned → `EIO` again.
- **App exits / client crashes:** in-RAM dirty buffers drop; data lost.

This is a deliberate v1 simplification — we do not journal writes
anywhere persistent; the app must retry or accept loss.

### 4.4 Cascading store deaths

Three stores, R=1, even distribution, killed one by one: first loses
1/3 of bytes, second loses another 1/3, third loses everything left. No
rebalance, no re-replication. The argument for background re-replication
(Phase 6); v1 requires an operator to notice and react before the next
death.

---

## 5. Resource exhaustion

### 5.1 Store RAM budget hit

storesrv tracks `ram_used` and refuses writes when `ram_used + req.data.len()
> ram_budget_bytes`. `WriteBlock` returns `Status::resource_exhausted`
("ram_budget exceeded"); store logs WARN. Client maps this to `errno ==
ENOSPC`:

- `write(2)` returns 0 (kernel buffered).
- `flush`/`close` returns -1, `errno == ENOSPC`.
- `df -h /mnt/dtmpfs` shows free near zero (statfs aggregates per-store).

#### Manual recovery

```
# Identify full store(s).
for s in store-0 store-1 store-2; do
  curl -s http://$s:7300/debug/blocks | jq '.ram_used'
done

# Free space (only v1 mechanism):
rm -rf /mnt/dtmpfs/old-checkpoints/

# Or raise budget on every store and roll-restart:
sudoedit ~/.config/dtmpfs/store.toml         # bump ram_budget_bytes
systemctl --user restart 'dtmpfs-store@*'    # data on each store is lost (R=1)

# Or add a new store. Just start it with the same cluster_token; it
# heartbeats in. NOTE: existing blocks do NOT rebalance in v1; only new
# writes use the new node.
```

### 5.2 File descriptor limits (client side)

The client opens TCP connections to every store and an `OpenFiles` entry
per fd. Limits that bite first:

- Process `RLIMIT_NOFILE` (default 1024; systemd unit sets 65535).
  Exhaustion: tonic connection-pool expansion fails;
  `Status::unavailable` "Too many open files".
- System-wide `fs.file-max`. `dmesg`: `VFS: file-max limit ... reached`.

#### Recovery

```
cat /proc/$(pgrep dtmpfs-mount)/limits | grep open
# If too low: bump LimitNOFILE in the unit and restart.
sudo sysctl -w fs.file-max=2097152            # system-wide
```

### 5.3 FUSE kernel queue full

The per-mount FUSE kernel queue can fill under extreme concurrency.
Symptom: `read`/`write`/`getattr` parked in `D` state in
`fuse_request_alloc`. The client process is CPU-bound; the kernel waits.

There is no userspace knob to enlarge the queue. Mitigations: raise
`fuse_threads` (default 4 → 16/32) and reduce application concurrency.
Rare on a healthy cluster; routine hits suggest a hot lock in the client.

### 5.4 Inode count

`MetaState.inodes` is unbounded. ~200 B per empty file plus size of
`blocks: BTreeMap<...>` per regular file. Soft limit ~200M inodes on a
200 GB-RAM meta host before OOM-killer fires.

#### Symptom

meta killed by OOM killer (`dmesg | grep 'killed process.*metasrv'`).
Recovery is §3.2 (meta crash) including the data-loss caveat.

#### Mitigation

```
pidstat -r -p $(pgrep metasrv) 5      # watch RSS
# Alert at >80% of host RAM. There is no online compaction; restart of
# meta loses everything (§3.2).
```

---

## 6. Operator-error scenarios

### 6.1 Wrong `cluster_token`

Every RPC carries the token in metadata header `cluster-token`. Mismatch
returns `Status::unauthenticated`.

**Symptom:** mount comes up but every op `EIO`s. Client log: `Status {
code: Unauthenticated, message: "cluster token mismatch" }`. Meta log:
`"reject RPC from <peer>: bad cluster_token"`.

**Fix:**

```
grep cluster_token ~/.config/dtmpfs/{meta,store,client}.toml
# Align all to a single 16+-char value, then:
systemctl --user restart dtmpfs-meta 'dtmpfs-store@*' dtmpfs-client
```

### 6.2 Wrong `meta_addr`

Client/store points at a URL that does not resolve, does not connect, or
connects to a non-dtmpfs process.

**Symptom:** client startup fails fast before `mount2`. Log:
`failed to ListNodes from meta: status: Unavailable, ...` or
`transport error: connection refused` or `dns error: failed to lookup`.

**Fix:** correct the URL in TOML and restart.

### 6.3 Port collision

`metasrv`/`storesrv` cannot bind `listen` because something else holds
the port.

**Symptom:** process exits seconds after start: `failed to bind
0.0.0.0:7100: Address already in use (os error 98)`.

**Fix:**

```
ss -tnlp | grep :7100         # find holder
kill <pid>                    # or pick a different port
```

### 6.4 Two stores with the same `node_id`

Both stores heartbeat with the same `node_id`; meta updates `last_seen`
on each, never times them out, and the ring's single entry flaps between
two physical addrs depending on which heartbeat last arrived.

**Symptom:** reads "owned by" that node_id flip between success and EIO.
Meta log shows `addr` flipping. v1 meta does NOT detect this; Phase 6 adds
a `heartbeat_addr ≠ recorded_addr` warning.

**Fix:**

```
# Pick unique node_ids in each store TOML, then:
systemctl --user restart 'dtmpfs-store@*' dtmpfs-meta
# (Meta restart trips §3.2 data loss. Acceptable — the collision likely
#  corrupted state already.)
```

### 6.5 Mounting twice on the same path

**Symptom:** second `dtmpfs-mount --config client.toml` exits with
`Device or resource busy (os error 16)` (`EBUSY`); original mount keeps
working.

**Fix:** pick a different `mount_point`, or `fusermount3 -u /mnt/dtmpfs`
and start fresh.

---

## 7. Data integrity considerations

dtmpfs v1 has **no application-layer checksum**.

### 7.1 Wire corruption

We have: TCP 16-bit checksum, HTTP/2 framing, protobuf length-prefixing.
We do NOT have: an end-to-end content checksum. A bit flip that survives
TCP and is bracketed by valid framing is undetectable in v1. Quiet LAN
likelihood is in the ~10^-9 to 10^-11 per-segment range — accepted for
v1.

### 7.2 In-RAM corruption

ECC RAM is assumed. There is no scrubbing or periodic re-checksum.

### 7.3 Stale-write reuse

A slow client could write to (ino, idx) at generation G when the inode is
now at G+5. Partial mitigation: `BlockKey` carries `generation`, so writes
at G and G+5 land in different DashMap cells (older cell orphaned). The
store does NOT yet reject writes for a too-old generation
(`Status::failed_precondition`); that is Phase 6.

### 7.4 Phase-6 plan

Add `xxhash64` per block: stored in `BlockPlacement.checksum: u64`,
computed by the client before `WriteBlock`, verified by the store on
receive, returned by `ReadBlock`, re-verified by the client. Cost ~one
xxhash pass per block (~10 GB/s). Until then, **do not assume read-back
equals written**.

---

## 8. Detection & alerting

### 8.1 What v1 has (manual)

- Meta WARN log when a store crosses `heartbeat_timeout_ms` and is moved
  to `Down`.
- Stores WARN at `ram_used > 0.8 × ram_budget_bytes` (per-WriteBlock).
- Stores WARN when refusing a write due to budget.
- Stores expose `/debug/blocks` if `debug_http_listen` is set (JSON
  `{ count, ram_used, ram_budget_bytes }`).

There is no Prometheus exporter or `/metrics` in v1; both land in Phase 6.

### 8.2 Suggested external monitoring

Cron-scrape works without Prometheus:

```
# /etc/cron.d/dtmpfs-watch (every minute)
* * * * * dtmpfs curl -fs --max-time 2 http://store-0:7300/debug/blocks \
            >> /var/log/dtmpfs-store-0.jsonl || \
            logger -t dtmpfs "store-0 unreachable"
```

| Metric | Source | Alert when |
|---|---|---|
| store up | `/debug/blocks` reachable | down ≥ 2 × heartbeat_timeout_ms |
| store ram_used | `/debug/blocks` JSON | > 80% of `ram_budget_bytes` |
| store block count | `/debug/blocks` JSON | drops > 10% in 1 min (crash + restart) |
| meta up | `pgrep metasrv` | down at all |
| meta RSS | `/proc/<pid>/status` | > 70% host RAM (inode alarm) |
| mount alive | `mountpoint -q /mnt/dtmpfs` | not a mountpoint |

### 8.3 Logs to watch

```
journalctl --user -u dtmpfs-meta -f
journalctl --user -u 'dtmpfs-store@*' -f
journalctl --user -u dtmpfs-client -f

# Greps that matter:
#   "marked Down"             — store went away (§3.1)
#   "refused write"           — RAM budget hit (§5.1)
#   "ram_budget_bytes 80%"    — soft warning
#   "Status::unauthenticated" — wrong token (§6.1)
#   "panicked at"             — bug; capture and file
```

---

## 9. Recovery runbooks

The runbooks below are framed as "user/app symptom → what to check → what to
do". They cross-reference the scenarios in §3 and §6.

### 9.1 Runbook: store-down

**Symptom:** `cat /mnt/dtmpfs/some-file` returns `Input/output error` for
*some* files but not others; new files written now also sometimes `EIO`.

```
# Step 1. Confirm the symptom is not "every op EIOs" — that's §9.2.
ls /mnt/dtmpfs/                          # should succeed
cat /mnt/dtmpfs/some-other-file          # should succeed for some

# Step 2. Find the dead store.
journalctl --user -u dtmpfs-meta -n 200 | grep -i 'marked Down'
# Expected output: "node store-1 marked Down: last_seen 7s ago".

# Step 3. SSH to that store and confirm.
ssh store-1
systemctl --user status dtmpfs-store@1
# If it's really down:
journalctl --user -u dtmpfs-store@1 -n 50
# Look for: panic, OOM, "Address already in use", explicit SIGKILL.

# Step 4. Restart it.
systemctl --user start dtmpfs-store@1

# Step 5. Verify back in service.
journalctl --user -u dtmpfs-meta -n 20 | grep -i 'marked Up'
# Expected: "node store-1 marked Up after heartbeat".

# Step 6. If R=1 the data is gone. Inform the user. There is no rebuild.
#         If R≥2 with Phase 6 client, reads should now succeed; there is
#         still no automated re-replication of the lost copy in v1.
```

### 9.2 Runbook: meta-down

**Symptom:** every operation on the mount returns `EIO`. Even
`ls /mnt/dtmpfs` fails with `Input/output error`.

```
# Step 1. Confirm meta is the cause.
ssh meta-host
systemctl --user status dtmpfs-meta
# If "active (running)": it's actually a token mismatch (§9.4) or a network
# issue. Re-check.

# Step 2. Restart meta.
systemctl --user restart dtmpfs-meta
journalctl --user -u dtmpfs-meta -f
# Expected: "meta server bound :7100", then within ~heartbeat_interval_ms
# you should see all stores reappear.

# Step 3. WARN THE USER. Every inode is gone. The FS is now empty.
#         (Search §3.2 to confirm if you are doing this for the first
#          time.)

# Step 4. Bounce clients to clear stale state.
ssh client-a
fusermount3 -u /mnt/dtmpfs
systemctl --user restart dtmpfs-client
mount | grep dtmpfs           # confirm remounted
ls /mnt/dtmpfs                # should now succeed and show empty dir
```

### 9.3 Runbook: mount-stuck

**Symptom:** `ls /mnt/dtmpfs` hangs forever. `cat` against any path hangs.
`Ctrl-C` doesn't free the process; it stays in `D` state.

```
# Step 1. Is the dtmpfs-mount process alive?
pgrep -af dtmpfs-mount
# If no:
fusermount3 -u /mnt/dtmpfs
systemctl --user start dtmpfs-client
# If yes: continue.

# Step 2. Is it CPU-bound?
top -p $(pgrep dtmpfs-mount)
# If 100% on one core: probably a tight loop bug; capture a backtrace:
sudo gdb -p $(pgrep dtmpfs-mount) -batch -ex 'thread apply all bt'

# Step 3. Is it stuck on a meta or store RPC?
ss -tnp 'dst :7100 or dst :7200 or dst :7201'   # confirms an open conn
# Then check the remote side liveness (§9.1, §9.2).

# Step 4. FUSE kernel queue full?  See §5.3.
ps -eo pid,stat,wchan,cmd | grep -E 'fuse|D'
# If many D-state processes are parked in fuse_request_alloc, raise
# fuse_threads in client.toml and restart the client.

# Step 5. Last resort — kill and remount.
kill -9 $(pgrep dtmpfs-mount)
fusermount3 -u /mnt/dtmpfs   # may need lazy: sudo umount -l /mnt/dtmpfs
systemctl --user restart dtmpfs-client
```

### 9.4 Runbook: client-token-mismatch

**Symptom:** mount comes up cleanly (`mount | grep dtmpfs` shows it), but
every operation `EIO`s. Logs show `Status::unauthenticated`.

```
# Step 1. Confirm.
journalctl --user -u dtmpfs-client -n 50 | grep -iE 'unauth|token'
# Expected: "Status { code: Unauthenticated, ... cluster token mismatch }".

# Step 2. Compare tokens across roles.
for f in ~/.config/dtmpfs/{meta,store,client}.toml; do
  echo "== $f"
  grep cluster_token "$f"
done

# Step 3. Pick one and propagate.
TOKEN=$(grep cluster_token ~/.config/dtmpfs/meta.toml | cut -d'"' -f2)
sed -i "s|^cluster_token = .*|cluster_token = \"$TOKEN\"|" \
   ~/.config/dtmpfs/store*.toml ~/.config/dtmpfs/client.toml

# Step 4. Roll-restart.
systemctl --user restart dtmpfs-meta 'dtmpfs-store@*' dtmpfs-client
# Step 5. Verify.
ls /mnt/dtmpfs                # should now succeed
```

### 9.5 Runbook: store-out-of-RAM

**Symptom:** `write(2)` returns 0 (kernel accepted it) but `close(2)`
returns `errno == ENOSPC` and the application sees "No space left on
device".

```
# Step 1. Confirm capacity hit.
df -h /mnt/dtmpfs                                 # cluster-wide free
for s in store-0 store-1 store-2; do
  curl -s http://$s:7200/debug/blocks | jq '{used:.ram_used,budget:.ram_budget_bytes}'
done

# Step 2. Free space (deletion is the only v1 mechanism).
rm -rf /mnt/dtmpfs/old-checkpoints/

# Step 3. If deletion isn't an option, raise budgets.
sudoedit ~/.config/dtmpfs/store.toml              # bump ram_budget_bytes
systemctl --user restart 'dtmpfs-store@*'         # this restarts EACH store
                                                  # in sequence; data on a
                                                  # store is lost when it
                                                  # restarts (R=1)

# Step 4. Or add a store. Build / scp the binary, drop a TOML, start it:
ssh new-store
systemctl --user start dtmpfs-store@4
# Meta picks it up via heartbeat; new writes will distribute to it. Note
# that EXISTING writes do NOT move (no rebalancer in v1).
```

### 9.6 Runbook: stale-mount-after-host-reboot

**Symptom:** after a client host reboot, `/mnt/dtmpfs` directory exists but
nothing is mounted on it. The systemd unit didn't start.

```
# Step 1. Verify.
mountpoint /mnt/dtmpfs                            # "is not a mountpoint"
systemctl --user status dtmpfs-client

# Step 2. Common cause: the user's systemd --user instance only starts
#         after that user logs in. Enable lingering so the user instance
#         starts at boot.
sudo loginctl enable-linger $USER

# Step 3. Make sure the unit is enabled.
systemctl --user enable dtmpfs-client.service

# Step 4. Start it now.
systemctl --user start dtmpfs-client
mount | grep dtmpfs
```

---

## 10. Disaster recovery

There is none in v1. dtmpfs is RAM-only, ephemeral, and stateless across
`dtmpfs-meta` restart. No backups, no snapshots (`Meta.Snapshot` is Phase
8), no WAL.

The "backup" model is **the user's responsibility**:

```
*/5 * * * * cp -au /mnt/dtmpfs/important/ /persistent/dtmpfs-mirror/
```

Roadmap, in order: Phase 6 background re-replication; Phase 7 Raft for
meta; Phase 7+/8 WAL; Phase 8+ snapshot-to-object-store.

Until then: **do not put anything in dtmpfs that you cannot regenerate**.
Same rule as `tmpfs(5)` — not a coincidence.

---

## See also

- [`HLD.md`](HLD.md) §9 — design-side framing of failure handling.
- [`consistency.md`](consistency.md) — close-to-open semantics and what
  "loss" means with respect to NFS-style visibility.
- [`operations.md`](operations.md) — the daily-run side: deploy, monitor,
  upgrade.
- [`configuration.md`](configuration.md) — every knob mentioned in this
  document.
- [`testing.md`](testing.md) and [`acceptance-tests.md`](acceptance-tests.md)
  — how each scenario above is reproduced in CI.
</content>
</invoke>