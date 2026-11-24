//! Mounting straight into another process's mount namespace (e.g. a running
//! container), while the daemon itself stays on the host.
//!
//! A helper thread gives itself a private fs_struct (`unshare(CLONE_FS)`),
//! which is what allows `setns(CLONE_NEWNS)` in a multi-threaded process and
//! confines the namespace switch to that one thread. It performs the mount
//! there and exits; the FUSE connection lives on in our /dev/fuse fd.

use nix::mount::{MntFlags, MsFlags};
use nix::sched::CloneFlags;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;

pub fn open_ns(path: &Path) -> io::Result<OwnedFd> {
    Ok(std::fs::File::open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?
        .into())
}

pub fn open_dev_fuse() -> io::Result<OwnedFd> {
    Ok(std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .map_err(|e| io::Error::new(e.kind(), format!("/dev/fuse: {e}")))?
        .into())
}

fn in_ns<T: Send>(ns: BorrowedFd<'_>, f: impl FnOnce() -> io::Result<T> + Send) -> io::Result<T> {
    std::thread::scope(|sc| {
        sc.spawn(|| {
            nix::sched::unshare(CloneFlags::CLONE_FS)?;
            nix::sched::setns(ns, CloneFlags::CLONE_NEWNS)?;
            f()
        })
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("namespace helper thread panicked")))
    })
}

/// Lazy unmount. In our own namespace an unprivileged daemon falls back to
/// the setuid helper; inside a foreign namespace we're root anyway.
fn lazy_umount(target: &Path, own_ns: bool) -> io::Result<()> {
    match nix::mount::umount2(target, MntFlags::MNT_DETACH) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::EPERM) if own_ns => {
            let st = std::process::Command::new("fusermount3")
                .arg("-u")
                .arg("-z")
                .arg(target)
                .status()?;
            if st.success() {
                Ok(())
            } else {
                Err(io::Error::other("fusermount3 -u failed"))
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// A previous instance that died without unmounting leaves a dead FUSE mount
/// behind (every access fails with ENOTCONN). Detach it so we can remount.
fn clear_stale_in(target: &Path, own_ns: bool) -> io::Result<()> {
    match std::fs::metadata(target) {
        Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) => {
            log::warn!("{}: clearing stale mount", target.display());
            lazy_umount(target, own_ns)
        }
        _ => Ok(()),
    }
}

pub fn clear_stale(target: &Path) -> io::Result<()> {
    clear_stale_in(target, true)
}

/// Mount the FUSE connection `fuse` at `target` in the current namespace
/// (needs root). Clears a stale mount and creates the directory first.
fn mount_fuse(target: &Path, fuse: BorrowedFd<'_>, fsname: &str, own_ns: bool) -> io::Result<()> {
    let opts = format!(
        "fd={},rootmode=40000,user_id={},group_id={},allow_other,default_permissions",
        fuse.as_raw_fd(),
        nix::unistd::geteuid(),
        nix::unistd::getegid(),
    );
    clear_stale_in(target, own_ns)?;
    std::fs::create_dir_all(target)?;
    nix::mount::mount(
        Some(fsname),
        target,
        Some("fuse.coffeefs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some(opts.as_str()),
    )
    .map_err(|e| {
        io::Error::new(
            io::Error::from(e).kind(),
            format!("mount {}: {e}", target.display()),
        )
    })
}

pub fn mount_here(target: &Path, fuse: BorrowedFd<'_>, fsname: &str) -> io::Result<()> {
    mount_fuse(target, fuse, fsname, true)
}

pub fn mount_in_ns(
    ns: BorrowedFd<'_>,
    target: &Path,
    fuse: BorrowedFd<'_>,
    fsname: &str,
) -> io::Result<()> {
    in_ns(ns, || mount_fuse(target, fuse, fsname, false))
}

/// Lazily detach `target`, in namespace `ns` if given.
pub fn detach(ns: Option<BorrowedFd<'_>>, target: &Path) -> io::Result<()> {
    match ns {
        Some(ns) => in_ns(ns, || lazy_umount(target, false)),
        None => lazy_umount(target, true),
    }
}
