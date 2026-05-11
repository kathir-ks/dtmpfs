# dtmpfs — Acceptance Test Suite

This document is the user-facing bar for dtmpfs. Each test below is
specified at copy-paste fidelity: setup commands, action commands,
exact expected output (or expected behaviour). A test passes only if
all "Expected" bullets match.

For background see:

- [testing.md](testing.md) — overall test strategy
- [HLD.md](HLD.md) — high-level design
- [LLD.md](LLD.md) — module-level design
- [protocol.md](protocol.md) — wire protocol
- [consistency.md](consistency.md) — close-to-open semantics
- [failure-model.md](failure-model.md) — failure modes and behaviour
- [operations.md](operations.md) — running the cluster
- [configuration.md](configuration.md) — TOML configuration
- [README.md](../README.md) — project overview


## How to read these tests

Each test follows the format:

```
### A-XXX: <one-line title>
**Phase**:        P1 / P2 / P3 / P4 / P5 / P6
**Category**:     Functional | Cross-host | Concurrency | Sharding |
                  Replication | Failure | POSIX | Performance |
                  Mount | Configuration
**Preconditions**:
- (e.g., 1 meta + 2 stores + 1 client running; mount at /mnt/dtmpfs)
**Steps**:
1. <command>
2. <command>
**Expected**:
- exit code 0
- stdout: "..."
- side effect: ...
**Pass criteria**:
- <how to decide pass vs fail>
**Notes**:
- <subtleties>
```

Where a step is preceded by `# user@hostA $`, that command runs on
host A; `# user@hostB $` runs on host B. A bare `$` runs on whichever
host the test focusses on.


## Standard preconditions

Unless overridden, every test assumes the following baseline cluster
("**std-cluster**"):

- 1 `metasrv` running on `127.0.0.1:7100` (config
  `config/meta.toml`).
- 2 `storesrv` running on `127.0.0.1:7200` and `127.0.0.1:7201`
  (configs `config/store0.toml`, `config/store1.toml`).
- Each store has a `ram_budget = 1 GiB`.
- 1 `dtmpfs-mount` client mounted on `/mnt/dtmpfs` (config
  `config/client.toml`), with `replication_factor = 1`,
  `block_size = 1048576`, `attr_cache_ttl_ms = 1000`.
- `cluster_token = "test-token"` everywhere.
- Mountpoint `/mnt/dtmpfs` is empty before the test.
- Test files are cleaned up between tests (`rm -rf /mnt/dtmpfs/*`).

When a test requires a non-default topology (e.g. R=2, 3 stores, two
clients), it says so under "Preconditions".


# Category: Mount lifecycle

### A-001: Cold mount succeeds
**Phase**: P1
**Category**: Mount
**Preconditions**:
- meta and stores running per std-cluster.
- `/mnt/dtmpfs` exists, is empty, owned by the running user.
- No client currently mounted there.

**Steps**:
1. `RUST_LOG=info ./target/release/dtmpfs-mount --config config/client.toml &`
2. `sleep 1`
3. `mountpoint -q /mnt/dtmpfs && echo MOUNTED`
4. `ls /mnt/dtmpfs`

**Expected**:
- Step 3 stdout: `MOUNTED`
- Step 3 exit code: `0`
- Step 4 stdout: empty (no entries other than the implicit `.`/`..`)
- Step 4 exit code: `0`
- Client log contains: `mounted dtmpfs at /mnt/dtmpfs`

**Pass criteria**:
- `mountpoint -q` returns 0; `ls` shows no entries; client log line
  present.

**Notes**:
- If `mountpoint` is missing on the host, substitute
  `findmnt /mnt/dtmpfs >/dev/null && echo MOUNTED`.


### A-002: Mount with bad cluster_token fails fast (EIO on first op)
**Phase**: P1
**Category**: Configuration / Mount
**Preconditions**:
- meta and stores running per std-cluster, with `cluster_token =
  "test-token"`.
- A separate client config `config/client-badtoken.toml` identical
  to `config/client.toml` except `cluster_token =
  "WRONG-TOKEN"`.

**Steps**:
1. `RUST_LOG=info ./target/release/dtmpfs-mount --config config/client-badtoken.toml &`
2. `sleep 1`
3. `ls /mnt/dtmpfs; echo exit=$?`
4. `cat /mnt/dtmpfs/anything 2>&1; echo exit=$?`

**Expected**:
- Step 3: `ls` returns non-zero with `Input/output error`. Final line
  contains `exit=5` (EIO).
- Step 4: `cat` prints `cat: /mnt/dtmpfs/anything: Input/output
  error`. Final line `exit=1`.
- Client log contains: `meta rejected RPC: Unauthenticated` (or
  similar).

**Pass criteria**:
- First operation on the mount returns `EIO` (libc 5).
- Client does not panic; mount remains live (just unusable).

**Notes**:
- We deliberately do not fail at mount time because the client cannot
  validate the token without an RPC, and we don't want a "ping" RPC
  during init. See [protocol.md](protocol.md) §"Auth".


### A-003: Mount on non-empty directory fails
**Phase**: P1
**Category**: Mount
**Preconditions**:
- meta and stores running per std-cluster.
- `/mnt/dtmpfs` exists and contains a file: `touch /mnt/dtmpfs/preexisting`.
- No client currently mounted there.

**Steps**:
1. `./target/release/dtmpfs-mount --config config/client.toml; echo exit=$?`

**Expected**:
- Stderr contains: `mount point /mnt/dtmpfs is not empty`.
- Final line: `exit=1` (or any non-zero).
- `mountpoint -q /mnt/dtmpfs` returns non-zero (not mounted).
- The pre-existing file is untouched.

**Pass criteria**:
- Process exits non-zero, no FUSE mount happens, file preserved.

**Notes**:
- `fuser` allows mount-on-non-empty with a flag; we deliberately do
  not pass it. See `crates/dtmpfs-client/src/fs.rs` `MountOption`
  list.


### A-004: Unmount via Ctrl-C cleans up via AutoUnmount
**Phase**: P1
**Category**: Mount
**Preconditions**:
- Client mounted per std-cluster, PID stored in `$CLIENT_PID`.

**Steps**:
1. `kill -INT $CLIENT_PID`
2. `sleep 1`
3. `mountpoint -q /mnt/dtmpfs; echo exit=$?`
4. `ps -p $CLIENT_PID; echo exit=$?`

**Expected**:
- Step 3 exit: non-zero (not mounted).
- Step 4 exit: non-zero (process gone).
- No leftover `fusermount3` lock files.
- Client log final lines include: `received SIGINT, unmounting` and
  `unmounted /mnt/dtmpfs`.

**Pass criteria**:
- After SIGINT, mount is gone within 1 s and process exits cleanly.

**Notes**:
- `MountOption::AutoUnmount` is what makes this work even if the
  client crashes; SIGINT is the normal-shutdown path.


### A-005: Re-mount after unmount works
**Phase**: P1
**Category**: Mount
**Preconditions**:
- A-004 has just completed; mountpoint is unmounted but exists.

**Steps**:
1. `RUST_LOG=info ./target/release/dtmpfs-mount --config config/client.toml &`
2. `sleep 1`
3. `mountpoint -q /mnt/dtmpfs && echo MOUNTED`
4. `echo round2 > /mnt/dtmpfs/x && cat /mnt/dtmpfs/x`

**Expected**:
- Step 3 stdout: `MOUNTED`.
- Step 4 stdout: `round2`.
- All steps exit 0.

**Pass criteria**:
- Mount succeeds and basic write/read works.

**Notes**:
- This catches the bug where unmount leaves stale state in the meta
  about the previous client's open handles.


# Category: File ops (Functional / POSIX)

### A-010: Create empty file via touch
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `touch /mnt/dtmpfs/empty`
2. `stat -c '%s %F' /mnt/dtmpfs/empty`
3. `ls -l /mnt/dtmpfs/empty`

**Expected**:
- Step 2 stdout: `0 regular empty file`.
- Step 3 stdout matches: `^-rw-r--r-- 1 .* 0 .* /mnt/dtmpfs/empty$`.
- Step 1, 2, 3 exit 0.

**Pass criteria**:
- File exists, size 0, is regular, mode `0644` (subject to umask).

**Notes**:
- Default mode is `0666 & ~umask`; with default umask `022` that's
  `0644`.


### A-011: Write small file (single block) via echo
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `echo hello > /mnt/dtmpfs/x`
2. `cat /mnt/dtmpfs/x`
3. `stat -c '%s' /mnt/dtmpfs/x`

**Expected**:
- Step 2 stdout: `hello`.
- Step 3 stdout: `6` (5 chars + newline from `echo`).
- All exit 0.

**Pass criteria**:
- Content matches exactly; size matches exactly.


### A-012: Read small file via cat
**Phase**: P1
**Category**: Functional
**Preconditions**:
- A-011 has just run; `/mnt/dtmpfs/x` contains `hello\n`.

**Steps**:
1. `cat /mnt/dtmpfs/x`
2. `head -c 5 /mnt/dtmpfs/x; echo`
3. `tail -c 1 /mnt/dtmpfs/x | xxd`

**Expected**:
- Step 1 stdout: `hello`.
- Step 2 stdout: `hello`.
- Step 3 stdout: `00000000: 0a                                       .`
- All exit 0.

**Pass criteria**:
- File content readable through any standard tool.


### A-013: Write large file (200 MiB) via dd
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster; each store has ≥ 256 MiB free.

**Steps**:
1. `dd if=/dev/urandom of=/tmp/src bs=1M count=200 status=none`
2. `cp /tmp/src /mnt/dtmpfs/big`
3. `stat -c '%s' /mnt/dtmpfs/big`

**Expected**:
- Step 3 stdout: `209715200`.
- Steps 1, 2, 3 exit 0.
- No error in client, store, meta logs.

**Pass criteria**:
- File of exactly 200 MiB created.

**Notes**:
- `block_size = 1 MiB`, so this should occupy 200 blocks.


### A-014: Read large file matches md5
**Phase**: P1
**Category**: Functional
**Preconditions**:
- A-013 has just run.

**Steps**:
1. `md5sum /tmp/src | awk '{print $1}' > /tmp/src.md5`
2. `md5sum /mnt/dtmpfs/big | awk '{print $1}' > /tmp/big.md5`
3. `diff /tmp/src.md5 /tmp/big.md5; echo exit=$?`

**Expected**:
- Step 3 stdout: `exit=0`.
- `diff` produces no output.

**Pass criteria**:
- MD5 of source and the file read back are identical.


### A-015: Append (>>) extends file
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `echo aaa > /mnt/dtmpfs/app`
2. `echo bbb >> /mnt/dtmpfs/app`
3. `cat /mnt/dtmpfs/app`
4. `stat -c '%s' /mnt/dtmpfs/app`

**Expected**:
- Step 3 stdout (two lines): `aaa\nbbb`.
- Step 4 stdout: `8` (3 + 1 + 3 + 1).

**Pass criteria**:
- Append produces concatenated content.


### A-016: Overwrite via > truncates and rewrites
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `echo longerlonger > /mnt/dtmpfs/o`
2. `echo hi > /mnt/dtmpfs/o`
3. `cat /mnt/dtmpfs/o`
4. `stat -c '%s' /mnt/dtmpfs/o`

**Expected**:
- Step 3 stdout: `hi`.
- Step 4 stdout: `3`.

**Pass criteria**:
- File replaced; size shrinks; no trailing bytes from the previous
  content.

**Notes**:
- Internally this is `O_TRUNC` on open; meta drops blocks ≥ new size
  (i.e. all blocks here) on `setattr(size=0)` followed by writes.


### A-017: `truncate -s 0 x` zeroes file
**Phase**: P1
**Category**: Functional / POSIX
**Preconditions**:
- std-cluster; `/mnt/dtmpfs/t` contains 1 MiB of data.

**Steps**:
1. `dd if=/dev/urandom of=/mnt/dtmpfs/t bs=1M count=1 status=none`
2. `truncate -s 0 /mnt/dtmpfs/t`
3. `stat -c '%s' /mnt/dtmpfs/t`
4. `wc -c < /mnt/dtmpfs/t`

**Expected**:
- Step 3 stdout: `0`.
- Step 4 stdout: `0`.

**Pass criteria**:
- Size becomes 0; `cat` returns nothing; meta has freed all blocks
  for that inode (verifiable in store logs at `RUST_LOG=debug`).


### A-018: `truncate -s 100M x` extends
**Phase**: P1
**Category**: Functional / POSIX
**Preconditions**:
- std-cluster; `/mnt/dtmpfs/sparse` does not exist.

**Steps**:
1. `touch /mnt/dtmpfs/sparse`
2. `truncate -s 100M /mnt/dtmpfs/sparse`
3. `stat -c '%s' /mnt/dtmpfs/sparse`
4. `md5sum /mnt/dtmpfs/sparse | awk '{print $1}'`

**Expected**:
- Step 3 stdout: `104857600`.
- Step 4 stdout: `2f282b84e7e608d5852449ed940bfc51` (MD5 of 100 MiB of
  zeroes).

**Pass criteria**:
- File reads back as 100 MiB of zero bytes; size matches.

**Notes**:
- v1 dtmpfs **materializes** zeros: extending allocates blocks. This
  costs RAM. A future optimization (sparse blocks / hole representation)
  is tracked in the v2 backlog. See [HLD.md](HLD.md) §"Block storage".


### A-019: Multiple writes within one open
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `python3 -c '
import os
fd = os.open("/mnt/dtmpfs/multi", os.O_WRONLY|os.O_CREAT|os.O_TRUNC, 0o644)
for i in range(10):
    os.write(fd, f"chunk{i}\n".encode())
os.close(fd)
'`
2. `cat /mnt/dtmpfs/multi`

**Expected**:
- Step 2 stdout (exact, 10 lines): `chunk0\nchunk1\nchunk2\nchunk3\nchunk4\nchunk5\nchunk6\nchunk7\nchunk8\nchunk9`.
- Both steps exit 0.

**Pass criteria**:
- All 10 chunks appear in order.

**Notes**:
- Per close-to-open semantics, dirty blocks are buffered until
  `close`; this test verifies the buffering works for many small
  writes.


### A-020: Random-offset writes (RMW path)
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `dd if=/dev/zero of=/mnt/dtmpfs/r bs=1M count=2 status=none`
2. `python3 -c '
import os
fd = os.open("/mnt/dtmpfs/r", os.O_WRONLY)
os.lseek(fd, 1500000, os.SEEK_SET)   # offset inside block 1
os.write(fd, b"X" * 16)
os.close(fd)
'`
3. `xxd -s 1499998 -l 20 /mnt/dtmpfs/r`

**Expected**:
- Step 3 stdout (exact): `0016e9be: 0000 5858 5858 5858 5858 5858 5858 5858  ..XXXXXXXXXXXXXX`
- Step 3 exit 0.

**Pass criteria**:
- Bytes at offset 1500000..1500016 are 'X', surrounding bytes
  unchanged (zero).

**Notes**:
- This exercises the **read-modify-write** path: writing inside an
  existing block reads the full block from the store first, modifies
  in memory, writes back.


### A-021: chmod changes mode and persists
**Phase**: P1
**Category**: Functional / POSIX
**Preconditions**:
- std-cluster.

**Steps**:
1. `touch /mnt/dtmpfs/mode`
2. `chmod 0600 /mnt/dtmpfs/mode`
3. `stat -c '%a' /mnt/dtmpfs/mode`
4. `chmod 0755 /mnt/dtmpfs/mode`
5. `stat -c '%a' /mnt/dtmpfs/mode`

**Expected**:
- Step 3 stdout: `600`.
- Step 5 stdout: `755`.

**Pass criteria**:
- `stat` reports the mode just set.


### A-022: chown (no-op for root, document)
**Phase**: P1
**Category**: Functional / POSIX
**Preconditions**:
- std-cluster.

**Steps**:
1. `touch /mnt/dtmpfs/own`
2. `stat -c '%u %g' /mnt/dtmpfs/own`
3. `chown $(id -u):$(id -g) /mnt/dtmpfs/own; echo exit=$?`
4. `stat -c '%u %g' /mnt/dtmpfs/own`

**Expected**:
- Step 2 and 4 stdout: `<euid> <egid>` for the running user.
- Step 3 stdout: `exit=0`.

**Pass criteria**:
- chown to self is a no-op succeeding silently.

**Notes**:
- We do **not** support changing uid/gid in v1; `chown` to a different
  user returns `EPERM`. This is intentional: dtmpfs has no real user
  database. See [consistency.md](consistency.md) §"Permissions".


### A-023: Stat reports correct size, mtime, generation
**Phase**: P4
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `T0=$(date +%s)`
2. `echo abc > /mnt/dtmpfs/s`
3. `stat -c '%s %Y' /mnt/dtmpfs/s`
4. `echo defg >> /mnt/dtmpfs/s`
5. `stat -c '%s %Y' /mnt/dtmpfs/s`

**Expected**:
- Step 3: size `4`, mtime ≥ `$T0`, ≤ `$T0 + 5`.
- Step 5: size `9`, mtime ≥ step-3 mtime.

**Pass criteria**:
- Size and mtime advance with writes.

**Notes**:
- Generation is not exposed via `stat` (it's an internal protocol
  field), but is asserted via `Meta.GetAttr` in the integration test
  variant of this case.


### A-024: Delete via rm
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `echo bye > /mnt/dtmpfs/d`
2. `ls /mnt/dtmpfs/d`
3. `rm /mnt/dtmpfs/d`
4. `ls /mnt/dtmpfs/d 2>&1; echo exit=$?`

**Expected**:
- Step 2 stdout: `/mnt/dtmpfs/d`.
- Step 4 stdout: `ls: cannot access '/mnt/dtmpfs/d': No such file or
  directory`. Final line `exit=2`.

**Pass criteria**:
- After rm, `ls` errors with ENOENT (libc 2).


### A-025: Delete-then-recreate yields a new inode
**Phase**: P4
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `echo first > /mnt/dtmpfs/n`
2. `INO1=$(stat -c '%i' /mnt/dtmpfs/n)`
3. `rm /mnt/dtmpfs/n`
4. `echo second > /mnt/dtmpfs/n`
5. `INO2=$(stat -c '%i' /mnt/dtmpfs/n)`
6. `[ "$INO1" != "$INO2" ] && echo DIFFERENT || echo SAME`

**Expected**:
- Step 6 stdout: `DIFFERENT`.

**Pass criteria**:
- Inode number changes across delete+create.

**Notes**:
- Important for tools that key on `(dev, ino)`. The meta server
  guarantees `next_ino` strictly increases (no reuse) within a single
  meta lifetime.


# Category: Directory ops

### A-030: mkdir
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `mkdir /mnt/dtmpfs/d`
2. `stat -c '%F %a' /mnt/dtmpfs/d`

**Expected**:
- Step 2 stdout: `directory 755` (subject to umask).

**Pass criteria**:
- Directory created with default mode.


### A-031: mkdir -p for nested
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `mkdir -p /mnt/dtmpfs/a/b/c/d`
2. `find /mnt/dtmpfs -mindepth 1 -type d | sort`

**Expected**:
- Step 2 stdout (exact, 4 lines):
  ```
  /mnt/dtmpfs/a
  /mnt/dtmpfs/a/b
  /mnt/dtmpfs/a/b/c
  /mnt/dtmpfs/a/b/c/d
  ```

**Pass criteria**:
- All four directories created.


### A-032: rmdir of empty
**Phase**: P1
**Category**: Functional
**Preconditions**:
- A-030 just ran (`/mnt/dtmpfs/d` exists, empty).

**Steps**:
1. `rmdir /mnt/dtmpfs/d`
2. `ls /mnt/dtmpfs/d 2>&1; echo exit=$?`

**Expected**:
- Step 2 final line: `exit=2`.
- Stderr: `ls: cannot access ...: No such file or directory`.

**Pass criteria**:
- Directory gone.


### A-033: rmdir of non-empty fails with ENOTEMPTY
**Phase**: P1
**Category**: Functional / POSIX
**Preconditions**:
- std-cluster.

**Steps**:
1. `mkdir /mnt/dtmpfs/full`
2. `touch /mnt/dtmpfs/full/x`
3. `rmdir /mnt/dtmpfs/full 2>&1; echo exit=$?`

**Expected**:
- Step 3 stdout: `rmdir: failed to remove '/mnt/dtmpfs/full':
  Directory not empty`. Final line `exit=1`.

**Pass criteria**:
- Errno is `ENOTEMPTY` (39); rmdir does not delete.


### A-034: rm -rf of nested
**Phase**: P1
**Category**: Functional
**Preconditions**:
- A-031 just ran (`/mnt/dtmpfs/a/b/c/d` exists).

**Steps**:
1. `touch /mnt/dtmpfs/a/file`
2. `touch /mnt/dtmpfs/a/b/file`
3. `rm -rf /mnt/dtmpfs/a`
4. `ls /mnt/dtmpfs/a 2>&1; echo exit=$?`

**Expected**:
- Step 4 final line: `exit=2`.

**Pass criteria**:
- Entire tree gone.


### A-035: ls /mnt/dtmpfs shows entries
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster (mountpoint freshly empty).

**Steps**:
1. `touch /mnt/dtmpfs/{a,b,c}`
2. `ls /mnt/dtmpfs | sort`

**Expected**:
- Step 2 stdout (exact, 3 lines): `a\nb\nc`.

**Pass criteria**:
- All three entries listed.

**Notes**:
- Order is not guaranteed by FUSE, hence `| sort`.


### A-036: find /mnt/dtmpfs traverses correctly
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster (mountpoint freshly empty).

**Steps**:
1. `mkdir -p /mnt/dtmpfs/x/y/z`
2. `touch /mnt/dtmpfs/x/y/file /mnt/dtmpfs/x/top`
3. `find /mnt/dtmpfs | sort`

**Expected**:
- Step 3 stdout (exact, 6 lines):
  ```
  /mnt/dtmpfs
  /mnt/dtmpfs/x
  /mnt/dtmpfs/x/top
  /mnt/dtmpfs/x/y
  /mnt/dtmpfs/x/y/file
  /mnt/dtmpfs/x/y/z
  ```

**Pass criteria**:
- All six paths listed.


### A-037: Directory rename across same parent
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `mkdir /mnt/dtmpfs/old`
2. `touch /mnt/dtmpfs/old/inside`
3. `mv /mnt/dtmpfs/old /mnt/dtmpfs/new`
4. `ls /mnt/dtmpfs/new`
5. `ls /mnt/dtmpfs/old 2>&1; echo exit=$?`

**Expected**:
- Step 4 stdout: `inside`.
- Step 5 final line: `exit=2`.

**Pass criteria**:
- `new/inside` exists; `old` does not.


### A-038: Directory rename across different parents
**Phase**: P1
**Category**: Functional
**Preconditions**:
- std-cluster.

**Steps**:
1. `mkdir -p /mnt/dtmpfs/p1/sub /mnt/dtmpfs/p2`
2. `mv /mnt/dtmpfs/p1/sub /mnt/dtmpfs/p2/sub`
3. `ls /mnt/dtmpfs/p1`
4. `ls /mnt/dtmpfs/p2`

**Expected**:
- Step 3 stdout: empty (no entries).
- Step 4 stdout: `sub`.

**Pass criteria**:
- Subtree moved across parents atomically.


# Category: Cross-host visibility (Phase 4+)

### A-050: Two clients on same VM, A writes + closes, B reads
**Phase**: P4
**Category**: Cross-host
**Preconditions**:
- std-cluster, plus a second client mounted on `/mnt/dtmpfs-b` using
  `config/client-b.toml` (different `node_id = "client-b"`, same
  `meta_addr` and `cluster_token`).

**Steps**:
1. `echo from-a > /mnt/dtmpfs/cross`
2. `sync`
3. `cat /mnt/dtmpfs-b/cross`

**Expected**:
- Step 3 stdout: `from-a`.
- All exit 0.

**Pass criteria**:
- Client B sees client A's write within a fresh `open` after the
  `sync` barrier.

**Notes**:
- `sync` here flushes the kernel buffer; close-to-open invalidation
  ensures B's `open` re-fetches attr+block_map from meta.


### A-051: A appends, sync, B reads
**Phase**: P4
**Category**: Cross-host
**Preconditions**:
- A-050 just ran. `/mnt/dtmpfs/cross` contains `from-a\n`.

**Steps**:
1. `echo more >> /mnt/dtmpfs/cross`
2. `sync`
3. `cat /mnt/dtmpfs-b/cross`

**Expected**:
- Step 3 stdout (two lines): `from-a\nmore`.

**Pass criteria**:
- B sees the new full content.


### A-052: A creates dir, B sees dir
**Phase**: P4
**Category**: Cross-host
**Preconditions**:
- two clients mounted as in A-050.

**Steps**:
1. `mkdir /mnt/dtmpfs/shared-d`
2. `sync`
3. `sleep 1.1`   # AttrCache TTL on B's existing readdir
4. `ls /mnt/dtmpfs-b/`

**Expected**:
- Step 4 stdout includes `shared-d`.

**Pass criteria**:
- B's readdir reflects A's mkdir.

**Notes**:
- Step 3 is needed because `readdir` results are cached for
  `attr_cache_ttl_ms`. A `sync` does not invalidate readdir cache;
  a fresh `opendir` after the TTL does.


### A-053: A deletes, B's stale stat works for 1s, then errors
**Phase**: P4
**Category**: Cross-host / Consistency
**Preconditions**:
- two clients mounted; `/mnt/dtmpfs/del` exists with `hello\n` (created
  by A, both clients have stat-ed it, e.g. `cat`-ed).

**Steps**:
1. `cat /mnt/dtmpfs-b/del`         # primes B's AttrCache
2. `rm /mnt/dtmpfs/del`
3. `stat /mnt/dtmpfs-b/del; echo exit=$?`     # may still succeed
4. `sleep 1.1`
5. `stat /mnt/dtmpfs-b/del 2>&1; echo exit=$?`

**Expected**:
- Step 3 may succeed (`exit=0`) within the AttrCache TTL.
- Step 5 stdout: `stat: cannot statx ...: No such file or directory`.
  Final line `exit=1`.

**Pass criteria**:
- Within TTL, stale stat succeeds; after TTL, ENOENT.

**Notes**:
- This is the documented close-to-open behaviour. See
  [consistency.md](consistency.md) §"Stale attrs". A user that needs
  stronger guarantees should call `sync` after delete and `open`
  on the reader (which bypasses AttrCache).


### A-054: A renames, B sees new path
**Phase**: P4
**Category**: Cross-host
**Preconditions**:
- two clients mounted; `/mnt/dtmpfs/r` exists (`echo abc >
  /mnt/dtmpfs/r`).

**Steps**:
1. `mv /mnt/dtmpfs/r /mnt/dtmpfs/r-renamed`
2. `sync`
3. `sleep 1.1`     # entry cache TTL
4. `cat /mnt/dtmpfs-b/r-renamed`
5. `cat /mnt/dtmpfs-b/r 2>&1; echo exit=$?`

**Expected**:
- Step 4 stdout: `abc`.
- Step 5 stdout includes `No such file or directory`. Final line
  `exit=1`.

**Pass criteria**:
- New path resolves; old path errors after TTL.


### A-055: Multi-host (2 VMs): same as A-050 across hosts
**Phase**: P4
**Category**: Cross-host
**Preconditions**:
- meta and stores on host A.
- Client mounted on host A at `/mnt/dtmpfs`.
- Client mounted on host B at `/mnt/dtmpfs` (config points at host
  A's meta).
- Test runner has SSH to both.

**Steps**:
1. `# user@hostA $ echo cross-host > /mnt/dtmpfs/x`
2. `# user@hostA $ sync`
3. `# user@hostB $ cat /mnt/dtmpfs/x`

**Expected**:
- Step 3 stdout: `cross-host`.

**Pass criteria**:
- Host B reads host A's content.


# Category: Sharding (Phase 3+)

### A-070: Write 16 blocks, count is roughly even across 2 stores
**Phase**: P3
**Category**: Sharding
**Preconditions**:
- std-cluster (2 stores, R=1).

**Steps**:
1. `dd if=/dev/urandom of=/mnt/dtmpfs/sh bs=1M count=16 status=none`
2. `sync`
3. `S0=$(curl -s http://127.0.0.1:7200/debug/blocks | wc -l)`
4. `S1=$(curl -s http://127.0.0.1:7201/debug/blocks | wc -l)`
5. `echo "$S0 $S1"`
6. `python3 -c "
import sys
s0, s1 = map(int, '$S0 $S1'.split())
print('OK' if abs(s0 - s1) <= 4 and s0 + s1 == 16 else f'FAIL s0={s0} s1={s1}')"`

**Expected**:
- Step 6 stdout: `OK`.

**Pass criteria**:
- Total blocks across stores = 16. Each store holds 8 ± 2 (i.e. 6..10).

**Notes**:
- HRW with two nodes is a fair coin per key; with 16 trials, 8±2 is a
  ~95% interval.


### A-071: Write 256 blocks across 4 stores, max-min ≤ 20%
**Phase**: P3
**Category**: Sharding
**Preconditions**:
- 1 meta + **4 stores** (ports 7200..7203) + 1 client, R=1.

**Steps**:
1. `dd if=/dev/urandom of=/mnt/dtmpfs/big bs=1M count=256 status=none`
2. `sync`
3. `for p in 7200 7201 7202 7203; do
     curl -s http://127.0.0.1:$p/debug/blocks | wc -l
   done | tee /tmp/spread`
4. `python3 -c "
v = list(map(int, open('/tmp/spread')))
print('OK' if max(v) - min(v) <= 0.2 * (sum(v) / len(v)) and sum(v) == 256 else f'FAIL {v}')"`

**Expected**:
- Step 4 stdout: `OK`.

**Pass criteria**:
- Sum is 256, max-min ≤ 20% of mean (≤ ~13 blocks).


### A-072: Block placement is stable across rewrites of the same file
**Phase**: P3
**Category**: Sharding
**Preconditions**:
- std-cluster, 2 stores.

**Steps**:
1. `dd if=/dev/zero of=/mnt/dtmpfs/stable bs=1M count=4 status=none`
2. `sync`
3. `curl -s http://127.0.0.1:7200/debug/blocks > /tmp/before-0`
4. `curl -s http://127.0.0.1:7201/debug/blocks > /tmp/before-1`
5. `dd if=/dev/zero of=/mnt/dtmpfs/stable bs=1M count=4 status=none`   # rewrite
6. `sync`
7. `curl -s http://127.0.0.1:7200/debug/blocks > /tmp/after-0`
8. `curl -s http://127.0.0.1:7201/debug/blocks > /tmp/after-1`
9. `diff /tmp/before-0 /tmp/after-0 | wc -l`
10. `diff /tmp/before-1 /tmp/after-1 | wc -l`

**Expected**:
- The set of `(ino, block_idx)` per store is the same before and
  after (only the `generation` field changes). Step 9 and 10 stdout
  may show generation diffs but no block-index churn.

**Pass criteria**:
- Same `(ino, block_idx)` mapping pre and post rewrite.

**Notes**:
- Verifies HRW is purely a function of key, independent of generation.


### A-073: Adding a third store mid-cluster
**Phase**: P3
**Category**: Sharding
**Preconditions**:
- 2 stores running; client has written 16 blocks.

**Steps**:
1. `dd if=/dev/urandom of=/mnt/dtmpfs/before bs=1M count=16 status=none`
2. `sync`
3. `curl -s http://127.0.0.1:7200/debug/blocks > /tmp/s0-before`
4. `curl -s http://127.0.0.1:7201/debug/blocks > /tmp/s1-before`
5. `# start store-2 on port 7202`
6. `RUST_LOG=info ./target/release/storesrv --config config/store2.toml &`
7. `sleep 2`     # wait for heartbeat & registration
8. `dd if=/dev/urandom of=/mnt/dtmpfs/after bs=1M count=16 status=none`
9. `sync`
10. `S2=$(curl -s http://127.0.0.1:7202/debug/blocks | wc -l)`
11. `diff /tmp/s0-before <(curl -s http://127.0.0.1:7200/debug/blocks) | head`

**Expected**:
- Step 10 stdout: a number > 0 (the new store has some `after` blocks).
- Step 11: blocks of `before` did not move; only `before` blocks may
  be present, plus possibly some `after` blocks.

**Pass criteria**:
- Old blocks did not migrate (no rebalancing in v1).
- New writes use the new store.

**Notes**:
- v1 explicitly does not rebalance. See
  [failure-model.md](failure-model.md) §"Rebalancing".


# Category: Replication (Phase 5+)

### A-090: With R=2 and 2 stores, every block has 2 placements
**Phase**: P5
**Category**: Replication
**Preconditions**:
- std-cluster but `replication_factor = 2` in `client.toml`.

**Steps**:
1. `dd if=/dev/urandom of=/mnt/dtmpfs/r2 bs=1M count=10 status=none`
2. `sync`
3. `S0=$(curl -s http://127.0.0.1:7200/debug/blocks | wc -l)`
4. `S1=$(curl -s http://127.0.0.1:7201/debug/blocks | wc -l)`
5. `echo "$S0 $S1"`

**Expected**:
- Step 5 stdout: `10 10` (every block on both stores).

**Pass criteria**:
- Each store holds 10 blocks.


### A-091: With R=2 and 3 stores, every block has 2 placements; primaries balanced
**Phase**: P5
**Category**: Replication / Sharding
**Preconditions**:
- 1 meta + 3 stores (ports 7200..7202) + 1 client; R=2.

**Steps**:
1. `dd if=/dev/urandom of=/mnt/dtmpfs/r2-3 bs=1M count=30 status=none`
2. `sync`
3. `for p in 7200 7201 7202; do
     ALL=$(curl -s http://127.0.0.1:$p/debug/blocks | wc -l)
     PRI=$(curl -s "http://127.0.0.1:$p/debug/blocks?role=primary" | wc -l)
     echo "$p $ALL $PRI"
   done`
4. `python3 -c "
import sys
total = 0; pris = []
for line in sys.stdin:
    _, all_, pri = map(int, line.split())
    total += all_; pris.append(pri)
ok = total == 60 and abs(max(pris) - min(pris)) <= 4 and sum(pris) == 30
print('OK' if ok else f'FAIL pris={pris} total={total}')" < <(curl_-result)`
   *(In practice, write a small helper or use jq. The pseudocode above
   shows intent.)*

**Expected**:
- Total blocks = 60 (30 unique × 2 placements).
- Sum of primaries across stores = 30.
- Max-min primary count ≤ 4.

**Pass criteria**:
- Replication factor enforced; primaries distributed.


### A-092: Kill primary, read still succeeds via replica
**Phase**: P5
**Category**: Replication / Failure
**Preconditions**:
- 3 stores running; R=2; file `/mnt/dtmpfs/repl` of 16 MiB written and
  synced.
- Identify a block whose primary is `store-0` (via meta's
  `/debug/inode/<ino>` endpoint); offset is `block_idx * 1 MiB`.

**Steps**:
1. `md5sum /mnt/dtmpfs/repl > /tmp/repl-before.md5`
2. `kill <store-0 PID>`
3. `sleep 2`     # wait > heartbeat_timeout
4. `# clear client block cache by re-opening — implicit on cat`
5. `md5sum /mnt/dtmpfs/repl > /tmp/repl-after.md5`
6. `diff /tmp/repl-before.md5 /tmp/repl-after.md5; echo exit=$?`

**Expected**:
- Step 6 final line: `exit=0`.
- All steps exit 0 (no `EIO`).

**Pass criteria**:
- File md5 unchanged after primary store death.


### A-093: Kill primary mid-flush
**Phase**: P5
**Category**: Replication / Failure
**Preconditions**:
- 3 stores running; R=2.

**Steps**:
1. `dd if=/dev/urandom of=/mnt/dtmpfs/midflush bs=1M count=64 conv=fsync status=none &`
2. `DD_PID=$!`
3. `sleep 0.05`     # let flush start
4. `kill <store-0 PID>`
5. `wait $DD_PID; echo dd_exit=$?`
6. `md5sum /mnt/dtmpfs/midflush 2>&1; echo md5_exit=$?`

**Expected**:
- Either:
  - (a) `dd` succeeds (`dd_exit=0`), `md5` succeeds (`md5_exit=0`), and
    md5 matches an md5 of the source. **Or**
  - (b) `dd` fails with `EIO` (`dd_exit=1`, stderr contains `Input/output
    error`), and the file is missing or partial. v1 does **not** retry
    in-flight writes — see [failure-model.md](failure-model.md)
    §"Write retries".

**Pass criteria**:
- One of (a) or (b). Anything else (silent corruption, panic) is a
  bug.

**Notes**:
- Phase 6 work moves this from (b) to (a) for store-only failures.


### A-094: Replicas are byte-identical to primary
**Phase**: P5
**Category**: Replication
**Preconditions**:
- 3 stores; R=2; `/mnt/dtmpfs/r` is 8 MiB written and synced.

**Steps**:
1. `INO=$(stat -c '%i' /mnt/dtmpfs/r)`
2. `# fetch block 0 from each store that holds it`
3. `for p in 7200 7201 7202; do
     curl -s -o /tmp/blk-$p \
       "http://127.0.0.1:$p/debug/block?ino=$INO&idx=0&gen=auto"
   done`
4. `# only the two stores that hold the block produce non-empty output`
5. `md5sum /tmp/blk-* | awk '{print $1}' | sort -u | wc -l`

**Expected**:
- Step 5 stdout: `1` (one unique md5 across the non-empty files; the
  third store's file is empty and excluded).

**Pass criteria**:
- All replicas byte-identical.


# Category: Failure injection (Phase 6+)

### A-110: Kill a store with R=1, reads of its blocks → EIO
**Phase**: P6
**Category**: Failure
**Preconditions**:
- std-cluster (R=1).

**Steps**:
1. `dd if=/dev/urandom of=/mnt/dtmpfs/r1 bs=1M count=8 status=none`
2. `sync`
3. `# pick a block whose primary is store-1`
4. `kill <store-1 PID>`
5. `sleep 2`
6. `cat /mnt/dtmpfs/r1 > /dev/null 2>&1; echo exit=$?`

**Expected**:
- Step 6 stdout: `exit=1` (cat fails with EIO from libc 5).
- Stderr contains `Input/output error`.

**Pass criteria**:
- Read fails cleanly with EIO; no panic; client log notes the dead
  store.


### A-111: Restart that store, reads still EIO (orphan blocks)
**Phase**: P6
**Category**: Failure
**Preconditions**:
- A-110 just ran; store-1 is dead; client log shows EIO.

**Steps**:
1. `RUST_LOG=info ./target/release/storesrv --config config/store1.toml &`
2. `sleep 2`     # heartbeat re-registration
3. `cat /mnt/dtmpfs/r1 > /dev/null 2>&1; echo exit=$?`

**Expected**:
- Step 3 stdout: `exit=1`. Stderr `Input/output error`.

**Pass criteria**:
- Restarted store has no blocks (RAM-only), and meta still points
  there. Read fails. **This is a documented v1 limitation.**

**Notes**:
- See [failure-model.md](failure-model.md) §"Store restart". A future
  Phase 6+ option is to mark the store's blocks as lost on
  re-registration so reads fail-faster with a clearer message.


### A-112: Kill meta, every op → EIO
**Phase**: P6
**Category**: Failure
**Preconditions**:
- std-cluster.

**Steps**:
1. `kill <meta PID>`
2. `sleep 2`
3. `ls /mnt/dtmpfs 2>&1; echo exit=$?`
4. `cat /mnt/dtmpfs/whatever 2>&1; echo exit=$?`
5. `touch /mnt/dtmpfs/x 2>&1; echo exit=$?`

**Expected**:
- Steps 3, 4, 5 each fail. Final line of each contains `exit=` non-zero
  with stderr `Input/output error` or `Transport endpoint is not
  connected`.

**Pass criteria**:
- All filesystem ops fail with `EIO`. No panic; mount remains live.


### A-113: Restart meta (empty state), mount appears empty
**Phase**: P6
**Category**: Failure
**Preconditions**:
- A-112 just ran.

**Steps**:
1. `RUST_LOG=info ./target/release/metasrv --config config/meta.toml &`
2. `sleep 2`
3. `ls /mnt/dtmpfs`

**Expected**:
- Step 3 stdout: empty. Exit 0.

**Pass criteria**:
- After meta restart with empty state, the mount is functional but
  empty. **Documented v1 limitation: meta has no persistence.**

**Notes**:
- See [failure-model.md](failure-model.md) §"Meta restart". v2 may
  add Raft + on-disk journal for meta; v1 is single-SPOF, in-memory.


### A-114: Kill client mount mid-write, dirty data is lost
**Phase**: P6
**Category**: Failure
**Preconditions**:
- std-cluster.

**Steps**:
1. `dd if=/dev/urandom of=/mnt/dtmpfs/dirty bs=1M count=64 status=none &`
2. `DD_PID=$!`
3. `sleep 0.1`     # let some writes happen
4. `kill -9 <client PID>`
5. `wait $DD_PID; echo dd_exit=$?`
6. `# AutoUnmount triggers; remount`
7. `RUST_LOG=info ./target/release/dtmpfs-mount --config config/client.toml &`
8. `sleep 1`
9. `ls -l /mnt/dtmpfs/dirty 2>&1; echo exit=$?`

**Expected**:
- Step 5: `dd_exit=` non-zero (write was interrupted).
- Step 9: file is either missing (`exit=2`, ENOENT) or present with
  size 0 (`stat` shows `Size: 0`). It is **not** present with partial
  data — partial blocks held only in client memory are lost on kill.

**Pass criteria**:
- No corruption (no partial-block bytes visible from another mount).


### A-115: Network partition simulated via iptables
**Phase**: P6
**Category**: Failure
**Preconditions**:
- 1 meta + 3 stores + 2 clients on the same host. Root or `sudo`
  available for iptables.

**Steps**:
1. `# write a file with R=1 whose blocks include some on store-2`
2. `dd if=/dev/urandom of=/mnt/dtmpfs/p bs=1M count=16 status=none && sync`
3. `# block traffic to store-2 from client A only`
4. `sudo iptables -A OUTPUT -p tcp --dport 7202 -j DROP`
5. `sleep 0.5`
6. `cat /mnt/dtmpfs/p > /dev/null 2>&1; echo exit=$?`
7. `cat /mnt/dtmpfs-b/p > /dev/null 2>&1; echo exit=$?`
8. `sudo iptables -D OUTPUT -p tcp --dport 7202 -j DROP`

**Expected**:
- Step 6: `exit=1` (EIO on partitioned client).
- Step 7: `exit=0` (other client unaffected).

**Pass criteria**:
- Partitioned-off client returns EIO for blocks on the unreachable
  store; other clients work.

**Notes**:
- iptables rule must be removed by step 8 even on test failure
  (`trap` in shell version).


# Category: Concurrency

### A-130: 100 parallel cat of the same file from one client
**Phase**: P1
**Category**: Concurrency
**Preconditions**:
- std-cluster; `/mnt/dtmpfs/par` of 16 MiB written.

**Steps**:
1. `dd if=/dev/urandom of=/mnt/dtmpfs/par bs=1M count=16 status=none && sync`
2. `EXP=$(md5sum /mnt/dtmpfs/par | awk '{print $1}')`
3. `for i in $(seq 1 100); do
     md5sum /mnt/dtmpfs/par | awk '{print $1}' &
   done | sort -u`

**Expected**:
- Step 3 stdout: a single line, the expected md5 (all 100 readers
  agreed). Same as `$EXP`.

**Pass criteria**:
- Exactly one unique md5 across 100 concurrent reads.


### A-131: 100 parallel cat from two clients
**Phase**: P4
**Category**: Concurrency / Cross-host
**Preconditions**:
- std-cluster + second client at `/mnt/dtmpfs-b`. File `/mnt/dtmpfs/par2`
  written and synced.

**Steps**:
1. `for i in $(seq 1 50); do md5sum /mnt/dtmpfs/par2  | awk '{print $1}' & done > /tmp/a &`
2. `for i in $(seq 1 50); do md5sum /mnt/dtmpfs-b/par2 | awk '{print $1}' & done > /tmp/b &`
3. `wait`
4. `cat /tmp/a /tmp/b | sort -u | wc -l`

**Expected**:
- Step 4 stdout: `1`.

**Pass criteria**:
- One unique md5 across 100 concurrent reads from two clients.


### A-132: 10 parallel writers to disjoint files
**Phase**: P1
**Category**: Concurrency
**Preconditions**:
- std-cluster.

**Steps**:
1. `for i in $(seq 0 9); do
     (dd if=/dev/urandom of=/mnt/dtmpfs/w-$i bs=1M count=8 status=none) &
   done; wait`
2. `sync`
3. `for i in $(seq 0 9); do stat -c '%s' /mnt/dtmpfs/w-$i; done | sort -u`

**Expected**:
- Step 3 stdout: `8388608` (single line; all 10 files have exactly
  8 MiB).

**Pass criteria**:
- All 10 writes succeeded; sizes correct; no panic in any process.


### A-133: 10 parallel writers to same file (overlapping)
**Phase**: P4
**Category**: Concurrency / Consistency
**Preconditions**:
- std-cluster (one client). Each writer writes the **same byte
  pattern** with their pid as the trailing tag.

**Steps**:
1. `for i in $(seq 0 9); do
     (python3 -c "
import os, sys
fd = os.open('/mnt/dtmpfs/over', os.O_WRONLY|os.O_CREAT, 0o644)
os.write(fd, ('w' + str($i)).encode() * (1024*1024 // 4))
os.close(fd)
") &
   done; wait`
2. `sync`
3. `head -c 4 /mnt/dtmpfs/over`

**Expected**:
- Step 3 stdout matches `^w[0-9]$` — exactly one of `w0..w9`. The
  file as a whole is the content of **one** writer (last close wins
  per file under close-to-open semantics).

**Pass criteria**:
- Final content corresponds to exactly one writer's content (no
  interleaving, no torn writes mid-block).

**Notes**:
- This is the documented close-to-open last-close-wins behaviour. See
  [consistency.md](consistency.md) §"Concurrent writes to the same
  file".


### A-134: Reader during writer's flush
**Phase**: P4
**Category**: Concurrency / Consistency
**Preconditions**:
- std-cluster + second client `/mnt/dtmpfs-b`. `/mnt/dtmpfs/r` initially
  contains content `OLD\n`, both clients have stat-ed it.

**Steps**:
1. `(echo NEW > /mnt/dtmpfs/r) &`
2. `WRITER=$!`
3. `# concurrent reader from B, no `sync` between
4. `cat /mnt/dtmpfs-b/r`
5. `wait $WRITER`
6. `sync`
7. `cat /mnt/dtmpfs-b/r`

**Expected**:
- Step 4 stdout: `OLD` (B saw pre-flush content because writer hadn't
  closed yet; B's AttrCache may be hot).
- Step 7 stdout: `NEW`.

**Pass criteria**:
- During a concurrent flush, reader sees stale-but-consistent content
  (not torn). After the writer closes and a `sync` barrier, reader
  sees the new content.

**Notes**:
- "Stale but consistent" means: if B sees generation N, every block
  it reads is at generation N. The block-cache key includes
  generation.


# Category: Edge cases / POSIX

### A-150: Zero-byte file write/read
**Phase**: P1
**Category**: POSIX
**Preconditions**:
- std-cluster.

**Steps**:
1. `> /mnt/dtmpfs/zero`
2. `stat -c '%s' /mnt/dtmpfs/zero`
3. `wc -c < /mnt/dtmpfs/zero`
4. `md5sum /mnt/dtmpfs/zero | awk '{print $1}'`

**Expected**:
- Step 2: `0`.
- Step 3: `0`.
- Step 4: `d41d8cd98f00b204e9800998ecf8427e` (md5 of empty input).

**Pass criteria**:
- Zero-byte file is well-defined and readable.


### A-151: 1-byte file
**Phase**: P1
**Category**: POSIX
**Preconditions**:
- std-cluster.

**Steps**:
1. `printf 'A' > /mnt/dtmpfs/one`
2. `stat -c '%s' /mnt/dtmpfs/one`
3. `xxd /mnt/dtmpfs/one`

**Expected**:
- Step 2 stdout: `1`.
- Step 3 stdout: `00000000: 41                                       A`.


### A-152: Exactly 1 MiB file (1 block, fully used)
**Phase**: P1
**Category**: POSIX
**Preconditions**:
- std-cluster.

**Steps**:
1. `dd if=/dev/urandom of=/tmp/exact1 bs=1M count=1 status=none`
2. `cp /tmp/exact1 /mnt/dtmpfs/exact1`
3. `stat -c '%s' /mnt/dtmpfs/exact1`
4. `cmp /tmp/exact1 /mnt/dtmpfs/exact1; echo exit=$?`

**Expected**:
- Step 3 stdout: `1048576`.
- Step 4 stdout: `exit=0`.

**Pass criteria**:
- 1 block exactly; bytes match.


### A-153: Exactly 1 MiB + 1 byte (2 blocks, second is 1 byte)
**Phase**: P1
**Category**: POSIX
**Preconditions**:
- std-cluster.

**Steps**:
1. `(dd if=/dev/urandom bs=1M count=1 status=none; printf X) > /tmp/exact1p1`
2. `cp /tmp/exact1p1 /mnt/dtmpfs/exact1p1`
3. `stat -c '%s' /mnt/dtmpfs/exact1p1`
4. `cmp /tmp/exact1p1 /mnt/dtmpfs/exact1p1; echo exit=$?`
5. `# verify a 2-block layout via /debug/inode`
6. `INO=$(stat -c '%i' /mnt/dtmpfs/exact1p1)`
7. `curl -s "http://127.0.0.1:7100/debug/inode?ino=$INO" | python3 -c "
import sys, json
d = json.load(sys.stdin)
print('OK' if len(d['blocks']) == 2 and d['size'] == 1048577 else f'FAIL {d}')"`

**Expected**:
- Step 4 stdout: `exit=0`.
- Step 7 stdout: `OK`.

**Pass criteria**:
- 2 blocks; second block holds 1 byte.


### A-154: File of size 0 after `truncate -s 0` of an existing file
**Phase**: P1
**Category**: POSIX
**Preconditions**:
- std-cluster; `/mnt/dtmpfs/t` is a 4 MiB file.

**Steps**:
1. `dd if=/dev/urandom of=/mnt/dtmpfs/t bs=1M count=4 status=none`
2. `sync`
3. `# blocks for ino exist on stores`
4. `INO=$(stat -c '%i' /mnt/dtmpfs/t)`
5. `truncate -s 0 /mnt/dtmpfs/t`
6. `sync`
7. `# blocks should be freed`
8. `S0=$(curl -s "http://127.0.0.1:7200/debug/blocks?ino=$INO" | wc -l)`
9. `S1=$(curl -s "http://127.0.0.1:7201/debug/blocks?ino=$INO" | wc -l)`
10. `echo "$S0 $S1"`

**Expected**:
- Step 10 stdout: `0 0`.

**Pass criteria**:
- After truncate-to-0, no blocks for that inode remain on any store.


### A-155: Filename with spaces and unicode
**Phase**: P1
**Category**: POSIX
**Preconditions**:
- std-cluster.

**Steps**:
1. `echo data > "/mnt/dtmpfs/with space and unicode okkk.txt"`
2. `cat "/mnt/dtmpfs/with space and unicode okkk.txt"`
3. `ls /mnt/dtmpfs/`

**Expected**:
- Step 2 stdout: `data`.
- Step 3 stdout includes (one line): `with space and unicode
  okkk.txt`.

**Pass criteria**:
- Names with spaces, multibyte UTF-8 round-trip.

**Notes**:
- Names are stored as raw bytes; no NFC/NFD normalization. POSIX
  permits this.


### A-156: Very long filename (255 bytes)
**Phase**: P1
**Category**: POSIX
**Preconditions**:
- std-cluster.

**Steps**:
1. `NAME=$(printf 'a%.0s' {1..255})`
2. `echo data > "/mnt/dtmpfs/$NAME"`
3. `ls "/mnt/dtmpfs/$NAME" | wc -c`     # 255 + newline + path prefix

**Expected**:
- Step 2 exit 0.
- Step 3 stdout: a number ≥ 256 (length of full path printed by ls).

**Pass criteria**:
- 255-byte filename works (POSIX `NAME_MAX`).


### A-157: Deep dir tree (depth 30)
**Phase**: P1
**Category**: POSIX
**Preconditions**:
- std-cluster.

**Steps**:
1. `P=/mnt/dtmpfs; for i in $(seq 1 30); do P=$P/d$i; mkdir "$P"; done`
2. `echo deep > "$P/leaf"`
3. `cat "$P/leaf"`
4. `find /mnt/dtmpfs -type d | wc -l`

**Expected**:
- Step 3 stdout: `deep`.
- Step 4 stdout: `31` (root + 30 nested).

**Pass criteria**:
- Depth-30 tree usable.


### A-158: Many files in one directory (10,000)
**Phase**: P1
**Category**: POSIX / Performance
**Preconditions**:
- std-cluster.

**Steps**:
1. `mkdir /mnt/dtmpfs/many`
2. `cd /mnt/dtmpfs/many && for i in $(seq 1 10000); do > $i; done`
3. `ls /mnt/dtmpfs/many | wc -l`
4. `find /mnt/dtmpfs/many -type f | wc -l`

**Expected**:
- Step 3 stdout: `10000`.
- Step 4 stdout: `10000`.

**Pass criteria**:
- All 10k entries listed by `ls` and `find`.

**Notes**:
- Run-time should be under 60 s on the prototype VM. If it's slower
  by an order of magnitude, file a perf bug.


### A-159: link returns EPERM
**Phase**: P1
**Category**: POSIX (non-conformance)
**Preconditions**:
- std-cluster.

**Steps**:
1. `touch /mnt/dtmpfs/orig`
2. `ln /mnt/dtmpfs/orig /mnt/dtmpfs/hard 2>&1; echo exit=$?`

**Expected**:
- Step 2 stderr: `ln: failed to create hard link
  '/mnt/dtmpfs/hard' => '/mnt/dtmpfs/orig': Operation not permitted`.
- Final line `exit=1`.

**Pass criteria**:
- ln fails with EPERM (1).

**Notes**:
- Documented non-support. See [consistency.md](consistency.md)
  §"Hardlinks".


### A-160: getxattr returns ENOSYS
**Phase**: P1
**Category**: POSIX (non-conformance)
**Preconditions**:
- std-cluster; `attr` package installed (`getfattr` available).

**Steps**:
1. `touch /mnt/dtmpfs/x`
2. `getfattr -n user.test /mnt/dtmpfs/x 2>&1; echo exit=$?`

**Expected**:
- Step 2 stderr contains `Function not implemented` or
  `Operation not supported`.
- Final line `exit=1`.

**Pass criteria**:
- xattr ops error out cleanly with ENOSYS (38) or ENOTSUP (95).

**Notes**:
- v1 has no xattrs. See [consistency.md](consistency.md) §"xattrs".


### A-161: mmap MAP_SHARED write+msync
**Phase**: P1
**Category**: POSIX (caveat)
**Preconditions**:
- std-cluster.

**Steps**:
1. `dd if=/dev/zero of=/mnt/dtmpfs/m bs=1M count=1 status=none`
2. `sync`
3. `python3 -c "
import mmap, os
fd = os.open('/mnt/dtmpfs/m', os.O_RDWR)
m = mmap.mmap(fd, 1024*1024, mmap.MAP_SHARED, mmap.PROT_READ|mmap.PROT_WRITE)
m[0:5] = b'HELLO'
m.flush()
m.close()
os.close(fd)
"`
4. `head -c 5 /mnt/dtmpfs/m`

**Expected**:
- Step 4 stdout: `HELLO`.
- All steps exit 0.

**Pass criteria**:
- Small `mmap MAP_SHARED` write becomes visible after `msync`.

**Notes**:
- v1 does **not** implement direct `mmap` writeback in the FUSE
  handler; the kernel falls back to `read`/`write` for files small
  enough to fit kernel page cache. **Do not** rely on this for files
  larger than a few MiB or for cross-host visibility.
  See [consistency.md](consistency.md) §"mmap".


# Category: Performance smoke (not regression-gated in v1)

### A-180: Write throughput ≥ 200 MB/s (single client, single store, localhost)
**Phase**: P1
**Category**: Performance
**Preconditions**:
- std-cluster; warm — run once before measuring.

**Steps**:
1. `dd if=/dev/zero of=/mnt/dtmpfs/perf bs=1M count=1024 conv=fsync status=progress 2>&1 | tail -1`

**Expected**:
- Output line of the form `... 1073741824 bytes (1.1 GB, 1.0 GiB)
  copied, T s, X MB/s` with `X >= 200`.

**Pass criteria**:
- Effective throughput ≥ 200 MB/s.

**Notes**:
- This is loose. v1 just wants "not catastrophic". A regression-tracking
  pipeline lands in Phase 7+.


### A-181: Read throughput ≥ 300 MB/s (cached) / ≥ 200 MB/s (cold)
**Phase**: P1
**Category**: Performance
**Preconditions**:
- A-180 just ran; `/mnt/dtmpfs/perf` is 1 GiB.

**Steps**:
1. `dd if=/mnt/dtmpfs/perf of=/dev/null bs=1M status=progress 2>&1 | tail -1`     # cached
2. `# drop caches`
3. `sync && echo 3 | sudo tee /proc/sys/vm/drop_caches`
4. `dd if=/mnt/dtmpfs/perf of=/dev/null bs=1M status=progress 2>&1 | tail -1`     # cold

**Expected**:
- Step 1: `X >= 300`.
- Step 4: `X >= 200`.

**Pass criteria**:
- Both thresholds met.


### A-182: Sequential 1 MiB ops latency p50 < 1 ms localhost
**Phase**: P1
**Category**: Performance
**Preconditions**:
- std-cluster; `fio` installed.

**Steps**:
1. `fio --name=seq --rw=write --bs=1M --size=512M --filename=/mnt/dtmpfs/fio --direct=0 --iodepth=1 --runtime=10 --time_based --output-format=json > /tmp/fio.json`
2. `python3 -c "
import json
d = json.load(open('/tmp/fio.json'))
p50_ns = d['jobs'][0]['write']['clat_ns']['percentile']['50.000000']
print(p50_ns / 1e6, 'ms')"`

**Expected**:
- Stdout p50 < 1.0 ms.

**Pass criteria**:
- p50 latency under 1 ms.


### A-183: 64 KiB random read latency p50 < 0.5 ms localhost
**Phase**: P1
**Category**: Performance
**Preconditions**:
- std-cluster; `/mnt/dtmpfs/fio` is 512 MiB.

**Steps**:
1. `fio --name=rand --rw=randread --bs=64k --size=512M --filename=/mnt/dtmpfs/fio --direct=0 --iodepth=1 --runtime=10 --time_based --output-format=json > /tmp/fio.json`
2. `python3 -c "
import json
d = json.load(open('/tmp/fio.json'))
p50_ns = d['jobs'][0]['read']['clat_ns']['percentile']['50.000000']
print(p50_ns / 1e6, 'ms')"`

**Expected**:
- Stdout p50 < 0.5 ms.

**Pass criteria**:
- p50 random-read latency under 0.5 ms (single block hit).


### A-184: Capacity: fill a store to 80% of ram_budget without OOM
**Phase**: P1
**Category**: Performance / Capacity
**Preconditions**:
- single store with `ram_budget = 1 GiB`; std-cluster otherwise.

**Steps**:
1. `dd if=/dev/zero of=/mnt/dtmpfs/cap bs=1M count=800 status=none`
2. `sync`
3. `curl -s http://127.0.0.1:7200/debug/stat | python3 -m json.tool`

**Expected**:
- Step 3 JSON includes `"used_bytes": >= 838860800` and the store
  process is alive.

**Pass criteria**:
- Store accepts up to 80% of `ram_budget` without OOM-kill or panic.

**Notes**:
- Store reserves 20% headroom for fragmentation and metadata.


# Category: Configuration & operations

### A-200: Wrong cluster_token: clear error in client log; first op EIO
**Phase**: P1
**Category**: Configuration / Mount
**Preconditions**:
- (same as A-002)

**Steps**: see A-002.

**Expected**: see A-002.

**Pass criteria**: see A-002.

**Notes**:
- This entry exists for completeness in the configuration category.


### A-201: Wrong meta_addr: client fails fast at startup
**Phase**: P1
**Category**: Configuration / Mount
**Preconditions**:
- meta NOT running on `127.0.0.1:9999`. Client config
  `config/client-bad-meta.toml` has `meta_addr =
  "http://127.0.0.1:9999"`.

**Steps**:
1. `./target/release/dtmpfs-mount --config config/client-bad-meta.toml; echo exit=$?`

**Expected**:
- Stderr contains `cannot connect to meta: Connection refused`.
- Final line: `exit=` non-zero.
- `mountpoint -q /mnt/dtmpfs` returns non-zero.

**Pass criteria**:
- Client refuses to mount and exits with a clear error.

**Notes**:
- Unlike a bad token (A-002), a bad meta address is detected at
  startup because the client tries an initial `Meta.ListNodes` to
  populate its node table.


### A-202: Port collision: server logs bind error and exits
**Phase**: P1
**Category**: Configuration / Mount
**Preconditions**:
- A meta server is already running on port 7100. A second meta
  config points at the same port.

**Steps**:
1. `./target/release/metasrv --config config/meta.toml & FIRST=$!`
2. `sleep 0.5`
3. `./target/release/metasrv --config config/meta.toml; echo exit=$?`
4. `kill $FIRST`

**Expected**:
- Step 3 stderr contains `bind: Address already in use`.
- Step 3 final line: `exit=` non-zero (typically 1).

**Pass criteria**:
- Second instance exits cleanly with a clear error.


### A-203: Two stores with same node_id
**Phase**: P6
**Category**: Configuration
**Preconditions**:
- store-0 and store-1 configs both use `node_id = "store-0"`.

**Steps**:
1. `./target/release/storesrv --config config/store0.toml &`
2. `sleep 0.5`
3. `./target/release/storesrv --config config/store1.toml &`
4. `sleep 1`
5. `# inspect meta log for warning`

**Expected**:
- Meta log contains: `WARN: node_id "store-0" registered from
  multiple addresses; behaviour undefined in v1`.
- Both stores remain running, but the meta's node table has only one
  entry for `"store-0"` (the most recent address wins).

**Pass criteria**:
- Warning logged. **Behaviour undefined v1** — operators are expected
  to use unique `node_id`. See [operations.md](operations.md)
  §"Node IDs".


### A-204: ramp-up: stop store, restart it, heartbeat re-registers within 2s
**Phase**: P6
**Category**: Operations
**Preconditions**:
- std-cluster; `heartbeat_ms = 200`, `heartbeat_timeout_ms = 1000`.

**Steps**:
1. `kill <store-1 PID>`
2. `sleep 1.5`     # > timeout
3. `# meta marks store-1 Down; verify`
4. `curl -s http://127.0.0.1:7100/debug/nodes | python3 -m json.tool`
5. `RUST_LOG=info ./target/release/storesrv --config config/store1.toml &`
6. `sleep 2`
7. `curl -s http://127.0.0.1:7100/debug/nodes | python3 -m json.tool`

**Expected**:
- Step 4 JSON shows store-1 with `"status": "Down"`.
- Step 7 JSON shows store-1 with `"status": "Up"`.

**Pass criteria**:
- Within 2 s of restart, meta sees store-1 as Up.


### A-205: df -h /mnt/dtmpfs shows aggregate ram budget
**Phase**: P3
**Category**: Operations
**Preconditions**:
- std-cluster (2 stores, each `ram_budget = 1 GiB`).

**Steps**:
1. `df -h /mnt/dtmpfs`

**Expected**:
- Output line where `Size` column is approximately `2.0G` (sum of
  `ram_budget` across live stores) and `Filesystem` is `dtmpfs` or
  `fuse.dtmpfs`.

**Pass criteria**:
- `df` reports the cluster's aggregate capacity.

**Notes**:
- Implemented via FUSE `statfs`. v1 reports the sum of `ram_budget`
  of `Up` stores, divided by `replication_factor`. With R=1 and 2
  stores at 1 GiB, that's 2 GiB usable.


# Manual exploratory checklist

To be run during the Phase-5+ bug bash. Two operators (A and B), each
on their own mount. After every step, note any anomaly: error message,
hang, slowness, surprising content. Anomalies are captured as P1/P2
bugs with full repro.

1. `git clone` a small repo (~10k files) into the mount; `git status`;
   `git log`.
2. `git clone` the same repo into a second mount; do `git pull`
   on both; check for divergence.
3. `tar cf - .` from one mount, `tar xf -` into another mount.
   Verify with `find ... -type f | xargs md5sum | sort` matches.
4. `rsync -av --progress /tmp/big-tree/ /mnt/dtmpfs/big-tree/`.
   Run twice; second run should be a no-op.
5. `rsync -av /mnt/dtmpfs-a/big-tree/ /mnt/dtmpfs-b/big-tree/`.
6. `vim /mnt/dtmpfs/notes.md` — open, edit, `:wq`. Reopen on the other
   mount; expect the latest content.
7. Open the same file in two `vim`s on two clients. Save on A; save
   on B. Document outcome (last-save-wins per close-to-open; B's
   content overwrites A's, A's swap file may be orphaned).
8. `python3 -m http.server` from inside the mount; download a file
   via `curl` from another host.
9. `make -j16` of a small C project inside the mount.
10. `npm install` inside the mount (heavy on small files and
    rename-on-replace patterns).
11. `cargo build` of a small Rust crate inside the mount; check for
    `target/` content correctness.
12. `unzip` a 100 MB zip; `zip -r` it back; diff.
13. SQLite: `sqlite3 /mnt/dtmpfs/test.db` and run a few transactions.
    Document WAL-mode behaviour (POSIX advisory lock interaction is
    documented as best-effort in v1; serialized journal mode should
    work).
14. `cp -a /tmp/big /mnt/dtmpfs/big`; `cp -a /mnt/dtmpfs/big /tmp/back`;
    diff.
15. Create a 32-deep directory tree; `find` it; `rm -rf` it.
16. Create 10,000 files in a directory; `ls -l > /dev/null` — note
    time taken; `rm` them.
17. `dd if=/dev/urandom of=/mnt/dtmpfs/big bs=1M count=2048` — fill
    much of the cluster's capacity. Observe behaviour at ENOSPC
    (should be ENOSPC, not panic).
18. While step 17 is running, kill one store. Document outcome
    (R=1 → EIO; R=2 → may proceed).
19. While step 17 is running, run a `cat` of an unrelated file.
    Should still work; latency may rise.
20. Try `chattr +i /mnt/dtmpfs/x` (immutable). Document outcome
    (likely ENOSYS).
21. Try `truncate -s 100G /mnt/dtmpfs/giant`. Should ENOSPC well
    before completion.
22. Mount and unmount in a tight loop 100 times.
23. `stress-ng --hdd 4 --hdd-bytes 100M --temp-path /mnt/dtmpfs`
    for 5 minutes. Document any panic, leak (`top` of store
    process), or slowdown.
24. Use `inotifywait -m /mnt/dtmpfs` from one client; trigger events
    from another. Document v1 behaviour (likely no notifications;
    inotify is local to the kernel that opened the watch).
25. `cp` a binary into the mount and `chmod +x` it; `./bin` should
    run.
26. `cp` a script with shebang `#!/usr/bin/env python3` into the
    mount; `chmod +x`; run. Document any FUSE-specific NOEXEC
    issues (we mount with default exec).
27. `ln -s target /mnt/dtmpfs/symlink`; verify `readlink` returns
    `target`; verify cross-mount.
28. `ln -s /absolute/path /mnt/dtmpfs/abs-symlink`; verify.
29. Run the system test suite of `pjdfstest` against the mount.
    Note known-failing classes (link, xattr, ACLs, devices) and
    capture the rest of the failures as bugs.
30. Run `bonnie++ -d /mnt/dtmpfs -s 1g -n 1024:1k:0:1` and capture
    the output for the perf log.


# Coverage map

This table maps each functional requirement (numbered as in
[HLD.md](HLD.md) §"Functional requirements") to the acceptance tests
that exercise it.

| #   | Functional requirement (HLD)                                                | Acceptance tests                                |
|-----|-----------------------------------------------------------------------------|-------------------------------------------------|
| F1  | Mountable on ≥2 hosts simultaneously                                        | A-001, A-005, A-050, A-055                      |
| F2  | Write on host A visible on host B post-close                                | A-050, A-051, A-055, A-131                      |
| F3  | Standard POSIX file ops (create, read, write, append, truncate, delete)     | A-010..A-018, A-024, A-150..A-154, A-159        |
| F4  | Standard POSIX directory ops (mkdir, rmdir, rename, ls, find)               | A-030..A-038, A-157, A-158                      |
| F5  | Files of arbitrary size, blocked at 1 MiB internally                        | A-013, A-014, A-152, A-153                      |
| F6  | Sharded across N stores via HRW                                             | A-070, A-071, A-072, A-073                      |
| F7  | Replicated at configurable R, with R=2 surviving 1 store death              | A-090, A-091, A-092, A-094                      |
| F8  | Inode generation bumps on close-with-dirty                                  | A-023, A-025                                    |
| F9  | AttrCache 1 s TTL on the client                                             | A-053                                           |
| F10 | Cluster auth via shared `cluster_token`                                     | A-002, A-200                                    |
| F11 | Configurable via TOML; role-tagged                                          | A-200..A-205                                    |
| F12 | `df` reports aggregate capacity                                             | A-205                                           |
| F13 | Heartbeats every `heartbeat_ms`; nodes timed out at `heartbeat_timeout_ms`  | A-204                                           |
| F14 | EIO on store death with R=1; transparent failover with R≥2                  | A-092, A-110                                    |
| F15 | EIO on meta death; recovery on restart with empty state                     | A-112, A-113                                    |
| F16 | No data corruption under concurrent writers (close-to-open last-close-wins) | A-130, A-132, A-133, A-134                      |
| F17 | Documented non-conformances: hardlinks, xattrs, special files, mmap         | A-159, A-160, A-161                             |
| F18 | Long filenames up to NAME_MAX (255 bytes)                                   | A-156                                           |
| F19 | Filenames with non-ASCII bytes (UTF-8)                                      | A-155                                           |
| F20 | Capacity: store accepts up to ~80% of `ram_budget`                          | A-184                                           |
| F21 | Performance: ≥ 200 MB/s write, ≥ 300 MB/s cached read on localhost         | A-180, A-181                                    |
| F22 | Latency: 1 MiB seq p50 < 1 ms; 64 KiB random p50 < 0.5 ms localhost        | A-182, A-183                                    |
| F23 | RMW path correctness for partial-block writes                               | A-020                                           |
| F24 | Block placement is independent of generation                                | A-072                                           |
| F25 | No automatic rebalance on store add (v1)                                    | A-073                                           |
| F26 | Network partition behaviour: minority side EIOs                             | A-115                                           |
| F27 | Mount point must be empty; AutoUnmount on Ctrl-C                            | A-003, A-004                                    |
| F28 | Append (`>>`) and overwrite (`>`) work as in any FS                         | A-015, A-016                                    |
| F29 | `truncate(0)` frees blocks                                                  | A-017, A-154                                    |
| F30 | `truncate(N>size)` materializes zeros (v1)                                  | A-018                                           |

When a new functional requirement is added to the HLD, this table
must grow a row. PRs that add an FR without an acceptance test (or a
plan for one in the next phase) are rejected.
