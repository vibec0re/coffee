//! fuser glue: translates FUSE requests into `Core` calls.

mod passthrough;

pub use passthrough::{Stats, StatsSnapshot};

use crate::core::{Caller, Core, SetAttr, TimeSpec};
use crate::sys::Res;
use fuser::{
    AccessFlags, CopyFileRangeFlags, Errno, FileAttr, FileHandle, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, Owner, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLseek,
    ReplyOpen, ReplyStatfs, ReplyStatx, ReplyWrite, ReplyXattr, Request, StatxMask, TimeOrNow,
    WriteFlags,
};
use passthrough::Passthrough;
use std::ffi::OsStr;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, SystemTime};

pub struct CoffeeFs {
    core: Arc<Core>,
    ttl: Duration,
    /// Try kernel passthrough (see [`passthrough`]).
    want_passthrough: bool,
    pt: Passthrough,
}

const GEN: Generation = Generation(0);

/// What every fuser reply type has in common.
trait Fail {
    fn fail(self, e: i32);
}

macro_rules! impl_fail {
    ($($t:ty),*) => {
        $(impl Fail for $t {
            fn fail(self, e: i32) {
                self.error(Errno::from_i32(e))
            }
        })*
    };
}

impl_fail!(
    ReplyAttr,
    ReplyCreate,
    ReplyData,
    ReplyDirectory,
    ReplyEmpty,
    ReplyEntry,
    ReplyLseek,
    ReplyOpen,
    ReplyStatfs,
    ReplyWrite,
    ReplyXattr
);

/// Answer with `ok(reply, value)`, or with the errno.
fn send<R: Fail, T>(reply: R, r: Res<T>, ok: impl FnOnce(R, T)) {
    match r {
        Ok(v) => ok(reply, v),
        Err(e) => reply.fail(e),
    }
}

fn empty(reply: ReplyEmpty, r: Res<()>) {
    send(reply, r, |r, ()| r.ok());
}

fn opened(reply: ReplyOpen, r: Res<u64>) {
    send(reply, r, |r, fh| {
        r.opened(FileHandle(fh), FopenFlags::empty())
    });
}

fn data(reply: ReplyData, r: Res<Vec<u8>>) {
    send(reply, r, |r, v| r.data(&v));
}

fn xattr(reply: ReplyXattr, r: Res<Vec<u8>>, size: u32) {
    send(reply, r, |reply, v| {
        if size == 0 {
            reply.size(v.len() as u32);
        } else if v.len() > size as usize {
            reply.fail(libc::ERANGE);
        } else {
            reply.data(&v);
        }
    });
}

fn caller(owner: Owner, umask: u32) -> Caller {
    Caller {
        uid: owner.uid,
        gid: owner.gid,
        umask,
    }
}

fn time(t: Option<TimeOrNow>) -> Option<TimeSpec> {
    t.map(|t| match t {
        TimeOrNow::Now => TimeSpec::Now,
        TimeOrNow::SpecificTime(t) => TimeSpec::At(t),
    })
}

impl CoffeeFs {
    pub fn new(core: Arc<Core>, ttl: Duration, passthrough: bool) -> Self {
        CoffeeFs {
            core,
            ttl,
            want_passthrough: passthrough,
            pt: Passthrough::default(),
        }
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.pt.stats.clone()
    }

    /// The backing id for a new open `fh` of `ino`, if it can use kernel
    /// passthrough; `open_backing` is the reply's registration call.
    fn backing(
        &self,
        ino: u64,
        fh: u64,
        open_backing: impl FnOnce(&File) -> std::io::Result<fuser::BackingId>,
    ) -> Option<Arc<fuser::BackingId>> {
        Stats::bump(&self.pt.stats.opens);
        self.pt.attach(ino, fh, || {
            let f = self
                .core
                .file(fh)
                .map_err(std::io::Error::from_raw_os_error)?;
            open_backing(&f)
        })
    }

    fn entry(&self, reply: ReplyEntry, r: Res<FileAttr>) {
        send(reply, r, |r, a| r.entry(&self.ttl, &a, GEN));
    }

    fn attr(&self, reply: ReplyAttr, r: Res<FileAttr>) {
        send(reply, r, |r, a| r.attr(&self.ttl, &a));
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
        if self.want_passthrough {
            if config.add_capabilities(InitFlags::FUSE_PASSTHROUGH).is_ok() {
                // 1: backing files must not be on a stacked fs (overlayfs),
                // but the container may stack an overlayfs on top of us
                let _ = config.set_max_stack_depth(1);
                self.pt.stats.passthrough.store(true, Relaxed);
                log::info!("kernel passthrough enabled");
            } else {
                log::info!("kernel has no FUSE passthrough; all I/O goes through the daemon");
            }
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.entry(reply, self.core.lookup(parent.0, name));
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.core.forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.attr(reply, self.core.getattr(ino.0));
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
        self.attr(reply, self.core.setattr(ino.0, &a, fh.map(|f| f.0)));
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        data(reply, self.core.readlink(ino.0));
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
        self.entry(
            reply,
            self.core.mknod(parent.0, name, mode, caller(owner, umask)),
        );
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
        self.entry(
            reply,
            self.core.mkdir(parent.0, name, mode, caller(owner, umask)),
        );
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        empty(reply, self.core.unlink(parent.0, name));
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        empty(reply, self.core.rmdir(parent.0, name));
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
        self.entry(
            reply,
            self.core
                .symlink(parent.0, link_name, target, caller(owner, 0)),
        );
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
        let r = self
            .core
            .rename((parent.0, name), (newparent.0, newname), flags.bits());
        empty(reply, r);
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        self.entry(reply, self.core.link(ino.0, newparent.0, newname));
    }

    fn open(
        &self,
        _req: &Request,
        ino: INodeNo,
        flags: OpenFlags,
        _kill_suid_gid: bool,
        reply: ReplyOpen,
    ) {
        let fh = match self.core.open(ino.0, flags.0) {
            Ok(fh) => fh,
            Err(e) => return reply.fail(e),
        };
        match self.backing(ino.0, fh, |f| reply.open_backing(f)) {
            Some(id) => reply.opened_passthrough(FileHandle(fh), FopenFlags::empty(), &id),
            None => reply.opened(FileHandle(fh), FopenFlags::empty()),
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
        Stats::bump(&self.pt.stats.daemon_reads);
        data(reply, self.core.read(fh.0, offset, size));
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
        Stats::bump(&self.pt.stats.daemon_writes);
        send(reply, self.core.write(fh.0, offset, data), |r, n| {
            r.written(n);
        });
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
        self.pt.release(fh.0);
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
        empty(reply, self.core.fsync(fh.0, datasync));
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        opened(reply, self.core.opendir(ino.0));
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
        send(reply, r, |r, ()| r.ok());
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
        ino: INodeNo,
        _fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        empty(reply, self.core.fsyncdir(ino.0, datasync));
    }

    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        send(reply, self.core.statfs(ino.0), |r, s| {
            r.statfs(
                s.blocks, s.bfree, s.bavail, s.files, s.ffree, s.bsize, s.namelen, s.frsize,
            );
        });
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
        empty(reply, self.core.setxattr(ino.0, name, value, flags));
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        xattr(reply, self.core.getxattr(ino.0, name), size);
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        xattr(reply, self.core.listxattr(ino.0), size);
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        empty(reply, self.core.removexattr(ino.0, name));
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
        let (a, fh) = match self
            .core
            .create(parent.0, name, mode, flags, caller(owner, umask))
        {
            Ok(r) => r,
            Err(e) => return reply.fail(e),
        };
        let (ttl, fh_, none) = (&self.ttl, FileHandle(fh), FopenFlags::empty());
        match self.backing(a.ino.0, fh, |f| reply.open_backing(f)) {
            Some(id) => reply.created_passthrough(ttl, &a, GEN, fh_, none, &id),
            None => reply.created(ttl, &a, GEN, fh_, none),
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
        empty(reply, self.core.fallocate(fh.0, offset, length, mode));
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
        send(reply, self.core.lseek(fh.0, offset, whence), |r, o| {
            r.offset(o);
        });
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
        // the reply carries a u32 count
        let len = len.min(u64::from(u32::MAX));
        let r = self.core.copy_file_range(
            (fh_in.0, offset_in),
            (fh_out.0, offset_out),
            len,
            flags.bits() as u32,
        );
        send(reply, r, |r, n| r.written(n as u32));
    }
}
