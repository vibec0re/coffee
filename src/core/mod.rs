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
//! Optionally the tree has a **root**: a host directory that the virtual
//! directories fall through to, a bit like a default route. `/` is then the
//! root directory, with the links and virtual directories laid over it. A
//! virtual directory below `/` falls through to the root's directory of the
//! same name, if there is one, so a link at `/work/proj` leaves the rest of
//! the root's `work/` visible next to it. Anything that isn't a link or a
//! virtual directory is plain passthrough to the root, writes included,
//! unless the root is read-only. A name in the tree covers the same name in
//! the root, the way a mount does: it can't be removed or renamed from inside
//! (`EBUSY`), and what it covers is out of reach until it goes.
//!
//! Every real node holds an `O_PATH` fd to its host object. Children are
//! resolved strictly one component at a time with `openat(O_NOFOLLOW)`, so a
//! symlink planted inside a shared directory can never make the (root-running)
//! daemon touch anything outside that directory: symlinks are returned to the
//! kernel as symlinks and resolved in the *container's* view.
//!
//! Each link gets its own "root id", and so does each root that is set.
//! Inodes are keyed by `(root id, dev, ino)`,
//! so the same host file reached via two links is two distinct inodes, and
//! `rename`/`link` across links fail with `EXDEV` (like separate bind mounts).
//! That keeps a read-only link from being written through a read-write one.
//!
//! Layout: this file holds the types, the inode table and lookups;
//! [`tree`] the virtual skeleton and the control plane; [`ns`] namespace
//! operations; [`io`] open files and directory listings; [`attr`] and
//! [`xattr`] attributes.

mod attr;
mod io;
mod ns;
mod tree;
mod xattr;

use crate::sys::{self, Res};
use attr::stat_attr;
use fuser::{FileAttr, FileType, INodeNo, Owner};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

pub use tree::parse_target;

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
    /// The host directory everything no link or virtual dir covers falls
    /// through to. Its target is always `/`.
    Root {
        source: PathBuf,
        #[serde(default)]
        ro: bool,
    },
}

impl ListEntry {
    pub fn target(&self) -> &Path {
        match self {
            ListEntry::Link { target, .. } | ListEntry::Dir { target } => target,
            ListEntry::Root { .. } => Path::new("/"),
        }
    }
}

/// Who is creating something: the FUSE request's owner and umask.
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    pub uid: u32,
    pub gid: u32,
    pub umask: u32,
}

impl Caller {
    /// The permission bits a new object gets for requested `mode`.
    fn mode(self, mode: u32) -> u32 {
        mode & !self.umask & 0o7777
    }
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
    /// The root's directory at this path, which everything that isn't a
    /// child falls through to. Resolved when the directory is made (or the
    /// root is set), one component at a time and never through a symlink.
    backing: Option<RealRef>,
}

impl VDir {
    fn new(implicit: bool, backing: Option<RealRef>) -> VDir {
        VDir {
            children: BTreeMap::new(),
            implicit,
            mtime: SystemTime::now(),
            backing,
        }
    }
}

/// The root, as it was asked for.
struct RootSource {
    source: PathBuf,
    ro: bool,
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

/// A real node's host object, usable without holding the state lock.
#[derive(Clone)]
struct RealRef {
    fd: Arc<OwnedFd>,
    root: u64,
    ro: bool,
    fmt: u32,
}

impl From<&Real> for RealRef {
    fn from(r: &Real) -> Self {
        RealRef {
            fd: r.fd.clone(),
            root: r.root,
            ro: r.ro,
            fmt: r.fmt,
        }
    }
}

enum NodeRef {
    /// A virtual directory: its own attributes, and its backing if it has
    /// one (whose attributes it then reports instead).
    Virt(FileAttr, Option<RealRef>),
    Real(RealRef),
}

struct State {
    nodes: HashMap<u64, Node>,
    by_key: HashMap<(u64, u64, u64), u64>,
    next_ino: u64,
    next_root: u64,
    root: Option<RootSource>,
}

#[derive(Clone)]
enum Handle {
    File(Arc<File>),
    Dir(Arc<Mutex<DirState>>),
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
    handles: Mutex<HashMap<u64, Handle>>,
    next_fh: AtomicU64,
    /// Owner reported for virtual directories.
    owner: Owner,
    /// Create objects as the requesting user (needs root): see `sys::AsCaller`.
    switch_creds: bool,
    born: SystemTime,
}

impl Core {
    pub fn new(owner: Owner, switch_creds: bool) -> Core {
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT_INO,
            Node {
                nlookup: 1,
                pinned: true,
                kind: Kind::Virt(VDir::new(false, None)),
            },
        );
        Core {
            st: Mutex::new(State {
                nodes,
                by_key: HashMap::new(),
                next_ino: ROOT_INO + 1,
                next_root: 1,
                root: None,
            }),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            owner,
            switch_creds,
            born: SystemTime::now(),
        }
    }

    fn st(&self) -> MutexGuard<'_, State> {
        sys::lock(&self.st)
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
            nlink: 2 + u32::try_from(subdirs).unwrap_or(u32::MAX - 2),
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
            Kind::Virt(v) => NodeRef::Virt(self.vattr(&s, ino, v), v.backing.clone()),
            Kind::Real(r) => NodeRef::Real(r.into()),
        })
    }

    /// The host object behind `ino`: a real node's, or a virtual directory's
    /// backing. `virt_err` for a virtual directory without one.
    fn real(&self, ino: u64, virt_err: i32) -> Res<RealRef> {
        match self.get(ino)? {
            NodeRef::Virt(_, b) => b.ok_or(virt_err),
            NodeRef::Real(r) => Ok(r),
        }
    }

    /// The host directory in which `name` may be created or removed in
    /// directory `ino`. The virtual skeleton is managed only via the control
    /// socket, so a virtual directory reads as read-only, unless it has a
    /// backing. Even then, a name in the tree (`shadowed`: `EEXIST` to create
    /// it, `EBUSY` to remove it) stays out of reach, like a mountpoint: the
    /// operation would otherwise land on the backing's object it covers.
    fn wdir(&self, ino: u64, name: &OsStr, shadowed: i32) -> Res<RealRef> {
        let r = {
            let s = self.st();
            match &s.nodes.get(&ino).ok_or(libc::ESTALE)?.kind {
                Kind::Real(r) => RealRef::from(r),
                Kind::Virt(v) => {
                    let b = v.backing.clone().ok_or(libc::EROFS)?;
                    if v.children.contains_key(name) {
                        return Err(shadowed);
                    }
                    b
                }
            }
        };
        if r.fmt != S_IFDIR {
            return Err(libc::ENOTDIR);
        }
        if r.ro {
            return Err(libc::EROFS);
        }
        Ok(r)
    }

    /// Enter a host object (`O_PATH` fd) into the inode table, or bump the
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

    pub fn lookup(&self, parent: u64, name: &OsStr) -> Res<FileAttr> {
        let cname = sys::name(name)?;
        let p = {
            let mut s = self.st();
            match &s.nodes.get(&parent).ok_or(libc::ESTALE)?.kind {
                Kind::Real(r) => RealRef::from(r),
                Kind::Virt(v) => match (v.children.contains_key(name), v.backing.clone()) {
                    // not in the tree: fall through to the root
                    (false, Some(b)) => b,
                    _ => {
                        let found = self.lookup_virtual(&mut s, parent, name)?;
                        drop(s);
                        return found.attr(self);
                    }
                },
            }
        };
        if p.fmt != S_IFDIR {
            return Err(libc::ENOTDIR);
        }
        let fd = sys::openat(p.fd.as_fd(), &cname, libc::O_PATH | libc::O_NOFOLLOW, 0)?;
        self.register(fd, p.root, p.ro)
    }

    /// A child of a virtual directory. Takes the kernel reference right away,
    /// so a concurrent `rm` can't free the node before we reply.
    fn lookup_virtual(&self, s: &mut State, parent: u64, name: &OsStr) -> Res<Found> {
        let child = s.child(parent, name).ok_or(libc::ENOENT)?;
        s.nodes.get_mut(&child).ok_or(libc::ENOENT)?.nlookup += 1;
        Ok(match &s.nodes[&child].kind {
            Kind::Real(r) => Found::LinkRoot(child, r.fd.clone()),
            Kind::Virt(v) => Found::Virt(self.vattr(s, child, v)),
        })
    }

    pub fn forget(&self, ino: u64, n: u64) {
        let mut s = self.st();
        if let Some(node) = s.nodes.get_mut(&ino) {
            node.nlookup = node.nlookup.saturating_sub(n);
            s.maybe_free(ino);
        }
    }

    pub fn getattr(&self, ino: u64) -> Res<FileAttr> {
        match self.get(ino)? {
            NodeRef::Virt(a, None) => Ok(a),
            NodeRef::Virt(_, Some(r)) | NodeRef::Real(r) => {
                Ok(stat_attr(ino, &sys::fstat(r.fd.as_fd())?))
            }
        }
    }

    /// Inodes in the table, and open file/directory handles.
    pub fn counts(&self) -> (usize, usize) {
        (self.st().nodes.len(), sys::lock(&self.handles).len())
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

/// What a lookup in a virtual directory found (already referenced).
enum Found {
    Virt(FileAttr),
    /// A link root: stat it outside the state lock.
    LinkRoot(u64, Arc<OwnedFd>),
}

impl Found {
    fn attr(self, core: &Core) -> Res<FileAttr> {
        match self {
            Found::Virt(a) => Ok(a),
            Found::LinkRoot(ino, fd) => sys::fstat(fd.as_fd())
                .map(|st| stat_attr(ino, &st))
                .inspect_err(|_| core.forget(ino, 1)),
        }
    }
}
