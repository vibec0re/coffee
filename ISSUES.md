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

### 16. `rm` removes a live entry and a pending one at the same target (low)

Suppose a runtime entry is pending because it conflicts with a config entry
at the same target. `rm TARGET` removes both. After a restart the config
entry comes back, but until then it's gone.

**Fix:** an `rm --pending` flag (or `"pending":true` in the request) that only
drops the pending entry.

### 17. `tests.rs` is long (low)

The file is 1055 lines. Ten tests are over 25 lines, and the setup code
(controller with a state file, `fs::write` + `chmod 600`) repeats.

**Fix:** a small fixture, `Inst::new(config)`, holding the controller, the
state path and a host dir, then split the file by area (`core`, `ctl`, `perm`).

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
- **Restarting a host-mode instance kills the container's bind mount.** The
  container sees `ENOTCONN` until it restarts. `inContainerNamespace` avoids
  this. Possible fix: hand the `/dev/fuse` fd to systemd's fd store
  (`FDSTORE=1`) and pick the session back up on restart.
- **All I/O goes through the daemon.** Kernel FUSE passthrough (Linux 6.9+,
  `FUSE_PASSTHROUGH` with backing fds) would let reads and writes skip the
  daemon.
- **One `O_PATH` fd per inode the kernel holds.** Very large trees can hit
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
