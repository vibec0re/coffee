//! Attributes: host `stat` to FUSE attributes, setattr, readlink, statfs.

use super::{Core, NodeRef, RealRef, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, SetAttr, StatFs, TimeSpec};
use crate::sys::{self, Res};
use fuser::{FileAttr, FileType, INodeNo};
use std::fs::File;
use std::os::fd::AsFd;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn ts(sec: i64, nsec: i64) -> SystemTime {
    let nsec = Duration::from_nanos(nsec.unsigned_abs());
    match u64::try_from(sec) {
        Ok(s) => UNIX_EPOCH + Duration::from_secs(s) + nsec,
        Err(_) => UNIX_EPOCH - Duration::from_secs(sec.unsigned_abs()) + nsec,
    }
}

fn to_timespec(t: Option<&TimeSpec>) -> libc::timespec {
    const NS: i128 = 1_000_000_000;
    let special = |nsec| libc::timespec {
        tv_sec: 0,
        tv_nsec: nsec,
    };
    match t {
        None => special(libc::UTIME_OMIT),
        Some(TimeSpec::Now) => special(libc::UTIME_NOW),
        Some(TimeSpec::At(t)) => {
            let ns = match t.duration_since(UNIX_EPOCH) {
                Ok(d) => d.as_nanos() as i128,
                Err(e) => -(e.duration().as_nanos() as i128),
            };
            libc::timespec {
                tv_sec: ns.div_euclid(NS) as libc::time_t,
                tv_nsec: ns.rem_euclid(NS) as libc::c_long,
            }
        }
    }
}

pub(super) fn kind_of(mode: u32) -> FileType {
    match mode & S_IFMT {
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        libc::S_IFIFO => FileType::NamedPipe,
        libc::S_IFSOCK => FileType::Socket,
        libc::S_IFCHR => FileType::CharDevice,
        libc::S_IFBLK => FileType::BlockDevice,
        _ => FileType::RegularFile,
    }
}

pub(super) fn stat_attr(ino: u64, st: &libc::stat) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size: st.st_size as u64,
        blocks: st.st_blocks as u64,
        atime: ts(st.st_atime, st.st_atime_nsec),
        mtime: ts(st.st_mtime, st.st_mtime_nsec),
        ctime: ts(st.st_ctime, st.st_ctime_nsec),
        crtime: UNIX_EPOCH,
        kind: kind_of(st.st_mode),
        perm: (st.st_mode & 0o7777) as u16,
        nlink: st.st_nlink as u32,
        uid: st.st_uid,
        gid: st.st_gid,
        // the kernel's dev_t encoding (12-bit major, 20-bit minor) fits
        rdev: st.st_rdev as u32,
        blksize: st.st_blksize as u32,
        flags: 0,
    }
}

impl RealRef {
    /// Through the open file if there is one, else the `O_PATH` fd.
    fn set_mode(&self, f: Option<&File>, mode: u32) -> Res<()> {
        let mode = mode & 0o7777;
        match f {
            Some(f) => sys::fchmod(f.as_fd(), mode),
            None if self.fmt == S_IFLNK => Err(libc::EOPNOTSUPP),
            None => sys::chmod(&sys::proc_path(self.fd.as_fd()), mode),
        }
    }

    fn truncate(&self, f: Option<&File>, size: u64) -> Res<()> {
        if let Some(f) = f {
            return sys::ftruncate(f.as_fd(), size);
        }
        match self.fmt {
            S_IFREG => {}
            S_IFDIR => return Err(libc::EISDIR),
            _ => return Err(libc::EINVAL),
        }
        let f = sys::reopen(self.fd.as_fd(), libc::O_WRONLY)?;
        sys::ftruncate(f.as_fd(), size)
    }
}

impl Core {
    pub fn setattr(&self, ino: u64, a: &SetAttr, fh: Option<u64>) -> Res<FileAttr> {
        let r = self.real(ino, libc::EROFS)?;
        if r.ro {
            return Err(libc::EROFS);
        }
        let f = fh.and_then(|fh| self.file(fh).ok());
        // owner first: chown clears set-id bits, so an explicit mode must win
        if a.uid.is_some() || a.gid.is_some() {
            let keep = u32::MAX;
            sys::fchown(r.fd.as_fd(), a.uid.unwrap_or(keep), a.gid.unwrap_or(keep))?;
        }
        if let Some(mode) = a.mode {
            r.set_mode(f.as_deref(), mode)?;
        }
        if let Some(size) = a.size {
            r.truncate(f.as_deref(), size)?;
        }
        if a.atime.is_some() || a.mtime.is_some() {
            let times = [a.atime.as_ref(), a.mtime.as_ref()].map(to_timespec);
            sys::utimens(r.fd.as_fd(), &times)?;
        }
        Ok(stat_attr(ino, &sys::fstat(r.fd.as_fd())?))
    }

    pub fn readlink(&self, ino: u64) -> Res<Vec<u8>> {
        let r = self.real(ino, libc::EINVAL)?;
        if r.fmt != S_IFLNK {
            return Err(libc::EINVAL);
        }
        sys::readlink(r.fd.as_fd())
    }

    pub fn statfs(&self, ino: u64) -> Res<StatFs> {
        let NodeRef::Real(r) = self.get(ino)? else {
            return Ok(StatFs {
                blocks: 0,
                bfree: 0,
                bavail: 0,
                files: 0,
                ffree: 0,
                bsize: 4096,
                namelen: 255,
                frsize: 4096,
            });
        };
        let s = sys::fstatvfs(r.fd.as_fd())?;
        Ok(StatFs {
            blocks: s.f_blocks,
            bfree: s.f_bfree,
            bavail: s.f_bavail,
            files: s.f_files,
            ffree: s.f_ffree,
            bsize: s.f_bsize as u32,
            namelen: s.f_namemax as u32,
            frsize: s.f_frsize as u32,
        })
    }
}
