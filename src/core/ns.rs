//! Namespace operations inside linked directories: create, remove, rename,
//! link.

use super::io::OPEN_STRIP;
use super::{Caller, Core, Handle, RealRef, S_IFDIR, S_IFMT};
use crate::sys::{self, Res};
use fuser::FileAttr;
use std::ffi::{CStr, OsStr};
use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::Path;
use std::sync::Arc;

impl Core {
    /// Switch this thread to the requester for the creating syscall.
    fn as_caller(&self, c: Caller) -> Res<sys::AsCaller> {
        sys::AsCaller::enter(c.uid, c.gid, self.switch_creds)
    }

    /// Pick up an object that was just created (or linked) in `p`.
    /// Resolving the name again can race with a rename in the host
    /// directory, but then we merely report what is there now, exactly as a
    /// lookup would; nothing is changed on it.
    fn pick_up(&self, p: &RealRef, name: &CStr) -> Res<FileAttr> {
        let fd = sys::openat(p.fd.as_fd(), name, libc::O_PATH | libc::O_NOFOLLOW, 0)?;
        self.register(fd, p.root, p.ro)
    }

    /// Create `name` in `parent` as `caller` with `make(dirfd, name)`.
    fn make(
        &self,
        parent: u64,
        name: &OsStr,
        caller: Caller,
        make: impl FnOnce(BorrowedFd<'_>, &CStr) -> Res<()>,
    ) -> Res<FileAttr> {
        let p = self.wdir(parent, name, libc::EEXIST)?;
        let c = sys::name(name)?;
        {
            let _id = self.as_caller(caller)?;
            make(p.fd.as_fd(), &c)?;
        }
        self.pick_up(&p, &c)
    }

    pub fn mkdir(&self, parent: u64, name: &OsStr, mode: u32, caller: Caller) -> Res<FileAttr> {
        let m = caller.mode(mode);
        self.make(parent, name, caller, |d, c| sys::mkdirat(d, c, m))
    }

    pub fn mknod(&self, parent: u64, name: &OsStr, mode: u32, caller: Caller) -> Res<FileAttr> {
        let fmt = mode & S_IFMT;
        // no device nodes on the host on behalf of a container
        if !matches!(fmt, libc::S_IFREG | libc::S_IFIFO | libc::S_IFSOCK) {
            return Err(libc::EPERM);
        }
        let m = fmt | caller.mode(mode);
        self.make(parent, name, caller, |d, c| sys::mknodat(d, c, m))
    }

    pub fn symlink(
        &self,
        parent: u64,
        name: &OsStr,
        target: &Path,
        caller: Caller,
    ) -> Res<FileAttr> {
        let t = sys::cstring(target.as_os_str())?;
        self.make(parent, name, caller, |d, c| sys::symlinkat(&t, d, c))
    }

    /// Open (and create if needed) `name` in `parent`.
    pub fn create(
        &self,
        parent: u64,
        name: &OsStr,
        mode: u32,
        flags: i32,
        caller: Caller,
    ) -> Res<(FileAttr, u64)> {
        let p = self.wdir(parent, name, libc::EEXIST)?;
        let c = sys::name(name)?;
        let base = (flags & !OPEN_STRIP) | libc::O_NOFOLLOW;
        let file = {
            let _id = self.as_caller(caller)?;
            let m = caller.mode(mode);
            match sys::openat(p.fd.as_fd(), &c, base | libc::O_CREAT | libc::O_EXCL, m) {
                Err(libc::EEXIST) if flags & libc::O_EXCL == 0 => {
                    sys::openat(p.fd.as_fd(), &c, base, 0)?
                }
                r => r?,
            }
        };
        let pathfd = sys::reopen(file.as_fd(), libc::O_PATH)?;
        let attr = self.register(pathfd, p.root, p.ro)?;
        let fh = self.new_handle(Handle::File(Arc::new(File::from(file))));
        Ok((attr, fh))
    }

    pub fn unlink(&self, parent: u64, name: &OsStr) -> Res<()> {
        let p = self.wdir(parent, name, libc::EBUSY)?;
        sys::unlinkat(p.fd.as_fd(), &sys::name(name)?, 0)
    }

    pub fn rmdir(&self, parent: u64, name: &OsStr) -> Res<()> {
        let p = self.wdir(parent, name, libc::EBUSY)?;
        sys::unlinkat(p.fd.as_fd(), &sys::name(name)?, libc::AT_REMOVEDIR)
    }

    pub fn rename(&self, from: (u64, &OsStr), to: (u64, &OsStr), flags: u32) -> Res<()> {
        if flags & libc::RENAME_WHITEOUT != 0 {
            return Err(libc::EINVAL);
        }
        let p = self.wdir(from.0, from.1, libc::EBUSY)?;
        let np = self.wdir(to.0, to.1, libc::EBUSY)?;
        if p.root != np.root {
            return Err(libc::EXDEV);
        }
        let (old, new) = (sys::name(from.1)?, sys::name(to.1)?);
        sys::renameat2(p.fd.as_fd(), &old, np.fd.as_fd(), &new, flags)
    }

    pub fn link(&self, ino: u64, newparent: u64, newname: &OsStr) -> Res<FileAttr> {
        let src = self.real(ino, libc::EPERM)?;
        let np = self.wdir(newparent, newname, libc::EEXIST)?;
        if src.root != np.root {
            return Err(libc::EXDEV);
        }
        if src.fmt == S_IFDIR {
            return Err(libc::EPERM);
        }
        let c = sys::name(newname)?;
        sys::link_fd(src.fd.as_fd(), np.fd.as_fd(), &c)?;
        self.pick_up(&np, &c)
    }
}
