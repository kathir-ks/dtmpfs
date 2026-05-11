# dtmpfs — Configuration Reference

Canonical reference for every dtmpfs config knob: **type**, **default**,
**units**, **range**, **when to tune**, cross-component constraints.

Deployment context: [`operations.md`](operations.md). Failure-related
knobs: [`failure-model.md`](failure-model.md).

---

## 1. Format

dtmpfs reads one TOML file per role at startup via `--config <path>`.
Internally `dtmpfs-common::config::Config` is a tagged enum:

```rust
#[derive(Deserialize)]
#[serde(tag = "role", rename_all = "lowercase", deny_unknown_fields)]
pub enum Config {
    Meta(MetaConfig),
    Store(StoreConfig),
    Client(ClientConfig),
}
```

Every TOML must have a top-level `role` field; remaining keys are
interpreted relative to that role. Unknown keys are rejected.

A minimal client config (all keys at the top level — no sub-sections):

```toml
role          = "client"
node_id       = "client-a"
cluster_token = "16-or-more-chars-of-shared-secret"
meta_addr     = "http://10.0.0.10:7100"
mount_point   = "/mnt/dtmpfs"
```

---

## 2. Common keys (top-level, all roles)

### 2.1 `role`

| | |
|---|---|
| Type | enum string |
| Allowed | `"meta"`, `"store"`, `"client"` |
| Default | required |

Selects the rest of the schema. One role per process.

### 2.2 `node_id`

| | |
|---|---|
| Type | string |
| Default | required |
| Length | 1..=63 chars |
| Charset | `[a-z0-9-]+`, must start with `[a-z0-9]` |
| Constraint | cluster-unique |

Used in the rendezvous-hash ring (placement), log lines,
`Meta.HeartbeatNode`, and Phase-6 duplicate detection.

For meta, `node_id` is informational in v1. For stores it is **load-
bearing** — changing a store's `node_id` reshuffles which blocks the
ring directs to it; in-flight blocks become orphaned. **Rule:** pick
once, never change.

### 2.3 `cluster_token`

| | |
|---|---|
| Type | string |
| Default | required |
| Length | ≥ 16 chars |

Sent as `cluster-token` gRPC metadata header. Constant-time compared;
mismatch returns `Status::unauthenticated`. May be supplied via env var
`DTMPFS_CLUSTER_TOKEN` (§8) to keep secrets off disk. The entire v1
auth model.

### 2.4 `log`

| | |
|---|---|
| Type | enum string |
| Allowed | `trace \| debug \| info \| warn \| error` |
| Default | `info` |

Default `tracing-subscriber` filter. `RUST_LOG` overrides and supports
per-module filters (`tonic=debug`).

### 2.5 `listen`

| | |
|---|---|
| Type | `SocketAddr` string |
| Default | required for meta/store; ignored for client |

Examples: `"0.0.0.0:7100"`, `"10.0.0.10:7100"`, `"127.0.0.1:7100"`,
`"[::1]:7100"`. Bind error (`EADDRINUSE`, `EACCES` for <1024 as
non-root) aborts startup with a clear log line.

---

## 3. `[meta]` section

Only valid when `role = "meta"`.

### 3.1 `heartbeat_timeout_ms`

| | |
|---|---|
| Type | u64 |
| Default | `5000` |
| Units | ms |
| Range | ≥ 1.5 × store's `heartbeat_interval_ms`; validated ≥ 1500 |

A store is marked `Down` (removed from ring) after this many ms with no
heartbeat. Set to a comfortable multiple of the store interval (default
1000 ms) to absorb transient blips.

### 3.2 `gc_interval_ms`

| | |
|---|---|
| Type | u64 |
| Default | `60000` |
| Range | 1000..3_600_000 ms |
| Status | placeholder in v1; consumed by Phase-6 GC |

Orphaned-block sweep frequency. Phase 6 walks live blocks, asks stores
for their keys, deletes the difference. v1 parses and ignores.

### 3.3 `max_open_handles`

| | |
|---|---|
| Type | u64 |
| Default | `100000` |
| Range | ≥ 1 |

Guardrail on `MetaState.open_handles`. `Meta.Open` past this returns
`Status::resource_exhausted`. Each handle ≈ 150 B → ~15 MB at default.

### 3.4 Reserved (rejected by v1)

`[meta.metrics]`, `[meta.raft]`, `[meta.persist]` — reserved for Phase
6+ / 7. Do not use in v1.

---

## 4. `[store]` section

Only valid when `role = "store"`.

### 4.1 `meta_addr`

| | |
|---|---|
| Type | URL string, scheme `http` |
| Default | required |

`Meta.HeartbeatNode` target. Unresolvable hostname at startup logs a
warning and retries with backoff (no fail-fast). `https://` not supported
in v1.

### 4.2 `ram_budget_bytes`

| | |
|---|---|
| Type | u64 |
| Default | `8000000000` (8 GB) |
| Range | ≥ 1 MiB; recommended ≤ host_free × 0.8 |

Strict cap. `WriteBlock` past this returns
`Status::resource_exhausted`. Stores WARN at 80% of budget. The cap
covers block payload only; ~100 B per-block slot overhead is not
counted (sub-1% slippage at 1 MiB blocks).

### 4.3 `advertise_addr`

| | |
|---|---|
| Type | `host:port` string (no scheme), required |

The address at which this store is reachable from other cluster members. Used in
`Meta.HeartbeatNode` so the meta can advertise it to clients via `Meta.ListNodes`. Must
be reachable from every client host. On a single-host setup `127.0.0.1:<port>` works;
on multi-host set this to the real IP: `10.0.0.20:7200`.

### 4.4 `heartbeat_interval_ms`

| | |
|---|---|
| Type | hardcoded |
| Default | `1000` ms |

Heartbeat interval is not configurable in v1; the store fires `Meta.HeartbeatNode` every
1 s. Must be a small fraction of meta's `heartbeat_timeout_ms` (default 5 s).

### 4.4 `debug_http_listen`

| | |
|---|---|
| Type | string OR null |
| Default | `null` |

If set, plain-HTTP server with **no auth** on this addr. Endpoints:

- `GET /debug/blocks` — JSON `{ node_id, count, ram_used,
  ram_budget_bytes, uptime_secs }`.
- `GET /debug/blocks/list` — JSON list of every block. Expensive; do
  not poll.

Bind to 127.0.0.1 or a private interface only.

### 4.5 Reserved

`[store.checksum]` (Phase 6 xxhash64), `[store.replicate]` (Phase 6+
replicate-path tuning).

---

## 5. `[client]` section

Only valid when `role = "client"`. The most-tuned role.

### 5.1 `meta_addr`

URL, required, scheme `http`. Client refuses to start if `Meta.ListNodes`
fails at startup; exits non-zero with a log line.

### 5.2 `mount_point`

| | |
|---|---|
| Type | path, required |
| Constraints | exists, dir, empty, writable by user |

Validated before `fuser::mount2`. Mount-time errors (`EBUSY`, `EACCES`)
surface verbatim from the kernel. Non-empty paths are refused (the kernel
would shadow contents — confusing).

### 5.3 `block_size`

| | |
|---|---|
| Type | u64 |
| Default | `1048576` (1 MiB) |
| Range | power of two, 64 KiB..16 MiB |

Must equal the cluster's block size. v1 has **no runtime check** —
mismatch is undefined behavior (Phase 6 adds validation via
`Meta.ListNodes`). Decide at design time, do not change. To change in
v1: drain everything ([`operations.md`](operations.md) §11), update all
TOMLs, restart.

### 5.4 `replication_factor`

| | |
|---|---|
| Type | u8 |
| Default | `1` |
| Range | 1..=3 |

Stores each block is written to. R=1: no redundancy (v1 default for
regenerable scratch). R=2: 2× RAM, tolerates one store death (Phase-6
client adds read failover). R=3: 3× RAM, tolerates two.

### 5.5 `attr_cache_ttl_ms`

| | |
|---|---|
| Type | u64 |
| Default | `1000` ms |
| Range | 100..60000 |

Drives dtmpfs `AttrCache` and FUSE kernel `attr_timeout`/`entry_timeout`.
Lower → fresher cross-host metadata at RPC cost. Higher → stale `stat`.
`open(2)` always re-fetches (close-to-open invalidation; see
[`consistency.md`](consistency.md)).

### 5.6 `block_cache_capacity_mb`

| | |
|---|---|
| Type | u64 |
| Default | `1024` (1 GiB) |
| Range | 0..1_048_576 |

LRU `BlockCache` capacity, keyed by `(ino, generation, block_idx)`.
`0` disables. Per-client-process, not per-mount.

### 5.7 `fuse_threads`

| | |
|---|---|
| Type | u32 |
| Default | `4` |
| Range | 1..256 |

fuser worker threads. Raise for many-concurrent-files workloads.
See [`operations.md`](operations.md) §8.3.

### 5.8 `tokio_worker_threads`

| | |
|---|---|
| Type | u32 OR null |
| Default | `null` (= num_cpus) |
| Range | 1..256 |

Override only after profiling on co-located hosts.

### 5.9 `[mount_options]`

Nested table in the client TOML. Each key maps to a `fuser::MountOption` variant.

| Key | Type | Default | Maps to | Notes |
|---|---|---|---|---|
| `allow_other` | bool | `true` | `AllowOther` | needs `/etc/fuse.conf: user_allow_other` |
| `default_permissions` | bool | `true` | `DefaultPermissions` | kernel enforces POSIX mode bits |
| `auto_unmount` | bool | `true` | `AutoUnmount` | kernel unmounts on process exit |
| `no_atime` | bool | `true` | `NoAtime` | suppress atime updates (recommended) |

`allow_other = true` requires the line `user_allow_other` in `/etc/fuse.conf`; without it
the mount fails with `Permission denied`. For development without that config, set both
`allow_other = false` and `auto_unmount = false` (auto_unmount silently requires
allow_other). `no_atime = false` only if the workload reads atime.

### 5.10 `keepalive_interval_secs`

| | |
|---|---|
| Type | u64 |
| Default | `30` |
| Range | 1..3600 |

HTTP/2 keepalive ping interval on gRPC channels. Lower → faster broken-
conn detection; raise → less overhead.

### 5.11 `rpc_timeout_ms`

| | |
|---|---|
| Type | u64 |
| Default | `5000` |
| Range | 100..60000 |

Per-RPC deadline for metadata RPCs (`Meta.*`, `Store.Stat`,
`Store.DeleteBlock`). Exceeded → `Status::deadline_exceeded` → `EIO`.

### 5.12 `write_rpc_timeout_ms`

| | |
|---|---|
| Type | u64 |
| Default | `30000` |
| Range | 1000..600000 |

Per-RPC deadline for `Store.WriteBlock`. Larger than `rpc_timeout_ms`
because 1 MiB over a slow link can take seconds.

---

## 6. Full configuration examples

These are kept up-to-date with the parser. They are the canonical
starting points. Drop them in `~/.config/dtmpfs/` and edit IPs and
tokens.

### 6.1 `config/meta.toml.example`

All keys are at the top level (no sub-sections).

```toml
# dtmpfs metadata server. See docs/configuration.md for every key.
role                 = "meta"
node_id              = "meta-0"                       # cluster-unique; never change
cluster_token        = "REPLACE-WITH-32-RANDOM-CHARS" # ≥ 16; same on every role
listen               = "0.0.0.0:7100"                 # bind addr; firewall this
replication_factor   = 1                              # R for new block allocations
heartbeat_timeout_ms = 5000                           # mark store Down after this many ms
max_open_handles     = 100000                         # cap; resource_exhausted past this
```

### 6.2 `config/store.toml.example`

```toml
# dtmpfs storage node. One process per store; unique node_id and listen.
role              = "store"
node_id           = "store-0"
cluster_token     = "REPLACE-WITH-32-RANDOM-CHARS"
listen            = "0.0.0.0:7200"                 # bind addr
advertise_addr    = "10.0.0.20:7200"               # addr meta and clients use to reach this store
meta_addr         = "http://meta-host:7100"
ram_budget_bytes  = 8000000000                     # 8 GB; ~80% of host free
debug_http_listen = "127.0.0.1:7300"               # omit or set to null to disable; NO auth
```

Note: the heartbeat interval is hardcoded to 1 s. `ram_budget_bytes` is the only required
store-capacity knob.

### 6.3 `config/client.toml.example`

```toml
# dtmpfs FUSE client. One process per mount.
role                    = "client"
node_id                 = "client-a"
cluster_token           = "REPLACE-WITH-32-RANDOM-CHARS"
meta_addr               = "http://meta-host:7100"
mount_point             = "/mnt/dtmpfs"             # must exist, dir, empty, writable
block_size              = 1048576                   # MUST match cluster
replication_factor      = 1                         # 1..=3
attr_cache_ttl_ms       = 1000                      # also FUSE kernel attr_timeout
block_cache_capacity_mb = 1024                      # LRU bound; 0 disables
fuse_threads            = 4
tokio_worker_threads    = null                      # null = num_cpus
keepalive_interval_secs = 30
rpc_timeout_ms          = 5000                      # metadata RPCs
write_rpc_timeout_ms    = 30000                     # Store.WriteBlock

[mount_options]
allow_other         = true    # requires /etc/fuse.conf: user_allow_other
default_permissions = true
auto_unmount        = true    # set false if a supervisor handles unmount
no_atime            = true
```

For a rootless single-host setup where `user_allow_other` is not set in `/etc/fuse.conf`,
use `allow_other = false` and `auto_unmount = false`.

---

## 7. Validation

Implemented in `crates/dtmpfs-common/src/config.rs::Config::validate()`.
Returns `Result<(), DtmpfsError>`; failure aborts with exit code 1 and a
clear log line.

### 7.1 Checks

| Check | Applies to |
|---|---|
| `role` ∈ {meta, store, client} | all |
| `node_id` length 1..=63, charset `[a-z0-9-]+` | all |
| `cluster_token` length ≥ 16 | all |
| `listen` parses as `SocketAddr` | meta, store |
| `meta.heartbeat_timeout_ms` ≥ 1500 | meta |
| `meta.max_open_handles` ≥ 1 | meta |
| `store.meta_addr` parses, scheme http | store |
| `store.ram_budget_bytes` ≥ 1 MiB | store |
| `store.heartbeat_interval_ms` 100..=60000 | store |
| `client.meta_addr` parses, scheme http | client |
| `client.mount_point` exists, dir, empty, writable | client |
| `client.block_size` power of two, 64 KiB..16 MiB | client |
| `client.replication_factor` 1..=3 | client |
| `client.attr_cache_ttl_ms` 100..=60000 | client |
| `client.fuse_threads` 1..=256 | client |

Cross-key checks are local only — the store cannot see the meta's TOML,
so the heartbeat margin is best-effort.

### 7.2 Failure handling

```
ERROR: config validation failed: client.mount_point: /mnt/dtmpfs is not empty
```

Process exits with status 1. No auto-correction. Fix the TOML and retry.

### 7.3 Order

1. `toml::from_str`.
2. `serde(deny_unknown_fields)` rejects typos.
3. `Config::validate()`.
4. Bind `listen`, dial `meta_addr` — runtime errors look the same to the
   user.

---

## 8. Environment variable overrides

| Env var | Overrides | Format |
|---|---|---|
| `DTMPFS_LOG` | `log` | trace/debug/info/warn/error |
| `DTMPFS_CLUSTER_TOKEN` | `cluster_token` | string ≥ 16 chars |
| `RUST_LOG` | per-module log filter | e.g. `tonic=trace,dtmpfs=debug` |

`RUST_LOG` is finer-grained and wins over `DTMPFS_LOG`.

### 8.1 Precedence

```
CLI flag  >  env var  >  config file  >  built-in default
```

v1 has only `--config <path>`; the rule is for forward-compat.

### 8.2 Examples

```
# Quick debug session:
DTMPFS_LOG=debug RUST_LOG=tonic=info,dtmpfs=trace \
    /usr/local/bin/dtmpfs-mount --config ~/.config/dtmpfs/client.toml

# Token off-disk:
DTMPFS_CLUSTER_TOKEN=$(cat /run/secrets/dtmpfs-token) \
    /usr/local/bin/metasrv --config ~/.config/dtmpfs/meta.toml
```

In a systemd unit:

```ini
[Service]
Environment=DTMPFS_LOG=info
EnvironmentFile=/run/secrets/dtmpfs.env       # DTMPFS_CLUSTER_TOKEN=...
```

---

## 9. Reload

v1 has **no live reload**. SIGHUP is ignored. Reload = process restart:

- meta restart → entire FS becomes empty
  ([`failure-model.md`](failure-model.md) §3.2).
- store restart → that store's blocks are lost (R=1) or tolerated (R≥2
  with Phase-6 client).
- client restart → mount briefly disappears; AutoUnmount keeps mount
  point clean.

Phase 6+ may reload a subset: `log` (trivial), `replication_factor`
(applies to new files on next `Open`), `attr_cache_ttl_ms` (next
Get/Open). `mount_options` and `block_size` cannot be live-reloaded;
`cluster_token` may be addable at Phase 7+. Token-rotation procedure:
[`operations.md`](operations.md) §10.1.

---

## See also

- [`HLD.md`](HLD.md) §10 — design rationale for these defaults.
- [`operations.md`](operations.md) §3 and §4 — how to assemble TOMLs into
  a working deployment.
- [`failure-model.md`](failure-model.md) §6 — operator-error scenarios
  caused by misconfigured TOML.
- [`protocol.md`](protocol.md) — wire-protocol invariants the TOML must
  match (block_size, cluster_token).
- [`README.md`](../README.md) — quickstart that uses the example TOMLs
  verbatim.
</content>
</invoke>