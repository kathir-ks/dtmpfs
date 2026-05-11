# dtmpfs — Config / Defaults / Values Consistency Audit

Scope: `configuration.md` (canonical), `operations.md`, `HLD.md`, `README.md`,
`testing.md`, `acceptance-tests.md`, `architecture.md`, `LLD.md`. Other docs
(`failure-model.md`, `protocol.md`, `consistency.md`) are referenced where
they participate in a discrepancy.

Key conventions: `configuration.md` is the canonical reference. Anywhere
another doc disagrees, the other doc is the offender.

---

## 1. Values table

Legend: `OK` = matches canonical / expected; `—` = not mentioned; otherwise
the cell shows what the doc actually says (with line number).

| Setting | Expected | configuration.md | operations.md | HLD.md | README.md | testing.md | acceptance-tests.md | architecture.md | LLD.md |
|---|---|---|---|---|---|---|---|---|---|
| Meta listen port | 7100 | OK (L104, L374) | OK (L56, L135) | OK (L94, L254) | — | — | OK (L56) | OK (L74) | OK (L472 comment) |
| Store listen port (first) | 7200 | OK (L390) | OK (L149) | OK (L94) | OK (L86) | — | OK (L58) | OK (L75) | OK (L485) |
| Store listen port (second, single host) | 7201 | OK (L164) | OK (L164) | OK (L241) | OK (L86) | — | OK (L59) | — | — |
| `block_size` | 1 MiB / 1048576 | OK (L244) | OK (L184) | OK (L26, L44) | OK (L11) | OK (L252) | OK (L63) | OK (L412) | OK (L477, L513) |
| `replication_factor` | 1 | OK (L258) | OK (L185) | OK §6.2 implicit | — | OK (L253) | OK (L62) | — | OK (L512) |
| `attr_cache_ttl_ms` | 1000 | OK (L270) | OK (L186) | OK (L54: "1 s TTL") | — | OK (1000 in 6.2 / 250 test override) | OK (L63) | OK (L466 "1s") | OK (L505, L515) |
| `block_cache_capacity_mb` | 1024 | OK (L283) | OK (L187) | OK (L152: 1024 MiB) | — | — | — | OK (L484) | OK (L507, L516) |
| `fuse_threads` | 4 | OK (L294) | OK (L188) | — | — | — | — | OK (L532, L26) | OK (L509, L517) |
| `heartbeat_interval_ms` | 1000 | OK (L190) | OK (L154, L169) | OK (L68 "every 1s", L388) | — | **conflict** (L521 "5 s") | — | OK (L388, L95 "every 1 s") | hardcoded `Duration::from_secs(1)` (L819, L1138); not in config |
| `heartbeat_timeout_ms` | 5000 | OK (L119) | OK (L138) | OK (I8: 5000) | — | OK (L1935 test 1000) | OK (L1935 test 1000) | OK (L361 "~5 s") | **renamed `heartbeat_dead_ms`** (L478, L514) |
| `ram_budget_bytes` (store) | 8_000_000_000 (8 GB) | OK (L177) | OK (L154, L168) | **conflict** (uses `store.capacity_mb`, L126, L151, L268) | — | — | **key shortened to `ram_budget`** (L60, L1824, L1958) | — | OK (L489) |
| `cluster_token` min length | 16 chars | OK (L79) | comment "≥ 16; same on every role" (L373) | — | — | — | uses `"test-token"` (10 chars) — violates min in test fixtures (L64) | — | not enforced in struct (L473, L488, L499) |
| `node_id` charset & max | `[a-z0-9-]+`, 63 chars | OK (L62) | uses `client-local`, `meta-0`, `store-0` | uses same | uses same | — | uses same | uses same | newtype, no validation shown |
| `rpc_timeout_ms` | 5000 | OK (L341) | OK (L595) | — | — | — | — | OK (L100, L507) | not in `ClientConfig` (L494-510) |
| `write_rpc_timeout_ms` | 30000 | OK (L353) | OK (L596) | — | — | — | — | OK (L100, L507) | not in `ClientConfig` |
| FUSE `entry_timeout`/`attr_timeout` | 1 s | tied to attr_cache_ttl_ms (L273) | OK (L583) | OK (L443: 1 s) | — | — | — | OK (L466) | not modeled |
| Mount point in examples | `/mnt/dtmpfs` | OK (L411) | OK (L80, L411) | OK (L255) | OK (L58, L101) | tempdir (test) | OK | OK | OK (L497) |
| Workspace crates | `dtmpfs-{proto,common,meta,store,client}` | — | — | — | OK (L152-157) | — | — | — | OK (L37-42) |
| Binary names | `metasrv`, `storesrv`, `dtmpfs-mount` | OK (L501, L505, L501) | OK (L90) | — | **conflict** (L10: "`dtmpfs-meta`, `dtmpfs-store`, `dtmpfs-mount`"); L72-74 OK | OK (L98) | OK (L56-61) | — | OK (L162, L192, L225) |
| Rust version | 1.94 | — | OK (L47 "1.94.0") | — | OK (L47) | OK (L333, L378) | — | — | OK (L48 "1.94", L282 "1.94.0") |
| `fuser` crate version | 0.14 | — | — | — | — | — | — | OK (L532) | OK (L73) |
| `tonic` version | 0.12 | — | — | — | — | — | — | — | OK (L9, L54) |

Notes on borderline cells:

- `attr_cache_ttl_ms`: testing.md L521-523 says "Production heartbeat is 5 s; a store goes Down after 5 misses (25 s)" — that conflates `heartbeat_interval_ms` with `heartbeat_timeout_ms` and is mathematically wrong relative to the canonical defaults (1 s × 5 = 5 s, not 25 s).
- `heartbeat_interval_ms`: LLD does not expose this knob; the value is hardcoded as `Duration::from_secs(1)` in two places (`heartbeat::spawn_watcher`, `spawn_heartbeat`). Configuration.md presents it as a tunable u64 with range `100..60000 ms` — cannot be honored by the LLD code as written.
- `cluster_token`: acceptance-tests.md fixtures use the 10-character `"test-token"` for `cluster_token` everywhere (`std-cluster` definition L64, A-002 token mismatch test). configuration.md mandates ≥ 16 chars; if validation runs at process startup the entire acceptance suite fails to launch.
- `ram_budget_bytes`: HLD.md uses a different field name (`store.capacity_mb`) **with different units** (MiB vs bytes) at three locations (§6.2 L126, L151; §8.3 L268).

---

## 2. CRITICAL — wrong key, wrong unit, or wrong port (will break working code or tests)

### C1. `cluster_token` metadata header name disagrees three ways across the docs.

- `configuration.md` §2.3 (L81): `Sent as x-cluster-token gRPC metadata header.`
- `operations.md` (L467, L513, L759, L769): `grpcurl ... -H 'x-cluster-token: <T>'`.
- `failure-model.md` (L444): `Every RPC carries the token in metadata header x-cluster-token.`
- vs.
- `HLD.md` §2 (L56): `a static shared secret carried in a gRPC metadata header (x-dtmpfs-token) on every RPC.`
- `architecture.md` (L45, L100, L512): `metadata.x-dtmpfs-token = <cluster_token>`.
- vs.
- `protocol.md` (L49, L53, L144): `cluster-token` (no `x-` prefix at all).

Three different header names. Only one can be implemented. The smoke-test
recipes in operations.md and the acceptance test for token mismatch
(A-002) both depend on whatever ends up in the code; if the code follows
HLD/architecture, the operations runbook commands silently send the
token under the wrong key and `grpcurl` health checks return
`Unauthenticated` against a healthy meta. **Pick one and propagate.**

### C2. `ram_budget_bytes` is renamed and re-united in HLD.md.

- `configuration.md` §4.2 names the key `ram_budget_bytes`, type u64, units bytes, default 8_000_000_000.
- `HLD.md` §6.2 (L126): `store.capacity_mb`, units MiB.
- `HLD.md` §6.2 (L151): `store.capacity_mb`.
- `HLD.md` §8.3 (L268): `(store.capacity_mb in store.toml)`.
- `LLD.md` §3.x (L489) uses `ram_budget_bytes` matching configuration.md.
- `acceptance-tests.md` (L60, L1824, L1958, L1972) shortens to `ram_budget` (no `_bytes`).

Three names (`ram_budget_bytes`, `capacity_mb`, `ram_budget`), two units
(bytes, MiB). The LLD struct will reject TOMLs that use the HLD name.

### C3. `heartbeat_timeout_ms` renamed `heartbeat_dead_ms` in LLD.md.

- `configuration.md` §3.1 (L119): `heartbeat_timeout_ms`, default 5000.
- `LLD.md` (L478-479, L514, L1997): `pub heartbeat_dead_ms: u64, // 5000` and the default fn `d_heartbeat_dead_ms`.

Same value, different key. TOMLs written against configuration.md hit
`deny_unknown_fields` and refuse to start.

### C4. `heartbeat_interval_ms` is a configuration knob but a hardcoded constant in LLD.

- `configuration.md` §4.3 (L190): `heartbeat_interval_ms`, type u64, default 1000, range 100..60000.
- `LLD.md` (L819 and L1138): `let mut tick = tokio::time::interval(Duration::from_secs(1));` — not parametrised; not present in `StoreConfig`.

Tests (testing.md L526) advertise `heartbeat_ms = 200` as a per-test
override. With LLD as written, the override is silently ignored and the
"wait for membership change" assertions would race against the real 1 s
tick.

### C5. `meta.listen` / `store.listen` keys renamed in LLD.

- `configuration.md` §2.5 (L97-106): `listen` (`SocketAddr`) for meta and store; `meta_addr` URL on store/client.
- `LLD.md`: `MetaConfig.bind_addr` (L472), `StoreConfig.bind_addr` + `advertise_addr` (L485-486), `StoreConfig.debug_http_bind` (L491).
- `configuration.md` §4.4: `debug_http_listen`.

Different key names (`listen` vs `bind_addr`, `debug_http_listen` vs
`debug_http_bind`). Plus LLD adds an undocumented `advertise_addr` that
configuration.md never mentions. TOMLs from `configuration.md`'s §6
examples will not deserialize.

### C6. README binary-name list contradicts itself.

- `README.md` L10: "...three binaries: `dtmpfs-meta`, `dtmpfs-store`, `dtmpfs-mount`."
- `README.md` L72-74: "Artifacts land in `target/release/`: `metasrv`, `storesrv`, `dtmpfs-mount`."
- Everywhere else (operations.md L90, L116, acceptance-tests.md L56-61, LLD.md L162/L192/L225): `metasrv`, `storesrv`, `dtmpfs-mount`.

The L10 list uses crate names where binary names belong; new readers
will type `./target/release/dtmpfs-meta` and get "no such file".

### C7. acceptance-tests.md uses store debug HTTP on the gRPC port.

- `configuration.md` §4.4 / `operations.md` §3.1: `debug_http_listen = "127.0.0.1:7300"` for store-0, `7301` for store-1; `debug_http_listen` is a **separate** port from `listen`.
- `architecture.md` §6.3 (L519-523): explicit table: gRPC `7200 + idx`, debug HTTP `7300 + idx`.
- `acceptance-tests.md` L936-937, L987-988, L991-992, L1017-1025, L1052-1053, L1072-1074, L1162, L1557-1558, L1829: `curl http://127.0.0.1:7200/debug/blocks` (gRPC port).
- Compatible: `failure-model.md` L737 also uses `:7200/debug/blocks` (same bug); L376, L568, L737 elsewhere use `:7300`.

A-070 / A-071 / A-072 / A-073 / A-090 / A-184 will all fail with
`Connection refused` because there is no HTTP server on the gRPC
listener.

### C8. acceptance-tests.md hits non-existent meta debug HTTP endpoints.

- `configuration.md` §6 / `operations.md` §6.3 (L463): "v1 meta has no debug HTTP. Use grpcurl"; only Phase 6 adds `/debug/inodes` and `/debug/handles`.
- `HLD.md` §11A L539: meta exposes `/debug/state` (only).
- `acceptance-tests.md` L1530 (`/debug/inode?ino=...`), L1941, L1944 (`/debug/nodes`): hits both endpoints under v1 std-cluster preconditions.

Same failure mode as C7. The test author appears to have invented endpoints.

### C9. Operations.md firewall table contradicts the operations.md single-host example for store ports.

- `operations.md` §1.4 (L57): `store-N | 7200+N/tcp ...`.
- `operations.md` §3.1 (L149, L164): store-0 listens 7200, store-1 listens 7201 (matches "7200+N").
- `operations.md` §4.1 (L244): the multi-host topology diagram shows all three stores on port 7200, with the same comment.

Internally inconsistent. Multi-host §4.1 is correct (each host can
re-use 7200 since they are different hosts), but the §1.4 table reads
as if the same host has store-N at 7200+N. Mixing the two confuses
readers reading top-to-bottom.

### C10. testing.md misstates production heartbeat.

- `testing.md` §6.1 (L521-523): `Production heartbeat is 5 s; a store goes Down after 5 misses (25 s).`
- Canonical (configuration.md, HLD.md): interval 1 s, timeout 5 s.

Off by 5×. Documents a wrong production-default to readers.

### C11. operations.md L784 mis-attributes a feature to Phase 2.

- `operations.md` §12.3 (L784): `# Phase 2 will add /mnt/dtmpfs/.health served entirely client-side.`
- `HLD.md` §11 phase table: P2 is "Split client / store; gRPC; data on one store". No mention of a `.health` file.
- A client-side health probe is more naturally a Phase 6+ hardening item.

The phase number in operations.md does not match the HLD roadmap.

---

## 3. MINOR — wrong but non-breaking (silent inconsistency, missing fields, awkward naming)

### M1. LLD.md's `MetaConfig` includes `replication_factor` and `block_size`; configuration.md puts them only on `[client]`.

- `LLD.md` L474-477 attaches both to `MetaConfig`.
- `configuration.md` §3 lists meta keys as `heartbeat_timeout_ms`, `gc_interval_ms`, `max_open_handles` — and explicitly reserves `[meta.metrics]`, `[meta.raft]`, `[meta.persist]`. Neither `replication_factor` nor `block_size` appears under `[meta]`.

Open question OQ-9 in HLD §13 ("enforce that meta records the cluster's block size in its config") is partially implemented in LLD but never documented in configuration.md, leaving readers with a half-spec.

### M2. configuration.md mentions `gc_interval_ms` and `max_open_handles`; LLD.md's `MetaConfig` has neither.

- `configuration.md` §3.2, §3.3 document the keys (with defaults 60000 and 100000).
- `LLD.md` `MetaConfig` (L470-480) is missing both. Validation in §7.1 references `meta.max_open_handles` but the struct doesn't declare it.

If configuration.md is right, LLD's struct is missing fields and the validation table is unreachable. If LLD is right, the configuration reference advertises non-existent keys.

### M3. configuration.md `[client]` section advertises six fields LLD's `ClientConfig` does not have.

Missing from LLD `ClientConfig` (L494-510): `[client.mount_options]` table (the four FUSE bool flags), `tokio_worker_threads`, `keepalive_interval_secs`, `rpc_timeout_ms`, `write_rpc_timeout_ms`, `log` (top-level), `listen` ignored note. These are documented as v1 with defaults.

### M4. acceptance-tests.md's std-cluster uses `cluster_token = "test-token"` (10 chars).

- `acceptance-tests.md` L64.
- `configuration.md` §2.3 mandates `cluster_token` length ≥ 16.
- `configuration.md` §7.1 lists this as a hard validation: `cluster_token length ≥ 16` for all roles.

If validation is on, every acceptance test fails before reaching its assertions. testing.md (L253) has the same default for the `TestCluster` builder.

### M5. acceptance-tests.md uses `ram_budget` (no `_bytes`) and `heartbeat_ms`.

- `acceptance-tests.md` L60: `ram_budget = 1 GiB`. L1824, L1958, L1972 same.
- `acceptance-tests.md` L1935: `heartbeat_ms = 200`.

If those are TOML keys, they collide with `deny_unknown_fields`. If they are prose shorthand, they are still confusing — every other doc uses the canonical names.

### M6. README.md repository-layout box mentions only the binaries, not `metasrv`/`storesrv` mapping.

- `README.md` L155-157 shows crate dirs `dtmpfs-meta/`, `dtmpfs-store/`, `dtmpfs-client/` — and adds inline comments `# bin: metasrv`, `# bin: storesrv`, `# bin: dtmpfs-mount`. Good.
- Earlier L10 contradicts this (see C6).

### M7. LLD.md never models the metadata header name.

LLD §1.x and the request-handling sketches do not show the gRPC metadata interceptor for the cluster token at all. Given C1 (three header-name camps), LLD silence means the implementer picks something and only one of the docs will be right by accident.

### M8. operations.md §1.4 mentions "store debug 7300+N/tcp (optional)" but the firewall rule is `7300:7399` (range of 100).

`operations.md` L294. Fine in practice but the `7200+N` line above limits stores to one per index ≤ 99 — same implicit cap is fine, just unstated.

### M9. HLD.md §11 phase table has only P1-P7 yet body text references Phase 8 and Phase 9.

- `HLD.md` L327: "...possible Phase 8."
- `HLD.md` L555: "Phase 8 stretch."
- `operations.md` L747: "Phase 8 may add metadata schema migrations."
- The roadmap table in HLD §11 (and the duplicate in README §"Phased roadmap") stops at P7.

Phase 8 either needs a row in the table or the prose should call it "post-v1 stretch".

### M10. Cluster topology examples — node_id consistency.

Expected "1 meta + 2 stores + 1 client" example uses `meta-0`, `store-0`, `store-1`, `client-a` per the audit prompt. Survey:

| Doc | Meta | Stores | Client |
|---|---|---|---|
| configuration.md §6 | `meta-0` | `store-0` | `client-a` |
| operations.md §3.1 (single host) | `meta-0` | `store-0`, `store-1` | `client-local` |
| operations.md §4 (multi-host) | meta-host | `store-{0,1,2}` (no node_id shown) | `client-{a,b}` |
| README.md §Run a 3-process | (uses example tomls) | `store-0`, `store-1` | (default in client.toml.example) |
| testing.md `TestCluster` defaults | unnamed | unnamed | unnamed |
| acceptance-tests.md std-cluster | unnamed | `store-0`, `store-1` (in tests' kill commands) | `client-b` for second mount (A-050) |
| HLD.md §8.1 diagram | unnamed | `store-0`, `store-1` (port labels) | unnamed |

Single-host operations.md uses `client-local` while every other doc uses `client-a`. Minor; the README quickstart is the most-read page and uses the example TOML which has `client-a`, so the single-host example diverging is gratuitous.

---

## 4. COSMETIC — pure presentation issues

### D1. Inline TOML port comment in operations.md.

- `operations.md` L242-244 ASCII diagram aligns three stores all at `:7200`. Fine semantically (different hosts) but the §3.1 single-host example showed 7200 / 7201. Add a sentence.

### D2. Markdown table alignment in configuration.md §5.9 mount_options.

- `configuration.md` L314-319 uses `| Key | Type | Default | Maps to | Notes |` — fine, just the only spot in the doc with a 5-column table; other tables are 2-column key/value definitions. Minor stylistic difference.

### D3. configuration.md §8.1 precedence note caveat.

- `configuration.md` L488-494: documents `CLI flag > env var > config file > default`, then notes "v1 has only `--config <path>`; the rule is for forward-compat." Slightly confusing because `DTMPFS_LOG`, `DTMPFS_CLUSTER_TOKEN`, `RUST_LOG` are documented as live overrides — those are env vars, not CLI flags, so the rule does apply today.

### D4. testing.md L378 cites FUSE `3.10.5` while operations.md L21/L39 cite FUSE 3 / `≥ 5.4` (kernel) / `≥ 3.10` (libfuse3).

- All consistent with each other but use different precision; no "wrong" value.

### D5. README.md §"Phased roadmap" duplicates the HLD phase table verbatim except for P4's pass test text.

- `HLD.md` L517: P4 "Two clients see each other's writes".
- `README.md` L200: P4 "Two clients see each other's writes after close".

Minor wording difference; both correct, but it's a duplication that will drift.

### D6. systemd unit names — uniformly `dtmpfs-meta.service`, `dtmpfs-store@.service`, `dtmpfs-client.service` everywhere (operations.md §5, failure-model.md §3.x). No discrepancies. ✓

### D7. Path conventions — `~/dtmpfs` for the source repo (operations.md L48, L88), `/mnt/dtmpfs` for the mount (universal), `~/.config/systemd/user/` for unit files (operations.md §5.x). No discrepancies. ✓

### D8. Environment variables `DTMPFS_LOG`, `DTMPFS_CLUSTER_TOKEN`, `RUST_LOG`.

- `configuration.md` §8 documents all three.
- `operations.md` references `RUST_LOG` extensively, `DTMPFS_CLUSTER_TOKEN` once (L697). Doesn't reference `DTMPFS_LOG` outside of inheriting the precedence. OK.
- Other docs don't contradict. ✓

### D9. Cargo workspace crates list — uniform across LLD §1.1 and README repo layout. ✓

### D10. Rust version, `fuser` version, `tonic` version.

- Rust 1.94 — README L47, operations.md L47, LLD L48 / L282, testing.md L333 / L378. ✓
- `fuser` 0.14 — LLD L73, architecture.md L532. ✓
- `tonic` 0.12 — LLD L9 / L54. ✓

---

## 5. Summary of which docs need edits

- **HLD.md** — replace `store.capacity_mb` → `store.ram_budget_bytes` (3 sites: L126, L151, L268). Decide on `x-dtmpfs-token` vs `x-cluster-token` and update §2 / §6.2 / §F9 to match canonical. Either add a P8 row to the §11 table or downgrade the prose Phase-8 references to "post-v1".
- **LLD.md** — rename `heartbeat_dead_ms` → `heartbeat_timeout_ms`. Rename `bind_addr` → `listen` (or update configuration.md to use `bind_addr` everywhere). Rename `debug_http_bind` → `debug_http_listen`. Add `heartbeat_interval_ms` to `StoreConfig`; thread it through `spawn_heartbeat` instead of hardcoding 1 s. Add `gc_interval_ms`, `max_open_handles` to `MetaConfig`. Add `[client.mount_options]`, `tokio_worker_threads`, `keepalive_interval_secs`, `rpc_timeout_ms`, `write_rpc_timeout_ms`, `log` to `ClientConfig`. Drop `replication_factor` / `block_size` from `MetaConfig` or document them in configuration.md.
- **README.md** — fix L10 binary-name list to `metasrv`, `storesrv`, `dtmpfs-mount`.
- **operations.md** — fix §12.3 phase number (P2 → P6+) for `.health` file. Reconcile §1.4 firewall table with §4.1 multi-host port diagram.
- **testing.md** — fix §6.1 production heartbeat description (1 s interval, 5 s timeout). Increase the `TestCluster` default `cluster_token` to ≥ 16 chars (or relax the validation).
- **acceptance-tests.md** — change std-cluster `cluster_token` from `"test-token"` (10) to ≥ 16 chars. Use `ram_budget_bytes` and `heartbeat_interval_ms` (full names) when describing TOML. Move all `/debug/blocks` curls from store gRPC ports `:7200/:7201/:7202/:7203` to the documented debug HTTP ports `:7300/:7301/:7302/:7303`. Replace meta-side debug HTTP calls (`/debug/inode`, `/debug/nodes`) with grpcurl invocations (since v1 meta has no debug HTTP).
- **architecture.md** — align metadata header name with whatever HLD/configuration agree on. The §6.2 / §3 mentions of `x-dtmpfs-token` are otherwise correct.
- **failure-model.md** — fix the L737 `:7200/debug/blocks` to `:7300/debug/blocks`. Reconcile `x-cluster-token` (L444) with whatever HLD/configuration finally agree.
- **protocol.md** — drop the `cluster-token` (no `x-` prefix) form at L49/L53/L144 in favor of the agreed canonical header.
