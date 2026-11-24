//! Open files and directory listings.

use super::attr::kind_of;
use super::{
    Core, DirEnt, DirState, Handle, Kind, NodeRef, ROOT_INO, RealRef, S_IFDIR, S_IFLNK, S_IFREG,
};
use crate::sys::{self, Res};
use fuser::FileType;
use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// Open flags that make no sense when re-opening an existing object (or
/// that we set ourselves).
pub(super) const OPEN_STRIP: i32 = libc::O_CREAT | libc::O_EXCL | libc::O_NOCTTY | libc::O_DIRECT;

fn sync(f: &File, datasync: bool) -> Res<()> {
    if datasync {
        f.sync_data()
    } else {
        f.sync_all()
    }
    .map_err(|e| sys::io_errno(&e))
}

fn dtype_kind(t: nix::dir::Type) -> FileType {
    use nix::dir::Type as T;
    match t {
        T::Directory => FileType::Directory,
        T::Symlink => FileType::Symlink,
        T::Fifo => FileType::NamedPipe,
        T::Socket => FileType::Socket,
        T::CharacterDevice => FileType::CharDevice,
        T::BlockDevice => FileType::BlockDevice,
        T::File => FileType::RegularFile,
    }
}

impl Core {
    pub(super) fn new_handle(&self, h: Handle) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        sys::lock(&self.handles).insert(fh, h);
        fh
    }

    fn handle(&self, fh: u64) -> Res<Handle> {
        sys::lock(&self.handles)
            .get(&fh)
            .cloned()
            .ok_or(libc::EBADF)
    }

    pub fn file(&self, fh: u64) -> Res<Arc<File>> {
        match self.handle(fh)? {
            Handle::File(f) => Ok(f),
            Handle::Dir(_) => Err(libc::EISDIR),
        }
    }

    pub fn release(&self, fh: u64) {
        sys::lock(&self.handles).remove(&fh);
    }

    pub fn open(&self, ino: u64, flags: i32) -> Res<u64> {
        let r = self.real(ino, libc::EISDIR)?;
        match r.fmt {
            S_IFREG => {}
            S_IFDIR => return Err(libc::EISDIR),
            S_IFLNK => return Err(libc::ELOOP),
            _ => return Err(libc::ENXIO),
        }
        let writes = flags & libc::O_ACCMODE != libc::O_RDONLY || flags & libc::O_TRUNC != 0;
        if r.ro && writes {
            return Err(libc::EROFS);
        }
        let f = sys::reopen(r.fd.as_fd(), flags & !(OPEN_STRIP | libc::O_NOFOLLOW))?;
        Ok(self.new_handle(Handle::File(Arc::new(File::from(f)))))
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> Res<Vec<u8>> {
        let f = self.file(fh)?;
        let mut buf = vec![0u8; size as usize];
        let mut n = 0;
        while n < buf.len() {
            match f.read_at(&mut buf[n..], offset + n as u64) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(sys::io_errno(&e)),
            }
        }
        buf.truncate(n);
        Ok(buf)
    }

    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Res<u32> {
        let f = self.file(fh)?;
        f.write_all_at(data, offset)
            .map_err(|e| sys::io_errno(&e))?;
        u32::try_from(data.len()).map_err(|_| libc::EINVAL)
    }

    pub fn fsync(&self, fh: u64, datasync: bool) -> Res<()> {
        match self.handle(fh)? {
            Handle::File(f) => sync(&f, datasync),
            // the kernel sends FSYNCDIR for directories
            Handle::Dir(_) => Ok(()),
        }
    }

    /// Make entries created/renamed/removed in a host directory durable.
    /// Virtual directories have nothing on disk.
    pub fn fsyncdir(&self, ino: u64, datasync: bool) -> Res<()> {
        let NodeRef::Real(r) = self.get(ino)? else {
            return Ok(());
        };
        let d = File::from(sys::reopen(
            r.fd.as_fd(),
            libc::O_RDONLY | libc::O_DIRECTORY,
        )?);
        sync(&d, datasync)
    }

    pub fn fallocate(&self, fh: u64, offset: u64, len: u64, mode: i32) -> Res<()> {
        sys::fallocate(self.file(fh)?.as_fd(), mode, offset, len)
    }

    pub fn lseek(&self, fh: u64, offset: i64, whence: i32) -> Res<i64> {
        sys::lseek(self.file(fh)?.as_fd(), offset, whence)
    }

    pub fn copy_file_range(
        &self,
        (fh_in, off_in): (u64, u64),
        (fh_out, off_out): (u64, u64),
        len: u64,
        flags: u32,
    ) -> Res<usize> {
        let fi = self.file(fh_in)?;
        let fo = self.file(fh_out)?;
        sys::copy_file_range(fi.as_fd(), off_in, fo.as_fd(), off_out, len, flags)
    }

    pub fn opendir(&self, ino: u64) -> Res<u64> {
        let src = match self.get(ino)? {
            NodeRef::Virt(_) => None,
            NodeRef::Real(r) if r.fmt == S_IFDIR => Some(r),
            NodeRef::Real(_) => return Err(libc::ENOTDIR),
        };
        Ok(self.new_handle(Handle::Dir(Arc::new(Mutex::new(DirState {
            ino,
            src,
            loaded: false,
            entries: Vec::new(),
        })))))
    }

    /// Calls `add(ino, next_offset, kind, name)` per entry until it returns
    /// true (reply buffer full). Snapshot is (re)taken at offset 0.
    pub fn readdir(
        &self,
        fh: u64,
        offset: u64,
        mut add: impl FnMut(u64, u64, FileType, &OsStr) -> bool,
    ) -> Res<()> {
        let Handle::Dir(m) = self.handle(fh)? else {
            return Err(libc::ENOTDIR);
        };
        let mut d = sys::lock(&m);
        if offset == 0 || !d.loaded {
            d.entries = match &d.src {
                None => self.virtual_entries(d.ino),
                Some(r) => self.host_entries(d.ino, r)?,
            };
            d.loaded = true;
        }
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        for (i, e) in d.entries.iter().enumerate().skip(start) {
            if add(e.ino, i as u64 + 1, e.kind, &e.name) {
                break;
            }
        }
        Ok(())
    }

    fn virtual_entries(&self, ino: u64) -> Vec<DirEnt> {
        let dot = |name: &str, ino| DirEnt {
            ino,
            kind: FileType::Directory,
            name: name.into(),
        };
        let s = self.st();
        let mut v = vec![dot(".", ino), dot("..", ROOT_INO)];
        let Some(vd) = s.vdir(ino) else { return v };
        v.extend(vd.children.iter().map(|(name, &c)| DirEnt {
            ino: c,
            kind: match s.nodes.get(&c).map(|n| &n.kind) {
                Some(Kind::Real(r)) => kind_of(r.fmt),
                _ => FileType::Directory,
            },
            name: name.clone(),
        }));
        v
    }

    fn host_entries(&self, ino: u64, r: &RealRef) -> Res<Vec<DirEnt>> {
        let dev = sys::fstat(r.fd.as_fd())?.st_dev;
        let dfd = sys::reopen(r.fd.as_fd(), libc::O_RDONLY | libc::O_DIRECTORY)?;
        let raw = dfd.as_raw_fd();
        let mut dir = nix::dir::Dir::from_fd(dfd).map_err(|e| e as i32)?;
        let mut v = Vec::new();
        for e in dir.iter() {
            let e = e.map_err(|e| e as i32)?;
            let name = e.file_name();
            let kind = if let Some(t) = e.file_type() {
                dtype_kind(t)
            } else {
                // SAFETY: `dir` owns the fd and outlives this borrow
                let dfd = unsafe { BorrowedFd::borrow_raw(raw) };
                kind_of(sys::fstatat_nofollow(dfd, name)?.st_mode)
            };
            v.push(DirEnt {
                ino: e.ino(),
                kind,
                name: OsStr::from_bytes(name.to_bytes()).to_os_string(),
            });
        }
        // report our inode numbers for entries the kernel already knows
        let s = self.st();
        for e in &mut v {
            if e.name == "." {
                e.ino = ino;
            } else if let Some(&known) = s.by_key.get(&(r.root, dev, e.ino)) {
                e.ino = known;
            }
        }
        Ok(v)
    }
}
