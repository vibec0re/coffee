//! The virtual skeleton and the control plane that edits it.

use super::{
    Core, Inval, Kind, ListEntry, Node, ROOT_INO, Real, RealRef, RootSource, S_IFDIR, S_IFMT,
    State, VDir,
};
use crate::sys;
use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

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

/// What the kernel must forget when `name` appears in or vanishes from
/// `parent`.
fn entry_invals(parent: u64, name: &OsStr) -> Vec<Inval> {
    vec![
        Inval::Entry {
            parent,
            name: name.to_os_string(),
        },
        Inval::Inode(parent),
    ]
}

/// Open a link's host object.
fn open_source(source: &Path) -> Result<(OwnedFd, u32), String> {
    if !source.is_absolute() {
        return Err(format!(
            "{}: source must be an absolute path",
            source.display()
        ));
    }
    let err = |e| format!("{}: {}", source.display(), sys::strerror(e));
    let fd = sys::open(
        &sys::cstring(source.as_os_str()).map_err(err)?,
        libc::O_PATH,
        0,
    )
    .map_err(err)?;
    let st = sys::fstat(fd.as_fd()).map_err(err)?;
    Ok((fd, st.st_mode & S_IFMT))
}

/// The directory `name` in backing `b`, if there is one. Never through a
/// symlink: the root may be writable from inside, and a planted
/// `work -> /etc` must not become the backing of a virtual `/work`.
fn sub_backing(b: Option<&RealRef>, name: &OsStr) -> Option<RealRef> {
    let b = b?;
    let fd = sys::openat(
        b.fd.as_fd(),
        &sys::name(name).ok()?,
        libc::O_PATH | libc::O_NOFOLLOW | libc::O_DIRECTORY,
        0,
    )
    .ok()?;
    Some(RealRef {
        fd: Arc::new(fd),
        root: b.root,
        ro: b.ro,
        fmt: S_IFDIR,
    })
}

/// Names in host directory `b` (for invalidating what the kernel may have
/// cached from it). Best effort: an unreadable directory yields none.
fn dir_names(b: &RealRef) -> Vec<OsString> {
    use std::os::unix::ffi::OsStrExt;
    let Ok(fd) = sys::reopen(b.fd.as_fd(), libc::O_RDONLY | libc::O_DIRECTORY) else {
        return Vec::new();
    };
    let Ok(mut dir) = nix::dir::Dir::from_fd(fd) else {
        return Vec::new();
    };
    dir.iter()
        .filter_map(Result::ok)
        .map(|e| OsStr::from_bytes(e.file_name().to_bytes()).to_os_string())
        .filter(|n| n != "." && n != "..")
        .collect()
}

impl State {
    pub(super) fn alloc_ino(&mut self) -> u64 {
        let i = self.next_ino;
        self.next_ino += 1;
        i
    }

    pub(super) fn vdir(&self, ino: u64) -> Option<&VDir> {
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

    pub(super) fn child(&self, parent: u64, name: &OsStr) -> Option<u64> {
        self.vdir(parent)?.children.get(name).copied()
    }

    pub(super) fn maybe_free(&mut self, ino: u64) {
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

    /// Unpin `ino` (and, for virtual dirs, everything below). Nodes the
    /// kernel still references stay alive until forgotten, so open files and
    /// cwds inside a removed link keep working.
    fn detach(&mut self, ino: u64) {
        let children = match self.nodes.get_mut(&ino) {
            Some(n) => {
                n.pinned = false;
                match &mut n.kind {
                    Kind::Virt(v) => std::mem::take(&mut v.children),
                    Kind::Real(_) => Default::default(),
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

    /// Hang a new node off virtual directory `parent`.
    fn attach(&mut self, parent: u64, name: &OsStr, kind: Kind) -> u64 {
        let ino = self.alloc_ino();
        let node = Node {
            nlookup: 0,
            pinned: true,
            kind,
        };
        self.nodes.insert(ino, node);
        if let Some(v) = self.vdir_mut(parent) {
            v.children.insert(name.to_os_string(), ino);
        }
        self.touch(parent);
        ino
    }

    /// Take `name` out of virtual directory `parent` and detach it.
    fn unhook(&mut self, parent: u64, name: &OsStr) {
        if let Some(c) = self.vdir_mut(parent).and_then(|v| v.children.remove(name)) {
            self.touch(parent);
            self.detach(c);
        }
    }

    /// Follow `comps` through virtual directories, stopping at the first
    /// missing one. Returns the inodes walked, starting with the root
    /// (`len() == comps.len() + 1` if all exist). Fails at a link.
    fn walk(&self, comps: &[OsString]) -> Result<Vec<u64>, String> {
        let mut path = vec![ROOT_INO];
        for (i, c) in comps.iter().enumerate() {
            let cur = path[path.len() - 1];
            match self.child(cur, c) {
                None => break,
                Some(ch) if self.vdir(ch).is_some() => path.push(ch),
                Some(_) => {
                    return Err(format!(
                        "{} is a link; links can't be placed inside linked directories",
                        join(&comps[..=i]).display()
                    ));
                }
            }
        }
        Ok(path)
    }

    /// Walk `comps`, creating missing directories as implicit ones. Returns
    /// the last directory and, if anything was created, the invalidations.
    /// Fails (before creating anything) if a component is a link.
    fn ensure_dirs(&mut self, comps: &[OsString]) -> Result<(u64, Vec<Inval>), String> {
        let path = self.walk(comps)?;
        let mut cur = path[path.len() - 1];
        let missing = &comps[path.len() - 1..];
        let invals = missing
            .first()
            .map_or_else(Vec::new, |c| entry_invals(cur, c));
        for c in missing {
            let backing = sub_backing(self.vdir(cur).and_then(|v| v.backing.as_ref()), c);
            cur = self.attach(cur, c, Kind::Virt(VDir::new(true, backing)));
        }
        Ok((cur, invals))
    }

    /// Give virtual directory `ino` a new backing (or none), and its virtual
    /// subdirectories theirs, resolved again by name below it. Collects what
    /// the kernel must forget: the names it may have looked up in the old
    /// backing, and the directory's attributes and listing.
    fn rebase(&mut self, ino: u64, backing: Option<RealRef>, invals: &mut Vec<Inval>) {
        let Some(v) = self.vdir_mut(ino) else { return };
        let old = std::mem::replace(&mut v.backing, backing.clone());
        let children: Vec<(OsString, u64)> =
            v.children.iter().map(|(n, &c)| (n.clone(), c)).collect();
        if let Some(old) = &old {
            invals.extend(
                dir_names(old)
                    .into_iter()
                    .filter(|n| !children.iter().any(|(c, _)| c == n))
                    .map(|name| Inval::Entry { parent: ino, name }),
            );
        }
        invals.push(Inval::Inode(ino));
        for (name, c) in children {
            if self.vdir(c).is_some() {
                let b = sub_backing(backing.as_ref(), &name);
                self.rebase(c, b, invals);
            }
        }
    }

    fn list(&self, ino: u64, path: &mut PathBuf, out: &mut Vec<ListEntry>) {
        let Some(v) = self.vdir(ino) else { return };
        for (name, &c) in &v.children {
            path.push(name);
            match self.nodes.get(&c).map(|n| &n.kind) {
                Some(Kind::Virt(cv)) => {
                    if !cv.implicit {
                        out.push(ListEntry::Dir {
                            target: path.clone(),
                        });
                    }
                    self.list(c, path, out);
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
}

impl Core {
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
            return Err("can't link over the root directory (set a root instead)".into());
        };
        let (fd, fmt) = open_source(source)?;

        let mut s = self.st();
        // validate before mutating anything
        let path = s.walk(dirs)?;
        let old = (path.len() == comps.len())
            .then(|| s.child(path[path.len() - 1], name))
            .flatten();
        if let Some(old) = old {
            if !replace {
                return Err(format!("{} already exists", target.display()));
            }
            if s.vdir(old).is_some_and(|v| !v.children.is_empty()) {
                return Err(format!("{} is a non-empty directory", target.display()));
            }
        }

        let (parent, invals) = s.ensure_dirs(dirs)?;
        s.unhook(parent, name);
        let root = s.next_root;
        s.next_root += 1;
        let link = Real {
            fd: Arc::new(fd),
            root,
            ro,
            fmt,
            key: None,
            source: Some(source.to_path_buf()),
        };
        s.attach(parent, name, Kind::Real(link));
        Ok(if invals.is_empty() {
            entry_invals(parent, name)
        } else {
            invals
        })
    }

    /// Make host directory `source` the root: what no link or virtual
    /// directory covers falls through to it, at `/` and below.
    pub fn set_root(&self, source: &Path, ro: bool, replace: bool) -> Result<Vec<Inval>, String> {
        let (fd, fmt) = open_source(source)?;
        if fmt != S_IFDIR {
            return Err(format!("{}: not a directory", source.display()));
        }
        let mut s = self.st();
        if !replace && let Some(old) = &s.root {
            return Err(format!(
                "the root is already set (to {})",
                old.source.display()
            ));
        }
        // a root id of its own, so nothing the old root handed out is reused
        // for the new one, which may differ in ro
        let root = s.next_root;
        s.next_root += 1;
        let backing = RealRef {
            fd: Arc::new(fd),
            root,
            ro,
            fmt,
        };
        s.root = Some(RootSource {
            source: source.to_path_buf(),
            ro,
        });
        let mut invals = Vec::new();
        s.rebase(ROOT_INO, Some(backing), &mut invals);
        Ok(invals)
    }

    /// Back to no root. What the kernel still holds from it (open files,
    /// cwds) keeps working, as after removing a link.
    pub fn clear_root(&self) -> Result<Vec<Inval>, String> {
        let mut s = self.st();
        if s.root.take().is_none() {
            return Err("/: no root set".into());
        }
        let mut invals = Vec::new();
        s.rebase(ROOT_INO, None, &mut invals);
        Ok(invals)
    }

    /// Undo [`Core::mkdir_virtual`]: the directory becomes implicit again, so
    /// it stays only while something below it does.
    pub fn demote_dir(&self, target: &Path) -> Result<Vec<Inval>, String> {
        let comps = parse_target(target)?;
        if comps.is_empty() {
            return Ok(Vec::new());
        }
        let empty = {
            let mut s = self.st();
            let path = s.walk(&comps)?;
            if path.len() != comps.len() + 1 {
                return Ok(Vec::new());
            }
            match s.vdir_mut(path[path.len() - 1]) {
                Some(v) => {
                    v.implicit = true;
                    v.children.is_empty()
                }
                None => return Ok(Vec::new()),
            }
        };
        // control-plane changes are serialized by the caller, so it's still empty
        if empty {
            self.remove(target, false)
        } else {
            Ok(Vec::new())
        }
    }

    /// Create an explicit (persistent, not auto-pruned) virtual directory.
    pub fn mkdir_virtual(&self, target: &Path) -> Result<Vec<Inval>, String> {
        let comps = parse_target(target)?;
        let mut s = self.st();
        let (dir, invals) = s.ensure_dirs(&comps)?;
        if let Some(v) = s.vdir_mut(dir) {
            v.implicit = false;
        }
        Ok(invals)
    }

    /// Remove the link or virtual directory at `target`, and any implicit
    /// parents that are left empty. At `/`, clear the root.
    pub fn remove(&self, target: &Path, recursive: bool) -> Result<Vec<Inval>, String> {
        let comps = parse_target(target)?;
        let Some((name, dirs)) = comps.split_last() else {
            return self.clear_root();
        };
        let notfound = || format!("{}: no such entry", target.display());
        let mut s = self.st();
        let path = s.walk(dirs).map_err(|_| notfound())?;
        if path.len() != comps.len() {
            return Err(notfound());
        }
        let parent = path[path.len() - 1];
        let child = s.child(parent, name).ok_or_else(notfound)?;
        if !recursive && s.vdir(child).is_some_and(|v| !v.children.is_empty()) {
            return Err(format!(
                "{} is a non-empty directory (use recursive)",
                target.display()
            ));
        }
        s.unhook(parent, name);
        let mut top = (parent, name);
        for i in (1..path.len()).rev() {
            let empty_implicit = s
                .vdir(path[i])
                .is_some_and(|v| v.implicit && v.children.is_empty());
            if !empty_implicit {
                break;
            }
            s.unhook(path[i - 1], &comps[i - 1]);
            top = (path[i - 1], &comps[i - 1]);
        }
        Ok(entry_invals(top.0, top.1))
    }

    pub fn list(&self) -> Vec<ListEntry> {
        let s = self.st();
        let mut out: Vec<ListEntry> = s
            .root
            .iter()
            .map(|r| ListEntry::Root {
                source: r.source.clone(),
                ro: r.ro,
            })
            .collect();
        s.list(ROOT_INO, &mut PathBuf::from("/"), &mut out);
        out
    }
}
