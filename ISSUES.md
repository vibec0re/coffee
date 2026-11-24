# Issues

Open issues from the code review of `6fecac6`, ordered by severity. Each one
is confirmed by reading the code. None has been reproduced in the VM test
yet, and none is covered by a test.

## Security

### 1. State file is world-writable (critical)

`serve()` sets a process-wide `umask(0)` (`src/main.rs:280`) so that it can
apply each request's umask itself. `Controller::save()` (`src/ctl.rs:148`)
then writes `NAME.tmp` with `std::fs::write`, which uses mode 0666, and renames
it over `NAME.json`. The result is `/var/lib/coffeefs/NAME.json` with mode 0666.

**Impact:** any local user can add
`{"kind":"link","target":"/host","source":"/","ro":false}`. The root daemon
replays it on the next start, which exposes the host's `/` read-write in the
container.

**Fix:** don't let the daemon's own files inherit umask 0. Either restore a
restrictive process umask (`0o077`) and `fchmod` every object created for a
request to its intended mode, or create every daemon-owned file with an
explicit mode (`OpenOptions::mode(0o600)`). Add a test that checks the state
file's mode.

### 2. Mountpoints and their parents are created 0777 (high)

`mount_fuse` (`src/mount.rs:89`) and the unprivileged path (`src/main.rs:347`)
call `create_dir_all` under the same `umask(0)`. On first start this creates
`/containers` and `/containers/NAME` world-writable.

**Impact:** a local user can replace `/containers/<other-instance>` with a
symlink to `/etc` before that instance starts. `clear_stale`, `create_dir_all`
and `mount(2)` all follow the symlink, so the FUSE fs ends up mounted over
`/etc`. A local user can also take over mountpoints of instances that haven't
started yet.

**Fix:** use `DirBuilder::new().recursive(true).mode(0o755)`. Same root cause
and fix as #1. Also refuse a mountpoint that is a symlink: open it with
`O_PATH|O_NOFOLLOW`, then mount via `/proc/self/fd/N`, or use the new mount API
(`move_mount`).

### 3. Ownership of new entries can be stolen through a race (high)

`finish_new` (`src/core.rs:660`) first creates the entry with
`mkdirat`/`mknodat`/`symlinkat`, then opens it again **by name**, and `adopt()`
`fchown`s whatever that open returned to the requester.

**Impact:** suppose a container user can write to a linked directory that
also holds a root-owned file. They loop `mkdir x` against
`rmdir x; mv root_file x`. When the race lands, the root daemon chowns
`root_file` to them.

`adopt()` has a related problem: when the chown fails, it cleans up with
`unlinkat` by name (`src/core.rs:655`). In a sticky directory that could delete
a file the requester isn't allowed to remove. `create()` is not affected,
because it gets its fd straight from `O_CREAT|O_EXCL`.

**Fix:** create entries as the caller, the way virtiofsd does: set
`setresuid`/`setresgid` for the current thread with raw syscalls around the
`*at()` call. The kernel then sets ownership, setgid inheritance and
sticky-dir rules itself, and `adopt()` can be removed. A smaller stopgap is to
`fstat` the reopened fd and refuse anything that isn't the expected type,
owned by the daemon's euid, with `nlink == 1` (2 for a directory).

## Correctness

### 4. A link whose source is temporarily missing is lost for good (high)

`load()` skips entries whose source can't be opened (`src/ctl.rs:178`). Any
later `save()` writes the state without them. That later save is either the
startup `--mkdir`/`--link` replay, which `handle()` saves after, or the next
runtime `add`/`rm`.

**Impact:** if the daemon starts before `/home`, ZFS or a network mount is
available, every runtime-added link on that filesystem is silently dropped.

**Fix:** keep entries that fail to load in a "pending" list. Write them back
on save, and show them in `ls` marked as unavailable. Optionally retry them
later, or when asked to over the control socket.

### 5. A conflict between the config and the state file causes a restart loop (medium)

The state file is loaded first. Then `--mkdir`/`--link` are applied, and
any error aborts `serve()` (`src/main.rs:295-310`). Combined with
`Restart=on-failure`, the instance never comes up.

**Examples:**
- `dirs = ["/x"]` in the config, plus a later `coffeefs add inst /srv /x --replace`
  that was persisted. On restart, `--mkdir /x` fails with "/x is a link".
- `--link src:/a` while the state has links under `/a` fails with "/a is a
  non-empty directory".

**Fix:** apply config entries first and the state file second. If the state
file conflicts with config entries, the config wins and the conflicting state
entry is logged and kept as pending (see #4). Never fail startup over a
conflict.

### 6. Removing a link from the NixOS config doesn't revoke it (medium)

With `persist = true` (the default), links from the config are saved into the
state file like runtime links, then replayed on every start
(`nix/module.nix:119-122`).

**Impact:** an admin deletes a link from `services.coffeefs.instances.X.links`
and rebuilds, but the container keeps access.

**Fix:** record where each entry came from (`config` or `runtime`), and
persist only `runtime` entries. Config entries come from argv on every start.
`ls` can show which is which.

### 7. A crash during `save()` can leave an empty state file and block startup (medium)

`save()` (`src/ctl.rs:148`) does write, then rename, with no `fsync` of the
file or the directory. After a power loss, `NAME.json` can be empty or
truncated. `load()` then fails to parse it, `serve()` exits 1, and systemd
restart-loops the instance until someone deletes the file.

**Fix:** `fsync` the tmp file before the rename and the directory after it.
Also, on a parse error, move the bad file aside as `NAME.json.corrupt` and
start empty (logged loudly) instead of refusing to start.

### 8. `fsyncdir` is a no-op (medium)

`fsyncdir` replies OK without doing anything (`src/fs.rs:348`), and
`Core::fsync` does the same for `Handle::Dir` (`src/core.rs:877`). An
application that does write, fsync, rename, then fsync on the directory is
told the rename is durable, but it isn't.

**Fix:** keep a real directory fd in `DirState` (or reopen the O_PATH fd with
`O_RDONLY|O_DIRECTORY`) and `fsync` it. Virtual dirs can stay a no-op.

### 9. `--link SRC:DST` splits at the first `:` (low)

`split_link` (`src/main.rs:128`) splits on the first `:`, so
`/srv/a:b:/x` becomes source `/srv/a` and target `b:/x`. The NixOS module
builds this argument from paths it doesn't validate (`nix/module.nix:124`).

**Fix:** have the module pass source and target as two separate arguments
(`--link SRC DST`, `num_args = 2`), or split at the last `:` and document that
targets can't contain `:`. Add an assertion in the module either way.

## Cleanup

### 10. The virtual-tree walk is implemented three times (low)

`add_link` has its own validation walk, `ensure_dirs` has a second one, and
`remove()` has a third (`src/core.rs:1110-1300`). The "is a link" error text
is already duplicated. `RealRef` is built by hand in both `get()` and
`lookup()` (around `src/core.rs:424` and `:492`). After `file()`, five places
repeat `let Handle::File(f) = &*h else { unreachable!() }`.

**Fix:** a shared `walk(&State, comps)` helper, `From<&Real> for RealRef`,
and a `file()` that returns the `File` directly.

## Limitations / future work

These are the caveats listed in the README, plus a few design limits.

- **No POSIX ACL enforcement.** The kernel checks mode bits only. Possible fix:
  combined with #3's per-thread credentials, drop `default_permissions` and
  let the host kernel check permissions as the caller.
- **Restarting a host-mode instance kills the container's bind mount.** The
  container sees `ENOTCONN` until it restarts. `inContainerNamespace` avoids
  this. Possible fix: hand the `/dev/fuse` fd to systemd's fd store
  (`FDSTORE=1`) and pick the session back up on restart.
- **All I/O goes through the daemon.** Kernel FUSE passthrough (Linux 6.9+,
  `FUSE_PASSTHROUGH` with backing fds) would let reads and writes skip the
  daemon for files opened rw or ro.
- **One O_PATH fd per inode the kernel holds.** Very large trees can hit
  `RLIMIT_NOFILE`. Set `LimitNOFILE` in the module, or switch to file handles
  (`name_to_handle_at`/`open_by_handle_at`) like virtiofsd.
- **No uid mapping.** Owners are passed through 1:1. That's fine for nspawn
  without user namespaces, but wrong for containers that use
  `PrivateUsers=`/userns.
- **Locks are local.** POSIX and flock locks don't coordinate with host
  processes.
- **Unix sockets in linked dirs can't be connected to.**
- **Inode numbers differ from the host's.** A file reached through two links
  shows up as two inodes.
