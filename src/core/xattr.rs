//! Extended attributes. Only the `user.` namespace is passed through: the
//! daemon runs as root, and letting a container read or write `trusted.*` /
//! `security.*` (file capabilities, overlayfs metadata, ...) on host files
//! would be a privilege leak.

use super::{Core, NodeRef, RealRef, S_IFLNK};
use crate::sys::{self, Res};
use std::ffi::OsStr;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;

fn is_user(name: &[u8]) -> bool {
    name.starts_with(b"user.")
}

impl Core {
    /// Where to read xattrs from (a virtual dir's backing, if it has one).
    /// `None`: nothing to read there (virtual dirs without one; symlinks,
    /// which can't carry `user.*` and which the /proc reopen would follow).
    fn xattr_src(&self, ino: u64) -> Res<Option<RealRef>> {
        Ok(match self.get(ino)? {
            NodeRef::Real(r) | NodeRef::Virt(_, Some(r)) if r.fmt != S_IFLNK => Some(r),
            _ => None,
        })
    }

    fn xattr_dst(&self, ino: u64, name: &OsStr) -> Res<RealRef> {
        if !is_user(name.as_bytes()) {
            return Err(libc::EPERM);
        }
        let r = self.real(ino, libc::EROFS)?;
        if r.ro {
            return Err(libc::EROFS);
        }
        if r.fmt == S_IFLNK {
            return Err(libc::EPERM);
        }
        Ok(r)
    }

    pub fn getxattr(&self, ino: u64, name: &OsStr) -> Res<Vec<u8>> {
        if !is_user(name.as_bytes()) {
            return Err(libc::ENODATA);
        }
        let r = self.xattr_src(ino)?.ok_or(libc::ENODATA)?;
        sys::getxattr(&sys::proc_path(r.fd.as_fd()), &sys::cstring(name)?)
    }

    pub fn listxattr(&self, ino: u64) -> Res<Vec<u8>> {
        let Some(r) = self.xattr_src(ino)? else {
            return Ok(Vec::new());
        };
        let raw = sys::listxattr(&sys::proc_path(r.fd.as_fd()))?;
        Ok(raw
            .split_inclusive(|&b| b == 0)
            .filter(|n| is_user(n))
            .flatten()
            .copied()
            .collect())
    }

    pub fn setxattr(&self, ino: u64, name: &OsStr, value: &[u8], flags: i32) -> Res<()> {
        let r = self.xattr_dst(ino, name)?;
        let path = sys::proc_path(r.fd.as_fd());
        sys::setxattr(&path, &sys::cstring(name)?, value, flags)
    }

    pub fn removexattr(&self, ino: u64, name: &OsStr) -> Res<()> {
        let r = self.xattr_dst(ino, name)?;
        sys::removexattr(&sys::proc_path(r.fd.as_fd()), &sys::cstring(name)?)
    }
}
