//! The filesystem model, independent of FUSE plumbing.
//!
//! The tree has two kinds of nodes:
//!
//! * **virtual directories**: the read-only skeleton (`/`, `/work`, ...). They
//!   exist only in memory and are created/removed through the control socket.
//! * **real nodes**: passthrough objects on the host. Each *link* (added at
//!   runtime via the control socket) is a real node hanging off a virtual
//!   directory; everything below a linked directory is passthrough.
//!
//! Every real node holds an `O_PATH` fd to its host object. Children are
//! resolved strictly one component at a time with `openat(O_NOFOLLOW)`, so a
//! symlink planted inside a shared directory can never make the (root-running)
//! daemon touch anything outside that directory: symlinks are returned to the
//! kernel as symlinks and resolved in the *container's* view.
//!
//! Each link gets its own "root id". Inodes are keyed by `(root id, dev, ino)`,
//! so the same host file reached via two links is two distinct inodes, and
//! `rename`/`link` across links fail with `EXDEV` (like separate bind mounts).
//! That keeps a read-only link from being written through a read-write one.

use crate::sys::{self, Res};
use fuser::{FileAttr, FileType, INodeNo, Owner};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, OsStr, OsString};
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ROOT_INO: u64 = 1;

const S_IFMT: u32 = libc::S_IFMT;
const S_IFDIR: u32 = libc::S_IFDIR;
const S_IFREG: u32 = libc::S_IFREG;
const S_IFLNK: u32 = libc::S_IFLNK;

/// A kernel cache entry that must be dropped after a control-plane change.
/// Sent by the caller *after* the core lock is released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inval {
    Entry { parent: u64, name: OsString },
    Inode(u64),
}

/// One entry of the control-plane view; also the on-disk state format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ListEntry {
    Link {
        target: PathBuf,
        source: PathBuf,
        #[serde(default)]
        ro: bool,
    },
    Dir {
        target: PathBuf,
    },
}

pub enum TimeSpec {
    Now,
    At(SystemTime),
}

#[derive(Default)]
pub struct SetAttr {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<TimeSpec>,
    pub mtime: Option<TimeSpec>,
}

pub struct StatFs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
}

struct Node {
    /// Kernel references (lookups minus forgets).
    nlookup: u64,
    /// Referenced by the virtual tree.
    pinned: bool,
    kind: Kind,
}

enum Kind {
    Virt(VDir),
    Real(Real),
}

struct VDir {
    children: BTreeMap<OsString, u64>,
    /// Created only as a parent of something else; pruned once empty.
    implicit: bool,
    mtime: SystemTime,
}

struct Real {
    fd: Arc<OwnedFd>,
    root: u64,
    ro: bool,
    fmt: u32,
    /// `(dev, ino)` on the host; `None` for link roots (not deduplicated).
    key: Option<(u64, u64)>,
    /// Host path for link roots.
    source: Option<PathBuf>,
}

impl Node {
    fn is_dir(&self) -> bool {
        match &self.kind {
            Kind::Virt(_) => true,
            Kind::Real(r) => r.fmt == S_IFDIR,
        }
    }
}

#[derive(Clone)]
struct RealRef {
    fd: Arc<OwnedFd>,
    root: u64,
    ro: bool,
    fmt: u32,
}

enum NodeRef {
    Virt(FileAttr),
    Real(RealRef),
}

struct State {
    nodes: HashMap<u64, Node>,
    by_key: HashMap<(u64, u64, u64), u64>,
    next_ino: u64,
    next_root: u64,
}

impl State {
    fn alloc_ino(&mut self) -> u64 {
        let i = self.next_ino;
        self.next_ino += 1;
        i
    }

    fn vdir(&self, ino: u64) -> Option<&VDir> {
        match &self.nodes.get(&ino)?.kind {
            Kind::Virt(v) => Some(v),
            Kind::Real(_) => None,
        }
    }

    fn vdir_mut(&mut self, ino: u64) -> Option<&mut VDir> {
        match &mut self.nodes.get_mut(&ino)?.kind {
            Kind::Virt(v) => Some(v),
            Kind::Real(_) => None,
        }
    }

    fn child(&self, parent: u64, name: &OsStr) -> Option<u64> {
        self.vdir(parent)?.children.get(name).copied()
    }

    fn maybe_free(&mut self, ino: u64) {
        if ino == ROOT_INO {
            return;
        }
        let free = matches!(self.nodes.get(&ino), Some(n) if n.nlookup == 0 && !n.pinned);
        if !free {
            return;
        }
        if let Some(Node {
            kind: Kind::Real(r),
            ..
        }) = self.nodes.remove(&ino)
            && let Some((dev, hino)) = r.key
        {
            let k = (r.root, dev, hino);
            if self.by_key.get(&k) == Some(&ino) {
                self.by_key.remove(&k);
            }
        }
    }

    /// Unhook `ino` (and, for virtual dirs, everything below) from the tree.
    /// Nodes the kernel still references stay alive until forgotten, so open
    /// files and cwds inside a removed link keep working.
    fn detach(&mut self, ino: u64) {
        let children = match self.nodes.get_mut(&ino) {
            Some(n) => {
                n.pinned = false;
                match &mut n.kind {
                    Kind::Virt(v) => std::mem::take(&mut v.children),
                    Kind::Real(_) => BTreeMap::new(),
                }
            }
            None => return,
        };
        for c in children.into_values() {
            self.detach(c);
        }
        self.maybe_free(ino);
    }

    fn touch(&mut self, ino: u64) {
        if let Some(v) = self.vdir_mut(ino) {
            v.mtime = SystemTime::now();
        }
    }
}

enum Handle {
    File(File),
    Dir(Mutex<DirState>),
}

struct DirState {
    ino: u64,
    src: Option<RealRef>,
    loaded: bool,
    entries: Vec<DirEnt>,
}

struct DirEnt {
    ino: u64,
    kind: FileType,
    name: OsString,
}

pub struct Core {
    st: Mutex<State>,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    next_fh: AtomicU64,
    /// Owner reported for virtual directories.
    owner: Owner,
    /// Create objects as the requesting user (needs root): see sys::AsCaller.
    switch_creds: bool,
    born: SystemTime,
}

fn ts(sec: i64, nsec: i64) -> SystemTime {
    if sec >= 0 {
        UNIX_EPOCH + Duration::new(sec as u64, nsec as u32)
    } else {
        UNIX_EPOCH - Duration::from_secs(sec.unsigned_abs()) + Duration::from_nanos(nsec as u64)
    }
}

fn to_timespec(t: &Option<TimeSpec>) -> libc::timespec {
    match t {
        None => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        Some(TimeSpec::Now) => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
        Some(TimeSpec::At(t)) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => libc::timespec {
                tv_sec: d.as_secs() as libc::time_t,
                tv_nsec: d.subsec_nanos() as libc::c_long,
            },
            Err(e) => {
                let d = e.duration();
                let mut sec = -(d.as_secs() as i64);
                let mut nsec = -(d.subsec_nanos() as i64);
                if nsec < 0 {
                    sec -= 1;
                    nsec += 1_000_000_000;
                }
                libc::timespec {
                    tv_sec: sec as libc::time_t,
                    tv_nsec: nsec as libc::c_long,
                }
            }
        },
    }
}

pub fn kind_of(mode: u32) -> FileType {
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

pub fn stat_attr(ino: u64, st: &libc::stat) -> FileAttr {
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
        rdev: st.st_rdev as u32,
        blksize: st.st_blksize as u32,
        flags: 0,
    }
}

/// Split an absolute in-filesystem path into components; rejects `..`.
pub fn parse_target(p: &Path) -> Result<Vec<OsString>, String> {
    if !p.is_absolute() {
        return Err(format!("{}: target must be an absolute path", p.display()));
    }
    let mut v = Vec::new();
    for c in p.components() {
        match c {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(n) => v.push(n.to_os_string()),
            Component::ParentDir => {
                return Err(format!("{}: '..' is not allowed", p.display()));
            }
            Component::Prefix(_) => return Err(format!("{}: invalid path", p.display())),
        }
    }
    Ok(v)
}

fn join(comps: &[OsString]) -> PathBuf {
    let mut p = PathBuf::from("/");
    p.extend(comps);
    p
}

impl Core {
    pub fn new(owner: Owner, switch_creds: bool) -> Core {
        let now = SystemTime::now();
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT_INO,
            Node {
                nlookup: 1,
                pinned: true,
                kind: Kind::Virt(VDir {
                    children: BTreeMap::new(),
                    implicit: false,
                    mtime: now,
                }),
            },
        );
        Core {
            st: Mutex::new(State {
                nodes,
                by_key: HashMap::new(),
                next_ino: ROOT_INO + 1,
                next_root: 1,
            }),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            owner,
            switch_creds,
            born: now,
        }
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn vattr(&self, s: &State, ino: u64, v: &VDir) -> FileAttr {
        let subdirs = v
            .children
            .values()
            .filter(|c| s.nodes.get(c).is_some_and(Node::is_dir))
            .count();
        FileAttr {
            ino: INodeNo(ino),
            size: 0,
            blocks: 0,
            atime: v.mtime,
            mtime: v.mtime,
            ctime: v.mtime,
            crtime: self.born,
            kind: FileType::Directory,
            perm: 0o555,
            nlink: 2 + subdirs as u32,
            uid: self.owner.uid,
            gid: self.owner.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn get(&self, ino: u64) -> Res<NodeRef> {
        let s = self.st();
        let n = s.nodes.get(&ino).ok_or(libc::ESTALE)?;
        Ok(match &n.kind {
            Kind::Virt(v) => NodeRef::Virt(self.vattr(&s, ino, v)),
            Kind::Real(r) => NodeRef::Real(RealRef {
                fd: r.fd.clone(),
                root: r.root,
                ro: r.ro,
                fmt: r.fmt,
            }),
        })
    }

    fn real(&self, ino: u64, virt_err: i32) -> Res<RealRef> {
        match self.get(ino)? {
            NodeRef::Virt(_) => Err(virt_err),
            NodeRef::Real(r) => Ok(r),
        }
    }

    /// A directory we may create/remove entries in. The virtual skeleton is
    /// managed only via the control socket, so it reads as a read-only fs.
    fn wdir(&self, ino: u64) -> Res<RealRef> {
        let r = self.real(ino, libc::EROFS)?;
        if r.fmt != S_IFDIR {
            return Err(libc::ENOTDIR);
        }
        if r.ro {
            return Err(libc::EROFS);
        }
        Ok(r)
    }

    /// Enter a host object (O_PATH fd) into the inode table, or bump the
    /// existing entry for it. Counts as one kernel lookup.
    fn register(&self, fd: OwnedFd, root: u64, ro: bool) -> Res<FileAttr> {
        let st = sys::fstat(fd.as_fd())?;
        let key = (root, st.st_dev, st.st_ino);
        let mut s = self.st();
        if let Some(&ino) = s.by_key.get(&key)
            && let Some(n) = s.nodes.get_mut(&ino)
        {
            n.nlookup += 1;
            return Ok(stat_attr(ino, &st));
        }
        let ino = s.alloc_ino();
        s.nodes.insert(
            ino,
            Node {
                nlookup: 1,
                pinned: false,
                kind: Kind::Real(Real {
                    fd: Arc::new(fd),
                    root,
                    ro,
                    fmt: st.st_mode & S_IFMT,
                    key: Some((key.1, key.2)),
                    source: None,
                }),
            },
        );
        s.by_key.insert(key, ino);
        Ok(stat_attr(ino, &st))
    }

    // ---------------------------------------------------------------- lookup

    pub fn lookup(&self, parent: u64, name: &OsStr) -> Res<FileAttr> {
        let cname = sys::name(name)?;
        enum Step {
            Done(FileAttr),
            LinkRoot(u64, Arc<OwnedFd>),
            Real(RealRef),
        }
        let step = {
            let mut s = self.st();
            let node = s.nodes.get(&parent).ok_or(libc::ESTALE)?;
            match &node.kind {
                Kind::Real(r) => Step::Real(RealRef {
                    fd: r.fd.clone(),
                    root: r.root,
                    ro: r.ro,
                    fmt: r.fmt,
                }),
                Kind::Virt(v) => {
                    let child = *v.children.get(name).ok_or(libc::ENOENT)?;
                    let c = s.nodes.get_mut(&child).ok_or(libc::ENOENT)?;
                    // Take the reference now so a concurrent `rm` can't free
                    // the node between here and the reply.
                    c.nlookup += 1;
                    match &c.kind {
                        Kind::Real(r) => Step::LinkRoot(child, r.fd.clone()),
                        Kind::Virt(_) => {
                            let s = &*s;
                            let Some(Kind::Virt(cv)) = s.nodes.get(&child).map(|n| &n.kind) else {
                                unreachable!()
                            };
                            Step::Done(self.vattr(s, child, cv))
                        }
                    }
                }
            }
        };
        match step {
            Step::Done(a) => Ok(a),
            Step::LinkRoot(ino, fd) => match sys::fstat(fd.as_fd()) {
                Ok(st) => Ok(stat_attr(ino, &st)),
                Err(e) => {
                    self.forget(ino, 1);
                    Err(e)
                }
            },
            Step::Real(p) => {
                if p.fmt != S_IFDIR {
                    return Err(libc::ENOTDIR);
                }
                let fd = sys::openat(p.fd.as_fd(), &cname, libc::O_PATH | libc::O_NOFOLLOW, 0)?;
                self.register(fd, p.root, p.ro)
            }
        }
    }

    pub fn forget(&self, ino: u64, n: u64) {
        let mut s = self.st();
        if let Some(node) = s.nodes.get_mut(&ino) {
            node.nlookup = node.nlookup.saturating_sub(n);
            s.maybe_free(ino);
        }
    }

    // ------------------------------------------------------------ attributes

    pub fn getattr(&self, ino: u64) -> Res<FileAttr> {
        match self.get(ino)? {
            NodeRef::Virt(a) => Ok(a),
            NodeRef::Real(r) => Ok(stat_attr(ino, &sys::fstat(r.fd.as_fd())?)),
        }
    }

    pub fn setattr(&self, ino: u64, a: SetAttr, fh: Option<u64>) -> Res<FileAttr> {
        let r = self.real(ino, libc::EROFS)?;
        if r.ro {
            return Err(libc::EROFS);
        }
        let h = fh.and_then(|fh| self.handle(fh).ok());
        let hfile = match h.as_deref() {
            Some(Handle::File(f)) => Some(f),
            _ => None,
        };
        // owner first: chown clears set-id bits, so an explicit mode must win
        if a.uid.is_some() || a.gid.is_some() {
            sys::fchown(
                r.fd.as_fd(),
                a.uid.unwrap_or(u32::MAX),
                a.gid.unwrap_or(u32::MAX),
            )?;
        }
        if let Some(mode) = a.mode {
            let mode = mode & 0o7777;
            if let Some(f) = hfile {
                sys::fchmod(f.as_fd(), mode)?;
            } else if r.fmt == S_IFLNK {
                return Err(libc::EOPNOTSUPP);
            } else {
                sys::chmod(&sys::proc_path(r.fd.as_fd()), mode)?;
            }
        }
        if let Some(size) = a.size {
            if let Some(f) = hfile {
                sys::ftruncate(f.as_fd(), size)?;
            } else {
                match r.fmt {
                    S_IFREG => {}
                    S_IFDIR => return Err(libc::EISDIR),
                    _ => return Err(libc::EINVAL),
                }
                let f = sys::reopen(r.fd.as_fd(), libc::O_WRONLY)?;
                sys::ftruncate(f.as_fd(), size)?;
            }
        }
        if a.atime.is_some() || a.mtime.is_some() {
            sys::utimens(
                r.fd.as_fd(),
                &[to_timespec(&a.atime), to_timespec(&a.mtime)],
            )?;
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

    // -------------------------------------------------------- namespace ops

    /// Switch this thread to the requester for the creating syscall.
    fn as_caller(&self, owner: Owner) -> Res<sys::AsCaller> {
        sys::AsCaller::enter(owner.uid, owner.gid, self.switch_creds)
    }

    /// Pick up an object we just created. Resolving the name again can race
    /// with a rename in the host directory, but then we merely report what
    /// is there now, exactly as a lookup would; nothing is changed on it.
    fn finish_new(&self, p: &RealRef, name: &CStr) -> Res<FileAttr> {
        let fd = sys::openat(p.fd.as_fd(), name, libc::O_PATH | libc::O_NOFOLLOW, 0)?;
        self.register(fd, p.root, p.ro)
    }

    pub fn mkdir(
        &self,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        owner: Owner,
    ) -> Res<FileAttr> {
        let p = self.wdir(parent)?;
        let c = sys::name(name)?;
        let m = mode & !umask & 0o7777;
        {
            let _id = self.as_caller(owner)?;
            sys::mkdirat(p.fd.as_fd(), &c, m)?;
        }
        self.finish_new(&p, &c)
    }

    pub fn mknod(
        &self,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        owner: Owner,
    ) -> Res<FileAttr> {
        let fmt = mode & S_IFMT;
        // no device nodes on the host on behalf of a container
        if !matches!(fmt, libc::S_IFREG | libc::S_IFIFO | libc::S_IFSOCK) {
            return Err(libc::EPERM);
        }
        let p = self.wdir(parent)?;
        let c = sys::name(name)?;
        let m = mode & !umask & 0o7777;
        {
            let _id = self.as_caller(owner)?;
            sys::mknodat(p.fd.as_fd(), &c, fmt | m)?;
        }
        self.finish_new(&p, &c)
    }

    pub fn symlink(&self, parent: u64, name: &OsStr, target: &Path, owner: Owner) -> Res<FileAttr> {
        let p = self.wdir(parent)?;
        let c = sys::name(name)?;
        let t = sys::cstring(target.as_os_str())?;
        {
            let _id = self.as_caller(owner)?;
            sys::symlinkat(&t, p.fd.as_fd(), &c)?;
        }
        self.finish_new(&p, &c)
    }

    pub fn unlink(&self, parent: u64, name: &OsStr) -> Res<()> {
        let p = self.wdir(parent)?;
        sys::unlinkat(p.fd.as_fd(), &sys::name(name)?, 0)
    }

    pub fn rmdir(&self, parent: u64, name: &OsStr) -> Res<()> {
        let p = self.wdir(parent)?;
        sys::unlinkat(p.fd.as_fd(), &sys::name(name)?, libc::AT_REMOVEDIR)
    }

    pub fn rename(
        &self,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
    ) -> Res<()> {
        if flags & libc::RENAME_WHITEOUT != 0 {
            return Err(libc::EINVAL);
        }
        let p = self.wdir(parent)?;
        let np = self.wdir(newparent)?;
        if p.root != np.root {
            return Err(libc::EXDEV);
        }
        sys::renameat2(
            p.fd.as_fd(),
            &sys::name(name)?,
            np.fd.as_fd(),
            &sys::name(newname)?,
            flags,
        )
    }

    pub fn link(&self, ino: u64, newparent: u64, newname: &OsStr) -> Res<FileAttr> {
        let src = self.real(ino, libc::EPERM)?;
        let np = self.wdir(newparent)?;
        if src.root != np.root {
            return Err(libc::EXDEV);
        }
        if src.fmt == S_IFDIR {
            return Err(libc::EPERM);
        }
        let c = sys::name(newname)?;
        sys::link_fd(src.fd.as_fd(), np.fd.as_fd(), &c)?;
        let fd = sys::openat(np.fd.as_fd(), &c, libc::O_PATH | libc::O_NOFOLLOW, 0)?;
        self.register(fd, np.root, np.ro)
    }

    // --------------------------------------------------------------- files

    fn new_handle(&self, h: Handle) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fh, Arc::new(h));
        fh
    }

    fn handle(&self, fh: u64) -> Res<Arc<Handle>> {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&fh)
            .cloned()
            .ok_or(libc::EBADF)
    }

    fn file(&self, fh: u64) -> Res<Arc<Handle>> {
        let h = self.handle(fh)?;
        match &*h {
            Handle::File(_) => Ok(h),
            Handle::Dir(_) => Err(libc::EISDIR),
        }
    }

    pub fn release(&self, fh: u64) {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&fh);
    }

    const OPEN_STRIP: i32 = libc::O_CREAT | libc::O_EXCL | libc::O_NOCTTY | libc::O_DIRECT;

    pub fn open(&self, ino: u64, flags: i32) -> Res<u64> {
        let r = self.real(ino, libc::EISDIR)?;
        match r.fmt {
            S_IFREG => {}
            S_IFDIR => return Err(libc::EISDIR),
            S_IFLNK => return Err(libc::ELOOP),
            _ => return Err(libc::ENXIO),
        }
        if r.ro && (flags & libc::O_ACCMODE != libc::O_RDONLY || flags & libc::O_TRUNC != 0) {
            return Err(libc::EROFS);
        }
        let f = sys::reopen(r.fd.as_fd(), flags & !(Self::OPEN_STRIP | libc::O_NOFOLLOW))?;
        Ok(self.new_handle(Handle::File(File::from(f))))
    }

    pub fn create(
        &self,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        owner: Owner,
    ) -> Res<(FileAttr, u64)> {
        let p = self.wdir(parent)?;
        let c = sys::name(name)?;
        let m = mode & !umask & 0o7777;
        let base = (flags & !Self::OPEN_STRIP) | libc::O_NOFOLLOW;
        let file = {
            let _id = self.as_caller(owner)?;
            match sys::openat(p.fd.as_fd(), &c, base | libc::O_CREAT | libc::O_EXCL, m) {
                Err(libc::EEXIST) if flags & libc::O_EXCL == 0 => {
                    sys::openat(p.fd.as_fd(), &c, base, 0)?
                }
                r => r?,
            }
        };
        let pathfd = sys::reopen(file.as_fd(), libc::O_PATH)?;
        let attr = self.register(pathfd, p.root, p.ro)?;
        let fh = self.new_handle(Handle::File(File::from(file)));
        Ok((attr, fh))
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> Res<Vec<u8>> {
        let h = self.file(fh)?;
        let Handle::File(f) = &*h else { unreachable!() };
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
        let h = self.file(fh)?;
        let Handle::File(f) = &*h else { unreachable!() };
        f.write_all_at(data, offset)
            .map_err(|e| sys::io_errno(&e))?;
        Ok(data.len() as u32)
    }

    pub fn fsync(&self, fh: u64, datasync: bool) -> Res<()> {
        let h = self.handle(fh)?;
        let r = match &*h {
            Handle::File(f) if datasync => f.sync_data(),
            Handle::File(f) => f.sync_all(),
            Handle::Dir(_) => return Ok(()),
        };
        r.map_err(|e| sys::io_errno(&e))
    }

    pub fn fallocate(&self, fh: u64, offset: u64, len: u64, mode: i32) -> Res<()> {
        let h = self.file(fh)?;
        let Handle::File(f) = &*h else { unreachable!() };
        sys::fallocate(f.as_fd(), mode, offset, len)
    }

    pub fn lseek(&self, fh: u64, offset: i64, whence: i32) -> Res<i64> {
        let h = self.file(fh)?;
        let Handle::File(f) = &*h else { unreachable!() };
        sys::lseek(f.as_fd(), offset, whence)
    }

    pub fn copy_file_range(
        &self,
        fh_in: u64,
        off_in: u64,
        fh_out: u64,
        off_out: u64,
        len: u64,
        flags: u32,
    ) -> Res<usize> {
        let hi = self.file(fh_in)?;
        let ho = self.file(fh_out)?;
        let (Handle::File(fi), Handle::File(fo)) = (&*hi, &*ho) else {
            unreachable!()
        };
        sys::copy_file_range(fi.as_fd(), off_in, fo.as_fd(), off_out, len, flags)
    }

    // ---------------------------------------------------------- directories

    pub fn opendir(&self, ino: u64) -> Res<u64> {
        let src = match self.get(ino)? {
            NodeRef::Virt(_) => None,
            NodeRef::Real(r) if r.fmt == S_IFDIR => Some(r),
            NodeRef::Real(_) => return Err(libc::ENOTDIR),
        };
        Ok(self.new_handle(Handle::Dir(Mutex::new(DirState {
            ino,
            src,
            loaded: false,
            entries: Vec::new(),
        }))))
    }

    /// Calls `add(ino, next_offset, kind, name)` per entry until it returns
    /// true (reply buffer full). Snapshot is (re)taken at offset 0.
    pub fn readdir(
        &self,
        fh: u64,
        offset: u64,
        mut add: impl FnMut(u64, u64, FileType, &OsStr) -> bool,
    ) -> Res<()> {
        let h = self.handle(fh)?;
        let Handle::Dir(m) = &*h else {
            return Err(libc::ENOTDIR);
        };
        let mut d = m.lock().unwrap_or_else(|e| e.into_inner());
        if offset == 0 || !d.loaded {
            d.entries = self.snapshot(d.ino, d.src.as_ref())?;
            d.loaded = true;
        }
        for (i, e) in d.entries.iter().enumerate().skip(offset as usize) {
            if add(e.ino, i as u64 + 1, e.kind, &e.name) {
                break;
            }
        }
        Ok(())
    }

    fn snapshot(&self, ino: u64, src: Option<&RealRef>) -> Res<Vec<DirEnt>> {
        let dot = |name: &str, ino| DirEnt {
            ino,
            kind: FileType::Directory,
            name: name.into(),
        };
        let Some(r) = src else {
            let s = self.st();
            let mut v = vec![dot(".", ino), dot("..", ROOT_INO)];
            if let Some(vd) = s.vdir(ino) {
                for (name, &c) in &vd.children {
                    let kind = match s.nodes.get(&c).map(|n| &n.kind) {
                        Some(Kind::Real(r)) => kind_of(r.fmt),
                        _ => FileType::Directory,
                    };
                    v.push(DirEnt {
                        ino: c,
                        kind,
                        name: name.clone(),
                    });
                }
            }
            return Ok(v);
        };
        let dev = sys::fstat(r.fd.as_fd())?.st_dev;
        let dfd = sys::reopen(r.fd.as_fd(), libc::O_RDONLY | libc::O_DIRECTORY)?;
        let raw = std::os::fd::AsRawFd::as_raw_fd(&dfd);
        let mut dir = nix::dir::Dir::from_fd(dfd).map_err(|e| e as i32)?;
        let mut v = Vec::new();
        for e in dir.iter() {
            let e = e.map_err(|e| e as i32)?;
            let name = e.file_name();
            let kind = match e.file_type() {
                Some(t) => dtype_kind(t),
                None => {
                    let st = sys::fstatat_nofollow(
                        unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) },
                        name,
                    )?;
                    kind_of(st.st_mode)
                }
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

    // ---------------------------------------------------------------- misc

    pub fn statfs(&self, ino: u64) -> Res<StatFs> {
        match self.get(ino)? {
            NodeRef::Virt(_) => Ok(StatFs {
                blocks: 0,
                bfree: 0,
                bavail: 0,
                files: 0,
                ffree: 0,
                bsize: 4096,
                namelen: 255,
                frsize: 4096,
            }),
            NodeRef::Real(r) => {
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
    }

    // Only the `user.` xattr namespace is passed through. The daemon runs as
    // root; letting a container read/write trusted.*/security.* (file caps,
    // overlayfs metadata, ...) on host files would be a privilege leak.

    fn xattr_target(&self, ino: u64, name: Option<&OsStr>, write: bool) -> Res<Option<RealRef>> {
        if let Some(n) = name
            && !n.as_bytes().starts_with(b"user.")
        {
            return Err(if write { libc::EPERM } else { libc::ENODATA });
        }
        let r = match self.get(ino)? {
            NodeRef::Virt(_) => return if write { Err(libc::EROFS) } else { Ok(None) },
            NodeRef::Real(r) => r,
        };
        if write && r.ro {
            return Err(libc::EROFS);
        }
        // user.* xattrs don't exist on symlinks; /proc reopen would follow it
        if r.fmt == S_IFLNK {
            return if write { Err(libc::EPERM) } else { Ok(None) };
        }
        Ok(Some(r))
    }

    pub fn getxattr(&self, ino: u64, name: &OsStr) -> Res<Vec<u8>> {
        let Some(r) = self.xattr_target(ino, Some(name), false)? else {
            return Err(libc::ENODATA);
        };
        sys::getxattr(&sys::proc_path(r.fd.as_fd()), &sys::cstring(name)?)
    }

    pub fn listxattr(&self, ino: u64) -> Res<Vec<u8>> {
        let Some(r) = self.xattr_target(ino, None, false)? else {
            return Ok(Vec::new());
        };
        let raw = sys::listxattr(&sys::proc_path(r.fd.as_fd()))?;
        let mut out = Vec::new();
        for n in raw.split_inclusive(|&b| b == 0) {
            if n.starts_with(b"user.") {
                out.extend_from_slice(n);
            }
        }
        Ok(out)
    }

    pub fn setxattr(&self, ino: u64, name: &OsStr, value: &[u8], flags: i32) -> Res<()> {
        let Some(r) = self.xattr_target(ino, Some(name), true)? else {
            unreachable!()
        };
        sys::setxattr(
            &sys::proc_path(r.fd.as_fd()),
            &sys::cstring(name)?,
            value,
            flags,
        )
    }

    pub fn removexattr(&self, ino: u64, name: &OsStr) -> Res<()> {
        let Some(r) = self.xattr_target(ino, Some(name), true)? else {
            unreachable!()
        };
        sys::removexattr(&sys::proc_path(r.fd.as_fd()), &sys::cstring(name)?)
    }

    // -------------------------------------------------------- control plane

    /// Walk/create the virtual directories for `comps`. Returns the last
    /// directory and, if anything was created, the invalidations for it.
    fn ensure_dirs(&self, s: &mut State, comps: &[OsString]) -> Result<(u64, Vec<Inval>), String> {
        let mut cur = ROOT_INO;
        let mut invals = Vec::new();
        for (i, c) in comps.iter().enumerate() {
            cur = match s.child(cur, c) {
                Some(ch) => match s.nodes.get(&ch).map(|n| &n.kind) {
                    Some(Kind::Virt(_)) => ch,
                    _ => {
                        return Err(format!(
                            "{} is a link; links can't be placed inside linked directories",
                            join(&comps[..=i]).display()
                        ));
                    }
                },
                None => {
                    let ino = s.alloc_ino();
                    s.nodes.insert(
                        ino,
                        Node {
                            nlookup: 0,
                            pinned: true,
                            kind: Kind::Virt(VDir {
                                children: BTreeMap::new(),
                                implicit: true,
                                mtime: SystemTime::now(),
                            }),
                        },
                    );
                    s.vdir_mut(cur).unwrap().children.insert(c.clone(), ino);
                    s.touch(cur);
                    if invals.is_empty() {
                        invals.push(Inval::Entry {
                            parent: cur,
                            name: c.clone(),
                        });
                        invals.push(Inval::Inode(cur));
                    }
                    ino
                }
            };
        }
        Ok((cur, invals))
    }

    /// Expose host path `source` at `target` (absolute path inside the fs).
    pub fn add_link(
        &self,
        target: &Path,
        source: &Path,
        ro: bool,
        replace: bool,
    ) -> Result<Vec<Inval>, String> {
        let comps = parse_target(target)?;
        let Some((name, dirs)) = comps.split_last() else {
            return Err("can't link over the root directory".into());
        };
        if !source.is_absolute() {
            return Err(format!(
                "{}: source must be an absolute path",
                source.display()
            ));
        }
        let csrc = sys::cstring(source.as_os_str()).map_err(|_| "bad source path".to_string())?;
        let fd = sys::open(&csrc, libc::O_PATH, 0)
            .map_err(|e| format!("{}: {}", source.display(), sys::strerror(e)))?;
        let st = sys::fstat(fd.as_fd())
            .map_err(|e| format!("{}: {}", source.display(), sys::strerror(e)))?;

        let mut s = self.st();
        // validate before mutating anything
        let mut cur = ROOT_INO;
        let mut existing_parent = true;
        for (i, c) in dirs.iter().enumerate() {
            match s.child(cur, c) {
                Some(ch) if s.vdir(ch).is_some() => cur = ch,
                Some(_) => {
                    return Err(format!(
                        "{} is a link; links can't be placed inside linked directories",
                        join(&comps[..=i]).display()
                    ));
                }
                None => {
                    existing_parent = false;
                    break;
                }
            }
        }
        let old = if existing_parent {
            s.child(cur, name)
        } else {
            None
        };
        if let Some(old) = old {
            if !replace {
                return Err(format!("{} already exists", target.display()));
            }
            if s.vdir(old).is_some_and(|v| !v.children.is_empty()) {
                return Err(format!("{} is a non-empty directory", target.display()));
            }
        }

        let (parent, mut invals) = self.ensure_dirs(&mut s, dirs)?;
        if let Some(old) = old {
            s.vdir_mut(parent).unwrap().children.remove(name);
            s.detach(old);
        }
        let root = s.next_root;
        s.next_root += 1;
        let ino = s.alloc_ino();
        s.nodes.insert(
            ino,
            Node {
                nlookup: 0,
                pinned: true,
                kind: Kind::Real(Real {
                    fd: Arc::new(fd),
                    root,
                    ro,
                    fmt: st.st_mode & S_IFMT,
                    key: None,
                    source: Some(source.to_path_buf()),
                }),
            },
        );
        s.vdir_mut(parent)
            .unwrap()
            .children
            .insert(name.clone(), ino);
        s.touch(parent);
        if invals.is_empty() {
            invals = vec![
                Inval::Entry {
                    parent,
                    name: name.clone(),
                },
                Inval::Inode(parent),
            ];
        }
        Ok(invals)
    }

    /// Create an explicit (persistent, not auto-pruned) virtual directory.
    pub fn mkdir_virtual(&self, target: &Path) -> Result<Vec<Inval>, String> {
        let comps = parse_target(target)?;
        if comps.is_empty() {
            return Ok(Vec::new());
        }
        let mut s = self.st();
        // ensure_dirs only fails before it has created anything (a fresh dir
        // has no children that could be links), so no partial state is left
        let (dir, invals) = self.ensure_dirs(&mut s, &comps)?;
        s.vdir_mut(dir).unwrap().implicit = false;
        Ok(invals)
    }

    /// Remove the link or virtual directory at `target`.
    pub fn remove(&self, target: &Path, recursive: bool) -> Result<Vec<Inval>, String> {
        let comps = parse_target(target)?;
        let Some((name, dirs)) = comps.split_last() else {
            return Err("can't remove the root directory".into());
        };
        let notfound = || format!("{}: no such entry", target.display());
        let mut s = self.st();
        let mut path = vec![ROOT_INO];
        for c in dirs {
            match s.child(*path.last().unwrap(), c) {
                Some(ch) if s.vdir(ch).is_some() => path.push(ch),
                _ => return Err(notfound()),
            }
        }
        let parent = *path.last().unwrap();
        let child = s.child(parent, name).ok_or_else(notfound)?;
        if !recursive && s.vdir(child).is_some_and(|v| !v.children.is_empty()) {
            return Err(format!(
                "{} is a non-empty directory (use recursive)",
                target.display()
            ));
        }
        s.vdir_mut(parent).unwrap().children.remove(name);
        s.touch(parent);
        s.detach(child);

        // prune now-empty implicit parents
        let mut top = (parent, name.clone());
        for i in (1..path.len()).rev() {
            let ino = path[i];
            let prune = s
                .vdir(ino)
                .is_some_and(|v| v.implicit && v.children.is_empty());
            if !prune {
                break;
            }
            let pp = path[i - 1];
            s.vdir_mut(pp).unwrap().children.remove(&comps[i - 1]);
            s.touch(pp);
            s.detach(ino);
            top = (pp, comps[i - 1].clone());
        }
        Ok(vec![
            Inval::Entry {
                parent: top.0,
                name: top.1,
            },
            Inval::Inode(top.0),
        ])
    }

    pub fn list(&self) -> Vec<ListEntry> {
        fn walk(s: &State, ino: u64, path: &mut PathBuf, out: &mut Vec<ListEntry>) {
            let Some(v) = s.vdir(ino) else { return };
            for (name, &c) in &v.children {
                path.push(name);
                match s.nodes.get(&c).map(|n| &n.kind) {
                    Some(Kind::Virt(cv)) => {
                        if !cv.implicit {
                            out.push(ListEntry::Dir {
                                target: path.clone(),
                            });
                        }
                        walk(s, c, path, out);
                    }
                    Some(Kind::Real(r)) => out.push(ListEntry::Link {
                        target: path.clone(),
                        source: r.source.clone().unwrap_or_default(),
                        ro: r.ro,
                    }),
                    None => {}
                }
                path.pop();
            }
        }
        let s = self.st();
        let mut out = Vec::new();
        walk(&s, ROOT_INO, &mut PathBuf::from("/"), &mut out);
        out
    }

    #[cfg(test)]
    pub fn inode_count(&self) -> usize {
        self.st().nodes.len()
    }

    #[cfg(test)]
    pub fn inode_list_for_tests(&self) -> Vec<u64> {
        self.st().nodes.keys().copied().collect()
    }
}
