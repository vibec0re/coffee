# Issues

Open issues from the code review of `6fecac6`, ordered by severity. Each one
was confirmed by reading the code. Fixed issues keep their numbers and are
listed at the end.

## Correctness

### 4. A link whose source is temporarily missing is lost for good (high)

`load()` skips entries whose source can't be opened (`src/ctl.rs:211`). Any
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
any error aborts `serve()` (`src/main.rs:296-312`). Combined with
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

### 7. A corrupt state file blocks startup (low)

`save()` now fsyncs the file and its directory (fixed together with #1), so a
crash should no longer leave a truncated `NAME.json`. A corrupt file from any
other cause still makes `load()` fail, `serve()` exit 1, and systemd
restart-loop the instance until someone deletes the file.

**Fix:** on a parse error, move the bad file aside as `NAME.json.corrupt` and
start empty (logged loudly) instead of refusing to start.

### 8. `fsyncdir` is a no-op (medium)

`fsyncdir` replies OK without doing anything (`src/fs.rs:348`), and
`Core::fsync` does the same for `Handle::Dir` (`src/core.rs:849`). An
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
`remove()` has a third (`src/core.rs:1082-1290`). The "is a link" error text
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

## Fixed

### 1. State file was world-writable (critical)

The daemon ran with a process-wide `umask(0)`, so `save()` created `NAME.json`
as 0666. Any local user could add a link, and the root daemon would replay it
on the next start.

**Fixed:**
- The process umask is now 022. Only FUSE worker threads switch to a private
  umask of 0, via `unshare(CLONE_FS)`, while they create on behalf of a
  request.
- `save()` removes any leftover `NAME.tmp`, then writes a fresh 0600 file with
  `O_EXCL` (so it never follows a planted symlink), fsyncs it, renames it and
  fsyncs the directory.
- `load()` refuses a state file that isn't owned by the daemon's user or that
  group/others can write, instead of replaying it.

Tests:
- Unit: `state_file_is_private_and_written_fresh`,
  `state_file_writable_by_others_is_refused`.
- VM: "daemon's own files are not world-writable".

### 2. Mountpoints and their parents were created 0777 (high)

This had the same umask root cause as #1.

**Fixed:**
- `mount::prepare_mountpoint` creates missing dirs as 0755 and refuses a
  mountpoint that is a symlink.
- Stale-mount detection uses `lstat`, and lazy unmount passes
  `UMOUNT_NOFOLLOW`.

**Remaining risk:** symlinks in ancestor directories are still followed. They
can only be planted if an admin pre-created a writable ancestor. Mounting
through the new mount API (`fsmount` + `move_mount` onto an
`openat2(RESOLVE_NO_SYMLINKS)` fd) would close that too.

Tests:
- Unit: `mountpoints_are_created_0755_and_symlinks_refused`.
- VM: `/containers` is 755.

### 3. Ownership of new entries could be stolen through a race (high)

The daemon created entries as root, then reopened them **by name** and chowned
whatever it found.

**Fixed:** `adopt()` is gone. `sys::AsCaller` switches the worker thread's
fsuid/fsgid to the caller for the creating syscall only: mkdir, mknod,
symlink, and `O_CREAT` open. It then raises `CAP_DAC_OVERRIDE` in that
thread, because the kernel already checked access under `default_permissions`
and the thread lacks the caller's supplementary groups. The host kernel now
sets owner, group (including setgid inheritance) and mode, so nothing is
chowned. The name is still re-resolved to register the new inode, but that is
read-only, the same as a lookup.

This also fixes a side effect of the old approach: `mkdir` with S_ISGID in
the mode used to set the bit by chmod. Linux ignores it, and so do we now.

Tests:
- Unit: `created_objects_get_the_requested_mode_whatever_the_process_umask`.
- VM: "objects are created as the caller, not chowned afterwards". It covers
  the caller's umask, fifos and symlinks, a supplementary-group-only dir, and
  setgid inheritance.
