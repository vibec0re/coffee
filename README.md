# coffeefs ☕

A FUSE passthrough filesystem whose contents are wired up **at runtime**.

Each instance mounts an initially empty tree, for example at `/containers/choom1`.
Host files and directories are linked into it while it runs, and removed again,
through a control socket that stays on the host, outside the container. Reads
and writes go straight to the real files, so the links work like hardlinks you
can add on the fly, or like bind mounts you can add without touching the
container's mount table.

```console
# coffeefs serve choom1                          # mounts /containers/choom1
# coffeefs add choom1 /home/annika/proj /work/proj
# coffeefs add choom1 /etc/resolv.conf /etc/resolv.conf --ro
# coffeefs ls choom1
/etc/resolv.conf -> /etc/resolv.conf (ro)
/work/proj -> /home/annika/proj
# coffeefs rm choom1 /work/proj                   # gone, effective immediately
```

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
  control socket changes it.
- **Below a link** everything passes through: create, write, rename, chmod,
  symlinks, hardlinks, `user.*` xattrs, fallocate, copy_file_range, and so on.
  `--ro` links reject every write with `EROFS`.
- **Removing a link** invalidates the kernel's caches straight away. Files
  that are still open, and processes whose cwd is inside the link, keep
  working, just as they would after a lazy unmount.
- **Multiple instances**: each `serve` is an independent process with its
  own mount, socket and optional state file.

## CLI

```
coffeefs serve NAME [-m MOUNTPOINT] [-s SOCKET] [--state FILE] [--config FILE]...
                    [--link SRC[:DST]]... [--link-ro SRC[:DST]]... [--mkdir DIR]...
                    [--pid PID | --mount-ns NSFILE] [--threads N] [--ttl SECS] [--retry SECS]
coffeefs add   INSTANCE SOURCE [TARGET] [--ro] [--replace]
coffeefs rm    INSTANCE TARGET [-r]
coffeefs mkdir INSTANCE TARGET
coffeefs ls    INSTANCE [--json]
```

`INSTANCE` is a name, which resolves to `/run/coffeefs/NAME.sock` (or
`$XDG_RUNTIME_DIR/coffeefs/` when not root; override with
`COFFEEFS_SOCKET_DIR`), or a path to a socket. The mountpoint defaults to
`/containers/NAME`.

### Config and runtime entries

Entries come from two places, and `ls` says which with a `[config]` tag:

- **Config**: `--config FILE`, `--link`, `--link-ro` and `--mkdir`. These are
  declarative. They are set up first on every start and never written to the
  state file, so taking one out of the config removes it.
- **Runtime**: `add`, `mkdir` and `rm` over the socket. With `--state FILE`
  they are saved after each change (fresh 0600 file, fsync, rename) and
  replayed after the config at startup.

A `--config` file uses the state file's format:

```json
[{"kind":"dir","target":"/tmp"},
 {"kind":"link","source":"/srv/x","target":"/x","ro":true}]
```

`--link SRC:DST` splits at the last `:/`, so SRC may contain `:`. For paths
that are stranger still, use `--config`.

Startup doesn't fail just because an entry can't be applied. Such an entry
becomes **pending**: `ls` shows it with the reason, it stays in the state
file, and it is retried every `--retry` seconds (default 5) until it works
or you `rm` it. This covers two cases:

- The source isn't there yet, e.g. `/home` mounts after the daemon starts.
- The entry conflicts with the config. The config wins, and the runtime entry
  waits.

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

Ops: `add {source,target,ro?,replace?}`, `rm {target,recursive?}`,
`mkdir {target}`, `ls`. `ls` returns `entries`, each a state-file entry plus
`origin` (`config` or `runtime`) and, for pending ones, `pending` (the reason).

The socket is mode `0600`, and every connection is also checked with
`SO_PEERCRED`: only root or the daemon's own user may connect.

## NixOS

```nix
{
  inputs.coffeefs.url = "path:/home/annika/viberoot/coffeefs";  # or wherever it lives
  # …
  imports = [ inputs.coffeefs.nixosModules.default ];

  services.coffeefs.instances = {
    # host mount at /containers/choom1, started before container@choom1
    choom1 = {
      container = "choom1";
      links = [ { source = "/srv/shared"; target = "/shared"; ro = true; } ];
    };
    # mounted directly inside the container at /coffee; starts and stops
    # with the container
    choom1-live = {
      container = "choom1";
      inContainerNamespace = true;
      mountpoint = "/coffee";
    };
  };

  containers.choom1.bindMounts."/work" = {
    hostPath = "/containers/choom1";
    isReadOnly = false;
  };
}
```

Each instance becomes `coffeefs-NAME.service` (`Type=notify`,
`Restart=on-failure`). `links` and `dirs` are passed as a generated `--config`
file, so removing one and rebuilding revokes it. Runtime changes are kept in
`/var/lib/coffeefs/NAME.json` (`persist`, on by default).

## Security model

The daemon normally runs as root on the host. The container sees only what is
linked in, and the design keeps it from reaching beyond that:

- **No host path is ever re-resolved.** Every inode holds an `O_PATH` fd, and
  children are opened one component at a time with `openat(…, O_NOFOLLOW)`.
  A symlink planted in a shared dir (`evil -> /etc`) goes back to the kernel
  as a symlink and resolves in the container's view, never on the host.
  Opening, truncating, chmod-ing or creating through a symlink fails.
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
- The daemon's own files are private. The state file is 0600 and written
  fresh on every save. Startup refuses a state file that anyone else can
  write. Mountpoints and their missing parents are created 0755, and a
  symlinked mountpoint is refused.
- Mounts are `nosuid,nodev`. Device nodes can't be created. Only the `user.*`
  xattr namespace passes through, so `trusted.*`, `security.capability` and
  the like are blocked.

## Caveats

- **POSIX ACLs are not enforced.** The kernel checks mode bits only. On files
  with ACLs the group bits show the ACL mask, which can grant more than the
  ACL would.
- Inode numbers differ from the host's. A host file reached through two links
  appears as two inodes.
- POSIX/flock locks are local to the mount and don't coordinate with host
  processes.
- Unix sockets inside linked dirs are visible but can't be connected to
  through FUSE.
- If you restart a host-mode instance, containers that bind-mounted the old
  mount keep a dead mount (`ENOTCONN`) until the container restarts. Use
  `inContainerNamespace` / `--pid` to avoid this.
- All I/O goes through the daemon. Kernel FUSE passthrough (Linux 6.9+) would
  remove that overhead, but it isn't used yet.

## Development

```console
$ nix develop -c cargo test                     # unprivileged unit tests (core + control plane)
$ nix build .#checks.x86_64-linux.vm -L         # NixOS VM: real kernel, nspawn container, 3 instances
```
