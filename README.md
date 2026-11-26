# coffeefs ☕

[![CI](https://github.com/vibec0re/coffee/actions/workflows/ci.yml/badge.svg)](https://github.com/vibec0re/coffee/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

Selective file access for containers: a FUSE passthrough filesystem whose
contents are wired up **at runtime**.

Each instance mounts an initially empty tree, for example at
`/containers/choom1`. You link host files and directories into it, and take
them out again, either declaratively (NixOS) or live through a control
socket that stays on the host, out of the container's reach. Reads and writes
go straight to the real files; on Linux 6.9+ the kernel does that by itself
(FUSE passthrough), so a build inside the container runs at close to native
speed. A link is like a bind mount you can add or revoke without touching
the container's mount table, and without restarting anything.

```console
# coffeefs serve choom1                          # mounts /containers/choom1
# coffeefs add choom1 /home/annika/proj /work/proj
# coffeefs add choom1 /etc/resolv.conf /etc/resolv.conf --ro
# coffeefs ls choom1
/etc/resolv.conf -> /etc/resolv.conf (ro)
/work/proj -> /home/annika/proj
# coffeefs rm choom1 /work/proj                   # gone, effective immediately
```

## Giving a choom access

With NixOS (see [below](#nixos) for the full module):

```nix
services.coffeefs.instances.choom1 = {
  container = "choom1";
  containerPath = "/work";          # bind-mounted into the container here
  links = [
    { source = "/home/annika/proj"; target = "/proj"; }            # read-write
    { source = "/srv/datasets";      target = "/data"; ro = true; } # read-only
  ];
};
```

Edit `links` and `nixos-rebuild switch`: the daemon **reloads in place**. New
links appear and dropped ones disappear inside the running container. The
mount, files the container has open, and links added at runtime all stay as
they are.

Ad hoc, from the host:

| | |
|---|---|
| grant | `coffeefs add choom1 /host/path /in/fs/path [--ro]` |
| revoke | `coffeefs rm choom1 /in/fs/path` |
| audit | `coffeefs ls choom1` (`[config]` = from the NixOS config, `[pending: …]` = not applied yet) |
| health | `coffeefs status choom1` |

Runtime grants survive daemon restarts (`persist`, on by default). Config
grants are re-applied on every start. Revoking either one takes effect at
once: the kernel's caches are invalidated, and new opens fail. Files that are
already open stay usable, just as after a lazy unmount.

Things to know:
- **Least privilege:** links are read-write unless you say `ro`. Only the
  linked paths exist in the tree. The rest of it is a read-only skeleton.
- **Escape-proof:** a link to a directory gives access to that directory, and
  nothing else. Symlinks inside it resolve in the container's view, and
  `..` can't leave it.
- **Ownership:** files the container creates belong to the user who created
  them inside the container. That uid is the same on the host (nspawn without
  user namespaces).

## A root: the default route

By default a choom sees only what is linked in, with a read-only skeleton
holding it together. Give an instance a **root** instead, and everything no
link covers falls through to that host directory, typically the choom's own
home:

```console
# coffeefs root choom1 /home/annika/choom1     # or: serve --root, NixOS `root`
# cd ~/code && coffeefs add choom1 ./project   # -> /containers/choom1/project
# coffeefs ls choom1
/ -> /home/annika/choom1 (root)
/project -> /home/annika/code/project
```

`/containers/choom1` is then the home itself, writable, with the links laid
over it, so the choom can keep `~/.cache`, dotfiles, and build output there.
The links work like mounts:
- **A link covers** whatever the root has under the same name, and can't be
  removed or renamed from inside (`EBUSY`).
- **A link further down**, such as `/work/proj`, leaves the rest of the
  root's `work/` visible next to it.
- **Renames** work anywhere within the root, but not between the root and a
  link (`EXDEV`).

`--ro` makes the root read-only, so only read-write links can be written.
There is one root per instance: change it with `coffeefs root … --replace`,
or clear it with `coffeefs rm choom1 /`. A root set in the NixOS config wins
over a runtime one.

## Model

```
/containers/choom1/          virtual dir (read-only skeleton)
├── etc/                     virtual dir, created implicitly
│   └── resolv.conf  ──────▶ host /etc/resolv.conf     (file link, ro)
└── work/
    └── proj/        ──────▶ host /home/annika/proj    (dir link, rw)
        └── …                everything below is plain passthrough
```

- **Links** map a path in the fs to a host file or directory. Missing parent
  directories are created as virtual directories and pruned again when they
  become empty. `mkdir` creates a virtual dir that stays, which is useful as a
  mountpoint.
- The **virtual skeleton** is read-only from inside (`EROFS`). Only the
  control socket changes it. With a [root](#a-root-the-default-route), a
  virtual directory falls through to the root's directory of the same name,
  if there is one, and is writable like it.
- **Below a link** everything passes through: create, write, rename, chmod,
  symlinks, hardlinks, `user.*` xattrs, fallocate, copy_file_range, mmap, and
  so on. `ro` links reject every write with `EROFS`.
- **Removing a link** invalidates the kernel's caches straight away. Files
  that are still open, and processes whose cwd is inside the link, keep
  working.
- **Multiple instances**: each `serve` is an independent process with its
  own mount, socket and optional state file. Use one per container.

## CLI

```
coffeefs serve  NAME [-m MOUNTPOINT] [-s SOCKET] [--state FILE] [--config FILE]...
                     [--root DIR | --root-ro DIR]
                     [--link SRC[:DST]]... [--link-ro SRC[:DST]]... [--mkdir DIR]...
                     [--pid PID | --mount-ns NSFILE] [--threads N] [--ttl SECS]
                     [--retry SECS] [--no-passthrough]
coffeefs add    INSTANCE SOURCE [TARGET] [--ro] [--replace]
coffeefs rm     INSTANCE TARGET [-r] [--pending]
coffeefs mkdir  INSTANCE TARGET
coffeefs root   INSTANCE DIR [--ro] [--replace]
coffeefs ls     INSTANCE [--json]
coffeefs status INSTANCE [--json]
```

`INSTANCE` is a name, which resolves to `/run/coffeefs/NAME.sock` (or
`$XDG_RUNTIME_DIR/coffeefs/` when not root; override with
`COFFEEFS_SOCKET_DIR`), or a path to a socket. The mountpoint defaults to
`/containers/NAME`. SOURCE and DIR may be relative to the current directory.
TARGET defaults to `/NAME`, NAME being SOURCE's last component, so
`add choom1 ./project` links it at `/project`. `rm INSTANCE /` clears the root.

`SIGHUP` re-reads the `--config` files and applies the difference without
remounting (`systemctl reload`). `SIGTERM`/`SIGINT` unmount and exit.

### Config and runtime entries

Entries come from two places, and `ls` says which with a `[config]` tag:

- **Config**: `--config FILE`, `--root`, `--link`, `--link-ro` and `--mkdir`. These are
  declarative. They are set up first on every start (and on reload), and are
  never written to the state file, so taking one out of the config removes it.
- **Runtime**: `add`, `mkdir`, `root` and `rm` over the socket. With `--state FILE`
  they are saved after each change (fresh 0600 file, fsync, rename) and
  replayed after the config at startup.

A `--config` file uses the state file's format:

```json
[{"kind":"root","source":"/home/annika/choom1"},
 {"kind":"dir","target":"/tmp"},
 {"kind":"link","source":"/srv/x","target":"/x","ro":true}]
```

`--link SRC:DST` splits at the last `:/`, so SRC may contain `:`. For paths
that are stranger still, use `--config`.

Startup or reload doesn't fail just because an entry can't be applied. Such
an entry becomes **pending**: `ls` shows it with the reason, it stays in the
state file, and it is retried every `--retry` seconds (default 5) until it
works or you `rm` it (`rm --pending` drops only the pending entry). Two cases
lead to this:
- **The source isn't there yet,** e.g. `/home` mounts after the daemon starts.
- **The entry conflicts with the config.** The config wins, and the runtime
  entry waits.

A state file that can't be parsed is renamed to `NAME.corrupt-<time>`, and
the instance starts without it. A state or config file that anyone other
than root or the daemon's user could write is refused.

### Mounting into a running container

```console
# coffeefs serve choom1 --pid "$(machinectl show -p Leader --value choom1)" -m /coffee
```

This mounts the fs at `/coffee` inside the container's mount namespace. The
daemon itself, its control socket and the host paths stay on the host. You
don't need a bind mount and the container doesn't need a restart. If the
daemon restarts, it mounts again in the same place.

### Control protocol

Newline-delimited JSON on the socket. You can send several requests per
connection:

```console
$ echo '{"op":"add","source":"/srv/x","target":"/x","ro":true}' | socat - UNIX:/run/coffeefs/choom1.sock
{"ok":true}
```

The ops:
- `add {source,target,ro?,replace?}`
- `rm {target,recursive?,pending?}`
- `mkdir {target}`
- `root {source,ro?,replace?}`: clear it with `rm` at `/`
- `ls`: returns `entries`. Each one is a state-file entry plus `origin`
  (`config` or `runtime`), and for pending ones `pending` (the reason).
- `status`: returns `status`, with these fields:
  - `passthrough`;
  - `opens` and `passthrough_opens`;
  - `daemon_reads` and `daemon_writes`;
  - `inodes` and `open_handles`;
  - `entries`, `pending` and `uptime_secs`.

The socket is mode `0600`, and every connection is also checked with
`SO_PEERCRED`: only root or the daemon's own user may connect.

## NixOS

```nix
{
  inputs.coffeefs.url = "path:/home/annika/viberoot/coffeefs";  # or wherever it lives
  # …
  imports = [ inputs.coffeefs.nixosModules.default ];

  services.coffeefs.instances = {
    # host mount at /containers/choom1, started before container@choom1 and
    # bind-mounted into it at /work
    choom1 = {
      container = "choom1";
      containerPath = "/work";
      links = [ { source = "/srv/shared"; target = "/shared"; ro = true; } ];
      dirs = [ "/scratch" ];
    };
    # or: mounted directly inside the running container at /coffee; starts
    # and stops with the container
    choom1-live = {
      container = "choom1";
      inContainerNamespace = true;
      mountpoint = "/coffee";
    };
  };
}
```

| option | default | |
|---|---|---|
| `mountpoint` | `/containers/NAME` | inside the container with `inContainerNamespace` |
| `container` | `null` | the NixOS container (nspawn machine) it belongs to |
| `containerPath` | `null` | host mode: bind-mount into the container here |
| `inContainerNamespace` | `false` | mount inside the running container instead of on the host |
| `root` | `null` | host dir everything else falls through to: a path, or `{ source, ro }` |
| `links`, `dirs` | `[]` | config entries: `{ source, target ? "/NAME", ro ? false }`, virtual dirs |
| `persist` | `true` | keep runtime changes in `/var/lib/coffeefs/NAME.json` |
| `passthrough` | `true` | kernel FUSE passthrough where available |
| `threads`, `ttl` | `4`, `"1"` | FUSE workers, kernel cache timeout (s) |
| `extraArgs` | `[]` | more `coffeefs serve` flags (e.g. `--retry`) |

Each instance becomes `coffeefs-NAME.service` (`Type=notify-reload`):

- **Changing `links`/`dirs`**: the config lives at `/etc/coffeefs/NAME.json`,
  so a rebuild only *reloads* the daemon. Nothing is remounted.
- **Upgrading the package** of a host-mode instance also just reloads. The
  old binary keeps serving, because a restart would leave containers that
  bind-mount it with a dead mount until they restart. To pick up a new
  version, restart the daemon and the container in one go:
  `systemctl restart coffeefs-choom1 container@choom1`.
  In-namespace instances restart, since they remount in place.
- **Hardening**:
  - a capability bounding set: `SYS_ADMIN`, `SYS_CHROOT`, `SYS_PTRACE`,
    `DAC_*`, `FOWNER`, `FSETID`, `CHOWN`, `SETUID`, `SETGID`;
  - `NoNewPrivileges`, `AF_UNIX` only, `RestrictNamespaces=mnt`, `MemoryDenyWriteExecute`;
  - `LimitNOFILE=1048576`.

  Options that give the service its own mount namespace (`ProtectSystem`,
  `PrivateTmp`, …) are deliberately absent: the mount must land on the host,
  and passthrough needs the host's writable view of linked files.

## Performance

On Linux 6.9+ (`CONFIG_FUSE_PASSTHROUGH`, on in NixOS kernels) with a root
daemon, opening a file registers the host file with the kernel. From then on,
reads, writes and mmaps of that open file never reach the daemon. Lookups,
`stat`, directory listings and opens still do (cached for `ttl` seconds).
`coffeefs status` shows whether passthrough is on, and how much I/O still goes
through the daemon.

If passthrough isn't available, all I/O goes through the daemon:
- **Kernel too old.** The init handshake refuses it.
- **Daemon not root.** The kernel refuses with `EPERM`.
- **Backing file on overlayfs.** This is decided per file. Stacking depth 1
  means the container may in turn put an overlayfs on top of coffeefs.

## Security model

The daemon normally runs as root on the host. The container sees only what is
linked in, and the design keeps it from reaching beyond that:

- **No host path is ever re-resolved.** Every inode holds an `O_PATH` fd, and
  children are opened one component at a time with `openat(…, O_NOFOLLOW)`.
  A symlink planted in a shared dir (`evil -> /etc`) goes back to the kernel
  as a symlink and resolves in the container's view, never on the host.
  Opening, truncating, chmod-ing or creating through a symlink fails.
- **The root never leads elsewhere.** It is writable from inside, so a
  choom can plant `work -> /etc` in it. A virtual `/work` then has no
  backing at all rather than `/etc`: backings are opened with `O_NOFOLLOW`
  too.
- **Links are isolated from each other.** Inodes are keyed per link, so
  `rename` and `link` across links return `EXDEV`, just like separate bind
  mounts. That matters when the same host dir is linked twice, once `ro` and
  once `rw`: a file can't be hardlinked out of the read-only view into the
  writable one.
- **Permissions**: the fs is mounted with `default_permissions`, so the kernel
  checks host modes and owners against the caller. New files, dirs, fifos and
  symlinks are created *as* the caller: the worker thread switches its
  fsuid/fsgid for that one syscall, so the host kernel sets owner, group
  (including setgid-directory inheritance) and mode. There is no
  chown-afterwards step to race.
- **Passthrough** doesn't widen access. The kernel opens the backing file
  with the flags of the FUSE open, which was already checked: a read-only
  link can only ever be opened read-only.
- The daemon's own files are private. The state file is 0600 and written
  fresh on every save. Startup refuses state and config files that anyone
  other than root or the daemon's user can write. Mountpoints and their
  missing parents are created 0755, and a symlinked mountpoint is refused.
- Mounts are `nosuid,nodev`. Device nodes can't be created. Only the `user.*`
  xattr namespace passes through, so `trusted.*`, `security.capability` and
  the like are blocked.

## Caveats

- **POSIX ACLs are not enforced.** The kernel checks mode bits only. On files
  with ACLs the group bits show the ACL mask, which can grant more than the
  ACL would.
- **No uid mapping.** Owners pass through 1:1, which is right for nspawn
  without user namespaces (`privateUsers`) and wrong with them.
- Inode numbers differ from the host's. A host file reached through two links
  appears as two inodes.
- POSIX/flock locks are local to the mount and don't coordinate with host
  processes.
- Unix sockets inside linked dirs are visible but can't be connected to
  through FUSE.
- A host-mode daemon that stops (crash, manual restart) leaves containers
  that bind-mounted it with a dead mount (`ENOTCONN`) until they restart.
  `inContainerNamespace` instances just remount.

See [ISSUES.md](ISSUES.md) for open issues and ideas.

## Development

```console
$ nix develop -c cargo test                       # unit tests (core + control plane), unprivileged
$ nix flake check                                 # + clippy, rustfmt, and the VM test
$ nix build .#checks.x86_64-linux.vm -L           # NixOS VM: real kernel, nspawn container,
                                                  # 3 instances, passthrough, config switch
```

CI (`.github/workflows/ci.yml`) runs the same flake checks on every push and
pull request, including the VM test on a KVM-enabled runner.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
