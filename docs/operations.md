# dtmpfs — Operations Guide

How to run dtmpfs in production: prerequisites, deployment, systemd units,
monitoring, troubleshooting, tuning, capacity, security, upgrade, and health
checks.

Design rationale: [`HLD.md`](HLD.md). Failure scenarios and runbooks:
[`failure-model.md`](failure-model.md). Full TOML schema:
[`configuration.md`](configuration.md).

---

## 1. Prerequisites

Linux-only. Tested on Ubuntu 22.04 / 24.04 and Debian 12 with kernel ≥ 5.4
(FUSE 3 features).

### 1.1 Kernel and FUSE

```
uname -r                          # expect ≥ 5.4
lsmod | grep -w fuse              # if empty: sudo modprobe fuse
ls -l /dev/fuse                   # expect: crw-rw-rw- root root
```

For non-root mounting with `AllowOther` (default):

```
grep -q '^user_allow_other' /etc/fuse.conf || \
  sudo sh -c 'echo user_allow_other >> /etc/fuse.conf'
```

### 1.2 Build dependencies

```
sudo apt-get update
sudo apt-get install -y libfuse3-dev pkg-config protobuf-compiler build-essential
pkg-config --modversion fuse3     # expect ≥ 3.10
protoc --version                  # expect ≥ libprotoc 3.15
```

### 1.3 Rust toolchain

```
which rustc || curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# rust-toolchain.toml pins 1.94.0; rustup respects it.
cd ~/dtmpfs && cargo --version
```

### 1.4 Network

gRPC over HTTP/2 over TCP. Open on each host firewall:

| Component | Default port | From |
|---|---|---|
| meta | 7100/tcp | every store, every client |
| store-N | 7200+N/tcp | every client, every other store (for `Replicate`) |
| store debug | 7300+N/tcp (optional) | monitoring host |

No UDP, no multicast, no service discovery — every TOML hard-codes
`meta_addr`.

### 1.5 DNS / `/etc/hosts`

TOMLs reference URLs like `http://meta-host:7100`. Without DNS, add on
every node:

```
# /etc/hosts
10.0.0.10  meta-host
10.0.0.20  store-0
10.0.0.21  store-1
```

### 1.6 Mount point

Must exist, be an empty directory, writable by the running user:

```
sudo mkdir -p /mnt/dtmpfs && sudo chown "$USER" /mnt/dtmpfs
```

---

## 2. Build

```
cd ~/dtmpfs && cargo build --release --workspace
# Cold: 3-5 min on a 4-core box. Warm rebuild: 20-40 s.
ls target/release/{metasrv,storesrv,dtmpfs-mount}
```

We keep debuginfo (set `profile.release.strip = "symbols"` to remove if
needed); backtraces help with [`failure-model.md`](failure-model.md) §9.

For identical binaries across hosts, build once and `scp`:

```
for h in meta-host store-{0,1,2} client-{a,b}; do
  scp target/release/{metasrv,storesrv,dtmpfs-mount} "$h:/usr/local/bin/"
done
```

Cross-compilation is not part of the v1 build matrix.

---

## 3. Single-host deployment (prototype)

Three processes plus one mount, all on one VM. Useful for development, CI,
and the canonical smoke test from [`HLD.md`](HLD.md) §11.

### 3.1 Layout

```
/usr/local/bin/metasrv
/usr/local/bin/storesrv
/usr/local/bin/dtmpfs-mount
~/.config/dtmpfs/meta.toml
~/.config/dtmpfs/store0.toml
~/.config/dtmpfs/store1.toml
~/.config/dtmpfs/client.toml
/mnt/dtmpfs/                       (mount point, empty before mount)
```

Minimal TOMLs for single-host (full schema in
[`configuration.md`](configuration.md)). All keys are at the top level — there are no
nested sub-sections:

```toml
# ~/.config/dtmpfs/meta.toml
role                 = "meta"
node_id              = "meta-0"
cluster_token        = "single-host-prototype-only-1234"
listen               = "127.0.0.1:7100"
replication_factor   = 1
heartbeat_timeout_ms = 5000
```

```toml
# ~/.config/dtmpfs/store0.toml
role              = "store"
node_id           = "store-0"
cluster_token     = "single-host-prototype-only-1234"
listen            = "127.0.0.1:7200"
advertise_addr    = "127.0.0.1:7200"      # addr clients use to reach this store
meta_addr         = "http://127.0.0.1:7100"
ram_budget_bytes  = 8000000000            # 8 GB
debug_http_listen = "127.0.0.1:7300"
```

```toml
# ~/.config/dtmpfs/store1.toml — identical except node_id and ports
role              = "store"
node_id           = "store-1"
cluster_token     = "single-host-prototype-only-1234"
listen            = "127.0.0.1:7201"
advertise_addr    = "127.0.0.1:7201"
meta_addr         = "http://127.0.0.1:7100"
ram_budget_bytes  = 8000000000
debug_http_listen = "127.0.0.1:7301"
```

```toml
# ~/.config/dtmpfs/client.toml
role                    = "client"
node_id                 = "client-local"
cluster_token           = "single-host-prototype-only-1234"
meta_addr               = "http://127.0.0.1:7100"
mount_point             = "/mnt/dtmpfs"
block_size              = 1048576
replication_factor      = 1
attr_cache_ttl_ms       = 1000
block_cache_capacity_mb = 1024

[mount_options]
allow_other         = true    # requires user_allow_other in /etc/fuse.conf
default_permissions = true
auto_unmount        = true
no_atime            = true
```

For a dev machine without `user_allow_other` in `/etc/fuse.conf`, set
`allow_other = false` and `auto_unmount = false` in the `[mount_options]` table.

### 3.2 Run with tmux (interactive development)

```
tmux new -s dtmpfs
# Pane 0 — meta
tmux send-keys -t dtmpfs.0 \
   'RUST_LOG=info /usr/local/bin/metasrv --config ~/.config/dtmpfs/meta.toml' C-m
tmux split-window -t dtmpfs -v
tmux send-keys -t dtmpfs.1 \
   'RUST_LOG=info /usr/local/bin/storesrv --config ~/.config/dtmpfs/store0.toml' C-m
tmux split-window -t dtmpfs -h
tmux send-keys -t dtmpfs.2 \
   'RUST_LOG=info /usr/local/bin/storesrv --config ~/.config/dtmpfs/store1.toml' C-m
tmux split-window -t dtmpfs -v
tmux send-keys -t dtmpfs.3 \
   'RUST_LOG=info /usr/local/bin/dtmpfs-mount --config ~/.config/dtmpfs/client.toml' C-m
tmux attach -t dtmpfs
```

Expected log order: meta `bound 127.0.0.1:7100`; each store `bound …`
then `heartbeat ack from meta-0`; client `connected to meta`,
`ListNodes returned 2 nodes`, `mounted /mnt/dtmpfs`.

### 3.3 Verify

```
mount | grep dtmpfs                                # dtmpfs on /mnt/dtmpfs type fuse...
stat -f /mnt/dtmpfs && df -h /mnt/dtmpfs           # cluster aggregate
echo hello > /mnt/dtmpfs/x && cat /mnt/dtmpfs/x    # round-trip small
dd if=/dev/urandom of=/mnt/dtmpfs/big bs=1M count=64 status=progress
md5sum /mnt/dtmpfs/big
mkdir /mnt/dtmpfs/d && echo bye > /mnt/dtmpfs/d/y && ls -la /mnt/dtmpfs/d
rm -r /mnt/dtmpfs/d
```

If anything fails: §7 and [`failure-model.md`](failure-model.md) §9.

---

## 4. Multi-host deployment

The minimum production-shaped topology is one meta plus two stores plus N
clients, on N+3 hosts (or fewer if you co-locate). Replication factor R=2
becomes meaningful here.

### 4.1 Topology

```
                  meta-host
                  metasrv :7100
                       |
       +---------------+---------------+
       |               |               |
   store-0         store-1         store-2
   :7200           :7200           :7200
       |               |               |
       +-------+-------+-------+-------+
               |               |
           client-a        client-b
           dtmpfs-mount    dtmpfs-mount
           /mnt/dtmpfs     /mnt/dtmpfs
```

You can co-locate roles to save VMs (e.g. meta + store-0 on host-A —
note that host-A death is then a §4.2 scenario in `failure-model.md`).
A common shape for ML workers: meta on its own host, every other host
is "store + client".

### 4.2 Distribute the binary

```
# scp from a build host (recommended):
for h in meta-host store-{0,1,2} client-{a,b}; do
  scp target/release/{metasrv,storesrv,dtmpfs-mount} "$h":/usr/local/bin/
done
# Alternatives: shared NFS of /usr/local/bin (NOT dtmpfs itself), or
# build on every host with the identical toolchain.
```

### 4.3 Per-role TOML differences

Same shape as §3.1 with real IPs/hostnames. Three differences that matter
for multi-host:

```toml
# meta.toml: bind on a real interface
listen = "0.0.0.0:7100"           # or "10.0.0.10:7100" for defense-in-depth

# store0.toml on host-store-0 (all keys flat, no sub-sections)
listen         = "0.0.0.0:7200"
advertise_addr = "10.0.0.20:7200" # real IP — clients reach this store here
meta_addr      = "http://meta-host:7100"

# client.toml
meta_addr = "http://meta-host:7100"
```

### 4.4 Open firewall

```
# UFW example on each role-host:
sudo ufw allow from 10.0.0.0/24 to any port 7100 proto tcp      # meta
sudo ufw allow from 10.0.0.0/24 to any port 7200:7299 proto tcp # stores
sudo ufw allow from 10.0.0.0/24 to any port 7300:7399 proto tcp # debug
sudo ufw reload
```

### 4.5 Cross-host smoke

```
# client-a:
echo "hi-from-a" > /mnt/dtmpfs/cross && sync
# client-b:
cat /mnt/dtmpfs/cross             # expect: hi-from-a

# 200 MiB integrity:
dd if=/dev/urandom of=/tmp/src bs=1M count=200            # on client-a
cp /tmp/src /mnt/dtmpfs/big && md5sum /tmp/src /mnt/dtmpfs/big
ssh client-b md5sum /mnt/dtmpfs/big                       # must match
```

If md5 differs, do NOT debug at app level — it's a dtmpfs bug. Capture
client logs both sides, store logs every node, meta log, and file an
issue.

---

## 5. Systemd units

Use **user units**. dtmpfs runs as the invoking user; root is unnecessary.
User units require `loginctl enable-linger` to start at boot.

```
sudo loginctl enable-linger "$USER"
mkdir -p ~/.config/systemd/user
```

### 5.1 `dtmpfs-meta.service`

```ini
# ~/.config/systemd/user/dtmpfs-meta.service
[Unit]
Description=dtmpfs metadata server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/metasrv --config %h/.config/dtmpfs/meta.toml
Environment=RUST_LOG=info
Restart=on-failure
RestartSec=2s
LimitNOFILE=65535
# meta is single-threaded for state operations; one CPU is enough.
# Adjust if profiling shows otherwise.
CPUWeight=200
MemoryHigh=80%
MemoryMax=90%

[Install]
WantedBy=default.target
```

### 5.2 `dtmpfs-store@.service` (templated)

The `@` makes this a *template* unit. The instance name (after the `@`)
maps to the `node_id` and to the config filename. Convention: instance
name == numeric ID.

```ini
# ~/.config/systemd/user/dtmpfs-store@.service
[Unit]
Description=dtmpfs storage node %i
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/storesrv --config %h/.config/dtmpfs/store%i.toml
Environment=RUST_LOG=info
Restart=on-failure
RestartSec=2s
LimitNOFILE=65535
# Stores are RAM-heavy. Plan around ram_budget_bytes from the TOML.
MemoryHigh=92%
MemoryMax=95%
# OOM-killing the store loses data (R=1) — bias against it.
OOMScoreAdjust=-200

[Install]
WantedBy=default.target
```

Enable and start specific instances:

```
systemctl --user enable dtmpfs-store@0.service
systemctl --user enable dtmpfs-store@1.service
systemctl --user start  dtmpfs-store@0.service
systemctl --user start  dtmpfs-store@1.service
systemctl --user status 'dtmpfs-store@*'
```

### 5.3 `dtmpfs-client.service`

```ini
# ~/.config/systemd/user/dtmpfs-client.service
[Unit]
Description=dtmpfs FUSE mount
# We need the cluster reachable AND we need /dev/fuse readable. The
# `dev-fuse.device` unit only exists if the fuse module is loaded.
After=network-online.target dev-fuse.device
Wants=network-online.target dev-fuse.device

[Service]
Type=simple
ExecStartPre=/usr/bin/test -d /mnt/dtmpfs
# Best-effort cleanup if the previous run left a stale mount.
ExecStartPre=-/usr/bin/fusermount3 -u /mnt/dtmpfs
ExecStart=/usr/local/bin/dtmpfs-mount --config %h/.config/dtmpfs/client.toml
ExecStop=/usr/bin/fusermount3 -u /mnt/dtmpfs
Environment=RUST_LOG=info
Restart=on-failure
RestartSec=2s
LimitNOFILE=65535

[Install]
WantedBy=default.target
```

Enable at boot:

```
systemctl --user enable dtmpfs-meta.service        # meta-host only
systemctl --user enable dtmpfs-store@0.service     # store-host only
systemctl --user enable dtmpfs-client.service      # every client host
```

### 5.4 Order of operations

`After=network-online.target` is enough on a single host. Across hosts,
the store's heartbeat retry and the client's `tonic` connect retry
absorb early "connection refused"; wait one heartbeat interval.

---

## 6. Monitoring

### 6.1 Logs

```
journalctl --user -u dtmpfs-meta -f
journalctl --user -u 'dtmpfs-store@*' -f
journalctl --user -u dtmpfs-client -f
journalctl --user -u dtmpfs-meta -p err -n 200          # errors only
journalctl --user -u dtmpfs-meta -g 'heartbeat\|marked' # membership events
```

`RUST_LOG` recipes: `info` (steady state); `info,dtmpfs=debug,tonic=info`
(bring-up); `info,tonic=trace,h2=trace` (wire debugging — chatty).

### 6.2 Store inspection

If `debug_http_listen` is set:

```
curl -s http://store-0:7300/debug/blocks
# { "node_id":"store-0", "count":4827, "ram_used":5051023360,
#   "ram_budget_bytes":8000000000, "uptime_secs":86400 }
```

### 6.3 Meta inspection

The meta exposes `GET /debug/state` via an optional HTTP endpoint (not enabled by default).
Use `grpcurl` for live inspection:

```
grpcurl -plaintext -H 'cluster-token: <T>' \
        meta-host:7100 dtmpfs.meta.v1.Meta/ListNodes
```

Note: the gRPC metadata header is `cluster-token` (not `cluster-token`).

### 6.4 Mount and FUSE stats

```
df -h /mnt/dtmpfs                       # cluster aggregate via statfs
stat -f /mnt/dtmpfs                     # bsize, free, total
mount | grep dtmpfs                     # FUSE-resolved mount options
cat /sys/fs/fuse/connections/*/waiting  # kernel queue depth (≈0 idle)
```

If `waiting` sits at 12+, raise `fuse_threads` (§8.3).

### 6.5 OS-level

```
htop                                    # CPU per process
ss -tnp 'sport = :7100 or sport :7200'  # who's connected
dmesg --ctime | grep -i fuse            # kernel side
pidstat -r -p $(pgrep metasrv) 5        # meta RSS trend
```

---

## 7. Troubleshooting decision tree

This is the high-level triage; for full runbooks see
[`failure-model.md`](failure-model.md) §9.

```
Symptom: every op on /mnt/dtmpfs returns EIO
├─ Is dtmpfs-mount running?
│    pgrep -af dtmpfs-mount
│    ├─ No  → systemctl --user start dtmpfs-client; check journal
│    └─ Yes → continue
│
├─ Is the mount actually present?
│    mountpoint /mnt/dtmpfs
│    ├─ No  → fusermount3 -u; restart client
│    └─ Yes → continue
│
├─ Can client reach meta?
│    grpcurl -plaintext -H 'cluster-token: <T>' \
│            meta-host:7100 dtmpfs.meta.Meta/ListNodes
│    ├─ Connection refused / timeout
│    │      → meta down (failure-model §3.2 / §9.2)
│    └─ Status: Unauthenticated
│           → token mismatch (failure-model §6.1 / §9.4)
│
├─ Is at least one store live in meta's ring?
│    journalctl --user -u dtmpfs-meta -n 100 | grep 'marked'
│    ├─ All stores Down → start them; check connectivity
│    └─ At least one Up → continue
│
├─ Does the affected file's primary store respond?
│    For each store: curl -fs http://store-N:7300/debug/blocks
│    ├─ Some unreachable → failure-model §3.1 / §9.1
│    └─ All reachable    → continue
│
└─ Is the store rejecting writes for capacity?
       grep 'refused write' on store logs
       ├─ Yes → failure-model §5.1 / §9.5
       └─ No  → escalate; capture all three logs and file an issue.

Symptom: ls /mnt/dtmpfs hangs forever
├─ Is meta unreachable?  (See §3.2)  Reads block on Lookup until deadline.
├─ Is the FUSE kernel queue full?    (See failure-model §5.3)
└─ Is the dtmpfs-mount process in a tight loop?  Capture a backtrace.

Symptom: write returns 0 (success) but close returns ENOSPC
└─ Store budget hit.  failure-model §5.1 / §9.5.

Symptom: cross-host visibility takes >1s after writer's close
├─ attr_cache_ttl_ms too high on reader? (default 1000)
└─ Reader hadn't issued a fresh open() since the write. Close-to-open
   only takes effect at open boundaries; an already-open fd does NOT
   refresh attrs (consistency.md §3).

Symptom: Cross-host md5sum mismatch
└─ This is a bug. Capture: client logs both sides, store logs every node,
   meta log, output of `curl /debug/blocks` for every store. File an
   issue.
```

---

## 8. Performance tuning

Most knobs trade RPC overhead against parallelism or RAM. None are
required for correctness.

### 8.1 `block_size` (client.toml)

Default 1 MiB. Larger (4-8 MiB) → fewer RPCs, better 10 GbE utilization,
more tail fragmentation. Smaller (≥ 256 KiB) → less small-file waste,
more per-RPC overhead. **All components must agree** — v1 has no runtime
check (Phase 6). Changing requires a full drain and restart (§11).

### 8.2 `block_cache_capacity_mb` (client.toml)

Default 1 GiB. LRU keyed by `(ino, generation, block_idx)`. Raise when
hot files are re-read by the same client (broadcast dataset shard,
build cache). Beyond the working set, more cache wastes RAM.

### 8.3 `fuse_threads` (client.toml)

Default 4. Raise to 16/32 when many concurrent FUSE ops or
`/sys/fs/fuse/connections/*/waiting` > 8. Beyond ~32 you're typically
RPC-pipeline-bound.

### 8.4 `attr_cache_ttl_ms` (client.toml)

Default 1000 ms. Drives both dtmpfs `AttrCache` and FUSE kernel
`attr_timeout`/`entry_timeout`. Lower → fresher cross-host metadata at
RPC cost. Higher → stale `stat` for up to the TTL. `open(2)` always
re-fetches (this is the close-to-open invalidation point).

### 8.5 `tokio_worker_threads`

Default `null` (= num_cpus). Lower on co-located meta+store hosts only
after profiling.

### 8.6 RPC deadlines

- `rpc_timeout_ms` (5000) — metadata RPCs.
- `write_rpc_timeout_ms` (30000) — `Store.WriteBlock`. Larger because a
  1 MiB block over a slow link can take seconds.

Lower to fail fast on a sick cluster; raise for slow links.

### 8.7 TCP / OS

```
# Apply on every meta and every store host:
sudo tee /etc/sysctl.d/90-dtmpfs.conf <<'EOF'
net.ipv4.tcp_congestion_control=bbr
net.core.rmem_max=67108864
net.core.wmem_max=67108864
EOF
sudo sysctl --system
```

### 8.8 NUMA

A meta/store pinned away from its NIC can lose 10-20% latency. Use
`lstopo` and `numactl --cpunodebind=0 --membind=0`. Irrelevant on
small/medium VMs.

---

## 9. Capacity planning

### 9.1 Per-store RAM

```
per_store_RAM = ram_budget_bytes ≤ host_free_RAM × 0.8
```

Per-block overhead (`Bytes` header + `DashMap` slot) is ~100 B at 1 MiB
blocks → sub-1% slippage.

### 9.2 Per-client BlockCache

```
per_client_RAM = block_cache_capacity_mb × 1 MiB
              + dirty_buffer_table        (≤ open_fds × block_size)
```

### 9.3 Meta RAM

```
meta_RAM ≈ 200 B × num_inodes
       +  32 B × num_blocks
       + 200 B × num_open_handles
```

±30% depending on map slack. On a 200 GB-RAM host: ~200M empty files →
~40 GB; ~10M × 64 MiB files → ~22 GB. Alarm at 70% RSS (see
[`failure-model.md`](failure-model.md) §5.4).

### 9.4 Cluster capacity

```
cluster_capacity_GB ≈ Σ(store ram_budget) / R
```

Add stores to grow capacity. v1 does NOT redistribute existing blocks;
new writes fill new nodes.

### 9.5 Network capacity

A 1 MiB block over 10 GbE is ~1 ms wire time. Reading 1 GiB at full
bandwidth from a single store is ~1 s. Concurrent throughput
≈ `min(NIC bandwidth, Σ store egress)`.

---

## 10. Security operations

Minimal v1 security: shared `cluster_token`, no TLS, no per-user auth.
Fitness: trusted LAN, single team.

### 10.1 Token rotation

No online rotation in v1.

```
NEW=$(openssl rand -base64 32)
for h in meta-host store-{0,1,2} client-{a,b}; do
  ssh "$h" "sed -i 's|^cluster_token = .*|cluster_token = \"$NEW\"|' \
            ~/.config/dtmpfs/*.toml"
done
# Roll-restart: meta, then stores, then clients.
ssh meta-host             systemctl --user restart dtmpfs-meta
for s in store-{0,1,2}; do ssh "$s" systemctl --user restart 'dtmpfs-store@*'; done
for c in client-{a,b};   do ssh "$c" systemctl --user restart dtmpfs-client; done
```

There is a window of seconds where roles disagree; cross-role RPCs in
that window fail `Unauthenticated`. Schedule during quiet time.

To keep secrets off disk, supply the token via env (see
[`configuration.md`](configuration.md) §8):

```ini
# In each unit:
Environment=DTMPFS_CLUSTER_TOKEN=<token>
```

### 10.2 Network exposure

- Bind to a private IP (`listen = "10.0.0.10:7100"`); avoid `0.0.0.0`
  unless your firewall is the source of truth.
- Reject inbound from non-cluster sources at iptables/nftables/VPC.
- Across an untrusted network, tunnel via WireGuard or SSH.

### 10.3 Audit log

Meta logs every successful RPC at INFO with method, peer, ino, outcome.
Forward to a log aggregator (Loki, ELK, journald-upload) for retention.

---

## 11. Upgrade procedure

v1 does **not** support online upgrade. Every upgrade is a full cluster
restart and loses all data. Same constraint as `tmpfs(5)`.

```
# 1. Confirm the workload tolerates loss.

# 2. Drain clients.
for c in client-{a,b}; do
  ssh "$c" 'fusermount3 -u /mnt/dtmpfs && systemctl --user stop dtmpfs-client'
done

# 3. Stop stores (order does not matter).
for s in store-{0,1,2}; do
  ssh "$s" 'systemctl --user stop "dtmpfs-store@*"'
done

# 4. Stop meta.
ssh meta-host 'systemctl --user stop dtmpfs-meta'

# 5. Replace binaries (§2 procedure).

# 6. Start meta, then stores, then clients.
ssh meta-host 'systemctl --user start dtmpfs-meta'
for s in store-{0,1,2}; do ssh "$s" 'systemctl --user start "dtmpfs-store@*"'; done
ssh meta-host 'journalctl --user -u dtmpfs-meta -n 50 | grep marked.Up'
for c in client-{a,b}; do ssh "$c" 'systemctl --user start dtmpfs-client'; done
```

A new binary that rejects an old TOML key will fail validation; there
is no auto-rollback — manually copy back the old binary.

Phase 7 (Raft + WAL) enables rolling upgrade; Phase 8 may add metadata
schema migrations.

---

## 12. Health checks

For load balancers, orchestrators, or cron reachability scripts.

### 12.1 Meta

```
grpcurl -plaintext -H "cluster-token: $TOKEN" --max-time 2 \
        meta-host:7100 dtmpfs.meta.Meta/ListNodes >/dev/null && echo OK || echo FAIL
```

Connection refused → process down. Unauthenticated → token mismatch
(liveness OK, readiness fail). Deadline exceeded → pegged; unhealthy.

### 12.2 Store

```
grpcurl -plaintext -H "cluster-token: $TOKEN" --max-time 2 \
        store-0:7200 dtmpfs.store.Store/Stat
# Healthy iff ram_used < ram_budget * 0.95.
curl -fs --max-time 2 http://store-0:7300/debug/blocks   # if enabled
```

A store at ≥ 95% budget is `ready=false` (writes fail) but still alive
for reads.

### 12.3 Client

```
mountpoint -q /mnt/dtmpfs && echo MOUNTED || echo UNMOUNTED
# v1: ls touches meta, so a meta outage looks like an unhealthy client.
ls -la /mnt/dtmpfs >/dev/null && echo HEALTHY || echo UNHEALTHY
# Phase 2 will add /mnt/dtmpfs/.health served entirely client-side.
```

### 12.4 End-to-end smoke

```
#!/bin/bash
set -euo pipefail
mountpoint -q /mnt/dtmpfs
probe="/mnt/dtmpfs/.health-probe-$$"
echo ok > "$probe" && [[ "$(cat "$probe")" == "ok" ]] && rm -f "$probe"
# Cross-host:
echo "probe-from-$(hostname)" > /mnt/dtmpfs/.cross-probe
ssh client-b "[[ -f /mnt/dtmpfs/.cross-probe ]]"
echo OK
```

---

## See also

- [`README.md`](../README.md) — quickstart.
- [`HLD.md`](HLD.md) — design rationale.
- [`architecture.md`](architecture.md) — diagrams + dataflow walkthroughs.
- [`failure-model.md`](failure-model.md) — failure scenarios and runbooks.
- [`configuration.md`](configuration.md) — every TOML key.
- [`protocol.md`](protocol.md) — gRPC service definitions.
- [`consistency.md`](consistency.md) — close-to-open semantics.
- [`testing.md`](testing.md), [`acceptance-tests.md`](acceptance-tests.md)
  — how the deployment is exercised in CI.
</content>
</invoke>