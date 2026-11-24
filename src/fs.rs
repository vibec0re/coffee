//! fuser glue: translates FUSE requests into `Core` calls.

use crate::core::{Core, SetAttr, TimeSpec};
use fuser::{
    AccessFlags, CopyFileRangeFlags, Errno, FileHandle, Filesystem, FopenFlags, Generation,
    INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, Owner, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLseek, ReplyOpen,
    ReplyStatfs, ReplyStatx, ReplyWrite, ReplyXattr, Request, StatxMask, TimeOrNow, WriteFlags,
};
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub struct CoffeeFs {
    pub core: Arc<Core>,
    pub ttl: Duration,
}

const GEN: Generation = Generation(0);

fn err(e: i32) -> Errno {
    Errno::from_i32(e)
}

fn time(t: Option<TimeOrNow>) -> Option<TimeSpec> {
    t.map(|t| match t {
        TimeOrNow::Now => TimeSpec::Now,
        TimeOrNow::SpecificTime(t) => TimeSpec::At(t),
    })
}

fn xattr_reply(r: Result<Vec<u8>, i32>, size: u32, reply: ReplyXattr) {
    match r {
        Ok(v) if size == 0 => reply.size(v.len() as u32),
        Ok(v) if v.len() > size as usize => reply.error(Errno::ERANGE),
        Ok(v) => reply.data(&v),
        Err(e) => reply.error(err(e)),
    }
}

impl Filesystem for CoffeeFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        for cap in [
            // drop cached pages when the host changes a file under us
            InitFlags::FUSE_AUTO_INVAL_DATA,
            InitFlags::FUSE_ATOMIC_O_TRUNC,
            InitFlags::FUSE_PARALLEL_DIROPS,
        ] {
            if let Err(e) = config.add_capabilities(cap) {
                log::debug!("kernel lacks capability {e:?}");
            }
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.core.lookup(parent.0, name) {
            Ok(a) => reply.entry(&self.ttl, &a, GEN),
            Err(e) => reply.error(err(e)),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.core.forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.core.getattr(ino.0) {
            Ok(a) => reply.attr(&self.ttl, &a),
            Err(e) => reply.error(err(e)),
        }
    }

    fn statx(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: Option<FileHandle>,
        _flags: u32,
        _mask: StatxMask,
        reply: ReplyStatx,
    ) {
        // kernel falls back to GETATTR
        reply.error(Errno::ENOSYS);
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        _kill_suid_gid: bool,
        reply: ReplyAttr,
    ) {
        let a = SetAttr {
            mode,
            uid,
            gid,
            size,
            atime: time(atime),
            mtime: time(mtime),
        };
        match self.core.setattr(ino.0, a, fh.map(|f| f.0)) {
            Ok(a) => reply.attr(&self.ttl, &a),
            Err(e) => reply.error(err(e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.core.readlink(ino.0) {
            Ok(v) => reply.data(&v),
            Err(e) => reply.error(err(e)),
        }
    }

    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        owner: Owner,
        reply: ReplyEntry,
    ) {
        match self.core.mknod(parent.0, name, mode, umask, owner) {
            Ok(a) => reply.entry(&self.ttl, &a, GEN),
            Err(e) => reply.error(err(e)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        owner: Owner,
        reply: ReplyEntry,
    ) {
        match self.core.mkdir(parent.0, name, mode, umask, owner) {
            Ok(a) => reply.entry(&self.ttl, &a, GEN),
            Err(e) => reply.error(err(e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.core.unlink(parent.0, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.core.rmdir(parent.0, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        owner: Owner,
        reply: ReplyEntry,
    ) {
        match self.core.symlink(parent.0, link_name, target, owner) {
            Ok(a) => reply.entry(&self.ttl, &a, GEN),
            Err(e) => reply.error(err(e)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        _owner: Option<Owner>,
        reply: ReplyEmpty,
    ) {
        match self
            .core
            .rename(parent.0, name, newparent.0, newname, flags.bits())
        {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        match self.core.link(ino.0, newparent.0, newname) {
            Ok(a) => reply.entry(&self.ttl, &a, GEN),
            Err(e) => reply.error(err(e)),
        }
    }

    fn open(
        &self,
        _req: &Request,
        ino: INodeNo,
        flags: OpenFlags,
        _kill_suid_gid: bool,
        reply: ReplyOpen,
    ) {
        match self.core.open(ino.0, flags.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(err(e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.core.read(fh.0, offset, size) {
            Ok(v) => reply.data(&v),
            Err(e) => reply.error(err(e)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.core.write(fh.0, offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(err(e)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.core.release(fh.0);
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.core.fsync(fh.0, datasync) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.core.opendir(ino.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(err(e)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let r = self.core.readdir(fh.0, offset, |ino, off, kind, name| {
            reply.add(INodeNo(ino), off, kind, name)
        });
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.core.release(fh.0);
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        match self.core.statfs(ino.0) {
            Ok(s) => reply.statfs(
                s.blocks, s.bfree, s.bavail, s.files, s.ffree, s.bsize, s.namelen, s.frsize,
            ),
            Err(e) => reply.error(err(e)),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        match self.core.setxattr(ino.0, name, value, flags) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        xattr_reply(self.core.getxattr(ino.0, name), size, reply);
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        xattr_reply(self.core.listxattr(ino.0), size, reply);
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.core.removexattr(ino.0, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        // only reached without default_permissions; the kernel checks modes
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        _kill_suid_gid: bool,
        owner: Owner,
        reply: ReplyCreate,
    ) {
        match self.core.create(parent.0, name, mode, umask, flags, owner) {
            Ok((a, fh)) => reply.created(&self.ttl, &a, GEN, FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(err(e)),
        }
    }

    fn fallocate(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        match self.core.fallocate(fh.0, offset, length, mode) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn lseek(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        match self.core.lseek(fh.0, offset, whence) {
            Ok(o) => reply.offset(o),
            Err(e) => reply.error(err(e)),
        }
    }

    fn copy_file_range(
        &self,
        _req: &Request,
        _ino_in: INodeNo,
        fh_in: FileHandle,
        offset_in: u64,
        _ino_out: INodeNo,
        fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        let len = len.min(u32::MAX as u64);
        match self.core.copy_file_range(
            fh_in.0,
            offset_in,
            fh_out.0,
            offset_out,
            len,
            flags.bits() as u32,
        ) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(err(e)),
        }
    }
}
