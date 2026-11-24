# Issues

This started as the code review of `6fecac6`, plus a Code Climate-style
check. #1–#14 are fixed (see the end), and each fix has unit tests and, where
the kernel or systemd is involved, a subtest in the NixOS VM test. The
remaining items are open.

## Open

### 15. Symlinks in a mountpoint's parent directories are followed (low)

What's fixed (#2): missing parents are created 0755, and a mountpoint that is
itself a symlink is refused.

What's left: a symlink in a pre-existing ancestor is still followed. It can
only be planted if an admin created that ancestor writable for others.

**Fix:** mount through the new mount API: `fsopen("fuse")` and `fsmount`,
then `move_mount` onto an `openat2(RESOLVE_NO_SYMLINKS)` fd. In a foreign
namespace this also avoids `/proc/self` (which isn't usable after `setns`).

### 17. `tests.rs` is long (low)

The file is 1055 lines. Ten tests are over 25 lines, and the setup code
(controller with a state file, `fs::write` + `chmod 600`) repeats.

**Fix:** a small fixture, `Inst::new(config)`, holding the controller, the
state path and a host dir, then split the file by area (`core`, `ctl`, `perm`).

### 18. Upgrading a host-mode instance needs a container restart (medium)

Config changes now reload in place, and so do package upgrades, on purpose.
A restart would leave containers that bind-mount the instance with a dead
mount. So a new binary only takes effect after
`systemctl restart coffeefs-X container@X`, and a crash has the same effect
as that restart.

**Fix:**
- Keep the `/dev/fuse` fd in systemd's fd store (`FDSTORE=1`,
  `FileDescriptorStoreMax=`), and on restart pick the connection back up
  instead of mounting again.
- The hard part is that the kernel still references the old inode numbers.
  The inode table, with its `O_PATH` fds, has to survive too, either in the fd
  store or by mapping inode numbers to file handles (`name_to_handle_at`).
  Until then, `inContainerNamespace` instances avoid the problem entirely.

### 19. No syscall filter in the unit (low)

The unit has a capability bounding set, `NoNewPrivileges`, `AF_UNIX` only and
`RestrictNamespaces=mnt`. It has no `SystemCallFilter=`.

**Fix:** `@system-service @mount` plus `setns`, `capset`, `setfsuid` and
`setfsgid`, then verify in the VM test. A mistake shows up as `SIGSYS`, so
the test has to exercise every path: namespace mode, passthrough, and
creation as the caller.

## Code climate

The same check as before:
- **Metrics:** `rust-code-analysis-cli`.
- **Lints:** clippy pedantic.
- **Thresholds:** Code Climate's defaults.

| | `6fecac6` | now |
|---|---|---|
| Max cognitive complexity | 18 (`setattr`) | 8 |
| Functions over complexity 5 | 15 | 13, all 6–8 |
| Longest function | 161 lines (`serve`) | 59 lines (`serve`, straight-line setup) |
| `add_link` | 95 lines, complexity 14 | 47 lines, complexity 8 |
| Largest non-trait file | `core.rs`, 1193 lines | `core/mod.rs`, 340 lines |
| Copies of the reply error arm in `fs.rs` | 25 | 0 |
| `unreachable!()` after typed lookups | 7 | 0 |
| Pedantic warnings (non-test) | 56 | about 30, all numeric casts at the FFI boundary or by-value args |

What changed:
- `core.rs` is now `core/`:
  - `mod.rs`: types, the inode table and lookup.
  - `tree.rs`: the skeleton and the control plane, with one `walk` plus
    `attach`/`unhook`.
  - `ns.rs`: namespace ops, with one `make` helper and a `Caller` for
    owner + umask.
  - `io.rs`: typed handles, so `file()` returns `Arc<File>`, plus the split
    listing code.
  - `attr.rs`: `setattr` split into helpers.
  - `xattr.rs`: separate read and write paths.
- The other splits:
  - `ctl.rs` became `ctl/` (controller, and `socket`).
  - `sys.rs` became `sys/` (wrappers, and `creds`).
  - `main.rs` became `main.rs` (CLI) plus `serve.rs`.

What's still above the thresholds, and why:
- `fs.rs` (442 code lines, 34 callbacks) is shaped by fuser's `Filesystem`
  trait.
- `Core::create` has 5 arguments: FUSE create carries both mode and flags.
- The `sys` wrappers mirror their syscalls' arguments.

## Limitations / future work

These are the caveats listed in the README, plus a few design limits.

- **No POSIX ACL enforcement.** The kernel checks mode bits only. Now that
  objects are created as the caller (#3), the next step could be: drop
  `default_permissions`, switch the full credentials (including supplementary
  groups) per request, and let the host kernel check permissions and ACLs as
  the caller.
- **A host-mode daemon that stops kills the container's bind mount** (see
  #18). Config changes and upgrades no longer restart it.
- **Metadata still goes through the daemon.** Since passthrough, file data
  doesn't; lookups, `stat` and `readdir` do, cached for `ttl` seconds.
- **One `O_PATH` fd per inode the kernel holds.** The module sets
  `LimitNOFILE=1048576`. For very large trees, switch to file handles
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

### Security

- **#1 State file was world-writable (critical).** The process-wide
  `umask(0)` made `NAME.json` 0666, so any local user could add a link that
  the root daemon replayed.
  - Now the process umask is 022, and only FUSE workers use a private umask
    of 0 (`unshare(CLONE_FS)`).
  - The state file is written fresh with `O_EXCL` and mode 0600.
  - State and config files that anyone other than root or the daemon's user
    could write are refused.
- **#2 Mountpoints were created 0777 (high).** Same cause. Now they're
  created 0755, a symlinked mountpoint is refused, and stale-mount handling
  uses `lstat` and `UMOUNT_NOFOLLOW`. What's left is #15.
- **#3 Ownership of new entries could be stolen through a race (high).** The
  daemon created as root, then chowned whatever the name resolved to. Now
  `sys::AsCaller` switches the worker thread's fsuid/fsgid (plus
  `CAP_DAC_OVERRIDE`) for the creating syscall only. The kernel sets owner,
  group and setgid inheritance, and nothing is chowned.

### Correctness

- **#4 A link whose source was missing at start was lost for good (high).**
  Such entries are now kept as *pending*: listed by `ls` with the reason,
  saved in the state file, and retried every `--retry` seconds (default 5).
- **#5 A config/state conflict caused a restart loop (medium).** The config
  is now applied first and the state second, and nothing fails startup. A
  conflicting state entry becomes pending (config wins).
- **#6 Removing a link from the NixOS config didn't revoke it (medium).**
  Config entries (`--config`, `--link`, `--mkdir`) are never written to the
  state file, and `ls` tags them `[config]`. State files from older versions
  that hold config entries are handled.
- **#7 A crash during `save()` could block startup (medium).** `save()` now
  fsyncs the file and its directory. A state file that can't be parsed
  (including a truncated one) is moved to `NAME.corrupt-<time>`, and the
  instance starts without it.
- **#8 `fsyncdir` was a no-op (medium).** It now fsyncs the host directory.
- **#9 `--link SRC:DST` split at the first `:` (low).** It now splits at the
  last `:/`. The NixOS module no longer builds these strings at all: it
  passes a generated JSON `--config` file.

### Cleanup (code climate)

- **#10** The tree walk existed three times. It's now one `State::walk`,
  plus `ensure_dirs`, `attach` and `unhook`.
- **#11** Handles are typed, `RealRef` has `From<&Real>`, and xattrs have
  separate read and write paths. The `unreachable!()`s are gone.
- **#12** `setattr` and `snapshot` were split into helpers.
- **#13** In `fs.rs`, the reply helpers (`send`, `empty`, `entry`, …) replace
  25 copied error arms.
- **#14** `serve` was split into `Instance::resolve`, `config_entries`,
  `mount_session` and `run_until_signal`, in `serve.rs`.

### Shipping

- **#16** `rm --pending` only drops a pending entry, and keeps what is live
  at the target.
- **Kernel FUSE passthrough:** open files read, write and mmap directly
  against the host file. There is one backing id per inode, refcounted
  across opens, with a fallback to daemon I/O. `coffeefs status` shows the
  counters. In the VM test, 16 MB of I/O through the mount causes no daemon
  reads or writes.
- **Hot reload:** `SIGHUP` (`Type=notify-reload`) applies config changes
  without remounting. The module installs the config at
  `/etc/coffeefs/NAME.json`, sets `reloadTriggers`, and makes host-mode
  instances reload instead of restart. The VM test switches to a
  specialisation and checks that the daemon PID stays the same, that links
  change inside the container, and that runtime links stay.
- **systemd hardening:** a capability bounding set, `NoNewPrivileges`,
  `AF_UNIX` only, `RestrictNamespaces=mnt`, `MemoryDenyWriteExecute`, and
  `LimitNOFILE`.
- **`containerPath`** creates the container's bind mount automatically.
- **Flake checks:** `package` (with the unit tests), `clippy`, `fmt`, `vm`.
