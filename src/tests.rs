//! Core + control-plane tests. These drive `Core` directly (no kernel
//! involved), so they run unprivileged and without /dev/fuse. The FUSE/kernel
//! side is covered by the NixOS VM test in nix/test.nix.

use crate::core::*;
use crate::ctl::{self, Controller, Entry, NoInval, Origin, Request};
use fuser::{FileAttr, FileType, Owner};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

const RD: i32 = libc::O_RDONLY;
const WR: i32 = libc::O_WRONLY;

fn me() -> Owner {
    Owner {
        uid: nix::unistd::geteuid().as_raw(),
        gid: nix::unistd::getegid().as_raw(),
    }
}

fn caller(umask: u32) -> Caller {
    let o = me();
    Caller {
        uid: o.uid,
        gid: o.gid,
        umask,
    }
}

fn core() -> Core {
    Core::new(me(), false)
}

/// Run `f` on a thread with its own fs_struct and the given umask, so a
/// test can pick a umask without disturbing the tests running next to it.
fn with_umask<T: Send>(umask: u32, f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        s.spawn(|| {
            assert_eq!(unsafe { libc::unshare(libc::CLONE_FS) }, 0);
            unsafe { libc::umask(umask) };
            f()
        })
        .join()
        .unwrap()
    })
}

fn mode_of(p: &Path) -> u32 {
    fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}

fn os(s: &str) -> &OsStr {
    OsStr::new(s)
}

fn walk(c: &Core, path: &str) -> Result<FileAttr, i32> {
    let mut a = c.getattr(ROOT_INO)?;
    for comp in path.split('/').filter(|s| !s.is_empty()) {
        a = c.lookup(a.ino.0, os(comp))?;
    }
    Ok(a)
}

fn ino(c: &Core, path: &str) -> u64 {
    walk(c, path)
        .unwrap_or_else(|e| panic!("{path}: errno {e}"))
        .ino
        .0
}

fn read_all(c: &Core, ino: u64) -> Vec<u8> {
    let fh = c.open(ino, RD).unwrap();
    let d = c.read(fh, 0, 1 << 20).unwrap();
    c.release(fh);
    d
}

fn names(c: &Core, ino: u64) -> Vec<String> {
    let fh = c.opendir(ino).unwrap();
    let mut v = Vec::new();
    c.readdir(fh, 0, |_, _, _, n| {
        v.push(n.to_string_lossy().into_owned());
        false
    })
    .unwrap();
    c.release(fh);
    v.sort();
    v
}

fn add(c: &Core, target: &str, source: &Path, ro: bool) -> Vec<Inval> {
    c.add_link(Path::new(target), source, ro, false).unwrap()
}

/// host layout: a.txt, sub/b.txt
fn host() -> TempDir {
    let t = TempDir::new().unwrap();
    fs::write(t.path().join("a.txt"), "hello").unwrap();
    fs::create_dir(t.path().join("sub")).unwrap();
    fs::write(t.path().join("sub/b.txt"), "bee").unwrap();
    t
}

#[test]
fn links_appear_in_virtual_tree() {
    let h = host();
    let c = core();
    let inv = add(&c, "/work/proj", h.path(), false);
    assert_eq!(
        inv,
        vec![
            Inval::Entry {
                parent: ROOT_INO,
                name: "work".into()
            },
            Inval::Inode(ROOT_INO)
        ]
    );

    let work = walk(&c, "/work").unwrap();
    assert_eq!(work.kind, FileType::Directory);
    assert_eq!(work.perm, 0o555);
    assert_eq!(names(&c, ROOT_INO), [".", "..", "work"]);
    assert_eq!(
        names(&c, ino(&c, "/work/proj")),
        [".", "..", "a.txt", "sub"]
    );
    assert_eq!(read_all(&c, ino(&c, "/work/proj/a.txt")), b"hello");
    assert_eq!(read_all(&c, ino(&c, "/work/proj/sub/b.txt")), b"bee");

    // a single file can be linked too
    add(&c, "/etc/greeting", &h.path().join("a.txt"), true);
    let g = walk(&c, "/etc/greeting").unwrap();
    assert_eq!(g.kind, FileType::RegularFile);
    assert_eq!(read_all(&c, g.ino.0), b"hello");

    assert_eq!(
        c.list(),
        vec![
            ListEntry::Link {
                target: "/etc/greeting".into(),
                source: h.path().join("a.txt"),
                ro: true
            },
            ListEntry::Link {
                target: "/work/proj".into(),
                source: h.path().into(),
                ro: false
            },
        ]
    );
    assert_eq!(c.lookup(ROOT_INO, os("nope")), Err(libc::ENOENT));
}

#[test]
fn readdir_reports_known_inode_numbers() {
    let h = host();
    let c = core();
    add(&c, "/p", h.path(), false);
    let a = ino(&c, "/p/a.txt");
    let fh = c.opendir(ino(&c, "/p")).unwrap();
    let mut seen = None;
    c.readdir(fh, 0, |i, _, k, n| {
        if n == "a.txt" {
            seen = Some((i, k));
        }
        false
    })
    .unwrap();
    assert_eq!(seen, Some((a, FileType::RegularFile)));
}

#[test]
fn readdir_resumes_at_offset() {
    let h = host();
    let c = core();
    add(&c, "/p", h.path(), false);
    let fh = c.opendir(ino(&c, "/p")).unwrap();
    let mut first = Vec::new();
    c.readdir(fh, 0, |_, off, _, n| {
        first.push((off, n.to_owned()));
        first.len() == 2 // "buffer full" after two
    })
    .unwrap();
    let mut rest = Vec::new();
    c.readdir(fh, first[1].0, |_, _, _, n| {
        rest.push(n.to_owned());
        false
    })
    .unwrap();
    assert_eq!(first.len() + rest.len(), 4);
}

#[test]
fn ro_link_rejects_every_write() {
    let h = host();
    let c = core();
    add(&c, "/ro", h.path(), true);
    let dir = ino(&c, "/ro");
    let file = ino(&c, "/ro/a.txt");

    assert_eq!(c.open(file, WR), Err(libc::EROFS));
    assert_eq!(c.open(file, libc::O_RDWR), Err(libc::EROFS));
    assert_eq!(c.open(file, RD | libc::O_TRUNC), Err(libc::EROFS));
    assert_eq!(
        c.create(dir, os("n"), 0o644, WR, caller(0)).err(),
        Some(libc::EROFS)
    );
    assert_eq!(
        c.mkdir(dir, os("d"), 0o755, caller(0)).err(),
        Some(libc::EROFS)
    );
    assert_eq!(c.unlink(dir, os("a.txt")), Err(libc::EROFS));
    assert_eq!(c.rmdir(dir, os("sub")), Err(libc::EROFS));
    assert_eq!(
        c.rename((dir, os("a.txt")), (dir, os("z")), 0),
        Err(libc::EROFS)
    );
    assert_eq!(
        c.symlink(dir, os("l"), Path::new("x"), caller(0)).err(),
        Some(libc::EROFS)
    );
    let chmod = SetAttr {
        mode: Some(0o600),
        ..Default::default()
    };
    assert_eq!(c.setattr(file, &chmod, None).err(), Some(libc::EROFS));
    assert_eq!(c.setxattr(file, os("user.x"), b"1", 0), Err(libc::EROFS));

    assert_eq!(read_all(&c, file), b"hello");
    assert_eq!(fs::read_to_string(h.path().join("a.txt")).unwrap(), "hello");
}

#[test]
fn rw_link_passes_writes_through() {
    let h = host();
    let c = core();
    add(&c, "/rw", h.path(), false);
    let dir = ino(&c, "/rw");

    // create + write
    let (a, fh) = c
        .create(dir, os("new.txt"), 0o664, libc::O_RDWR, caller(0o002))
        .unwrap();
    assert_eq!(a.kind, FileType::RegularFile);
    assert_eq!(c.write(fh, 0, b"written").unwrap(), 7);
    assert_eq!(c.read(fh, 0, 100).unwrap(), b"written");
    c.release(fh);
    let p = h.path().join("new.txt");
    assert_eq!(fs::read_to_string(&p).unwrap(), "written");
    assert_eq!(
        fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o664
    );

    // umask is honoured
    let (_, fh) = c
        .create(dir, os("masked"), 0o666, WR, caller(0o027))
        .unwrap();
    c.release(fh);
    assert_eq!(
        fs::metadata(h.path().join("masked"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );

    // mkdir / symlink / readlink
    let d = c.mkdir(dir, os("d"), 0o750, caller(0)).unwrap();
    assert_eq!(d.kind, FileType::Directory);
    assert!(h.path().join("d").is_dir());
    let l = c
        .symlink(dir, os("ln"), Path::new("new.txt"), caller(0))
        .unwrap();
    assert_eq!(l.kind, FileType::Symlink);
    assert_eq!(c.readlink(l.ino.0).unwrap(), b"new.txt");

    // rename + hardlink (same inode via both names)
    c.rename((dir, os("new.txt")), (d.ino.0, os("moved.txt")), 0)
        .unwrap();
    assert!(h.path().join("d/moved.txt").exists());
    assert_eq!(
        c.rename(
            (dir, os("a.txt")),
            (d.ino.0, os("moved.txt")),
            libc::RENAME_NOREPLACE
        ),
        Err(libc::EEXIST)
    );
    let moved = c.lookup(d.ino.0, os("moved.txt")).unwrap();
    let hl = c.link(moved.ino.0, dir, os("hard")).unwrap();
    assert_eq!(hl.ino, moved.ino);
    assert_eq!(hl.nlink, 2);

    // setattr: truncate, chmod, mtime
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    let sa = SetAttr {
        size: Some(3),
        mode: Some(0o600),
        mtime: Some(TimeSpec::At(t)),
        ..Default::default()
    };
    let after = c.setattr(moved.ino.0, &sa, None).unwrap();
    assert_eq!((after.size, after.perm, after.mtime), (3, 0o600, t));
    assert_eq!(fs::read_to_string(h.path().join("hard")).unwrap(), "wri");

    // fallocate/lseek/copy_file_range via handles
    let src = c.open(moved.ino.0, RD).unwrap();
    let (_, dst) = c
        .create(dir, os("copy"), 0o644, libc::O_RDWR, caller(0))
        .unwrap();
    assert_eq!(c.copy_file_range((src, 0), (dst, 0), 3, 0).unwrap(), 3);
    assert_eq!(c.read(dst, 0, 10).unwrap(), b"wri");
    assert_eq!(c.lseek(dst, 0, libc::SEEK_DATA).unwrap(), 0);
    c.release(src);
    c.release(dst);

    // unlink / rmdir
    c.unlink(dir, os("hard")).unwrap();
    c.unlink(d.ino.0, os("moved.txt")).unwrap();
    c.rmdir(dir, os("d")).unwrap();
    assert!(!h.path().join("d").exists());
    assert_eq!(c.rmdir(dir, os("sub")), Err(libc::ENOTEMPTY));
}

#[test]
fn mknod_refuses_device_nodes() {
    let h = host();
    let c = core();
    add(&c, "/rw", h.path(), false);
    let dir = ino(&c, "/rw");
    assert_eq!(
        c.mknod(dir, os("dev"), libc::S_IFCHR | 0o666, caller(0))
            .err(),
        Some(libc::EPERM)
    );
    assert_eq!(
        c.mknod(dir, os("blk"), libc::S_IFBLK | 0o666, caller(0))
            .err(),
        Some(libc::EPERM)
    );
    let f = c
        .mknod(dir, os("fifo"), libc::S_IFIFO | 0o600, caller(0))
        .unwrap();
    assert_eq!(f.kind, FileType::NamedPipe);
}

#[test]
fn virtual_skeleton_is_read_only() {
    let h = host();
    let c = core();
    add(&c, "/work/proj", h.path(), false);
    let work = ino(&c, "/work");
    for parent in [ROOT_INO, work] {
        assert_eq!(
            c.create(parent, os("f"), 0o644, WR, caller(0)).err(),
            Some(libc::EROFS)
        );
        assert_eq!(
            c.mkdir(parent, os("d"), 0o755, caller(0)).err(),
            Some(libc::EROFS)
        );
        assert_eq!(
            c.symlink(parent, os("s"), Path::new("x"), caller(0)).err(),
            Some(libc::EROFS)
        );
        assert_eq!(
            c.mknod(parent, os("p"), libc::S_IFIFO | 0o644, caller(0))
                .err(),
            Some(libc::EROFS)
        );
    }
    // links themselves can't be removed/renamed/replaced from inside
    assert_eq!(c.rmdir(work, os("proj")), Err(libc::EROFS));
    assert_eq!(c.unlink(work, os("proj")), Err(libc::EROFS));
    assert_eq!(
        c.rename((work, os("proj")), (work, os("x")), 0),
        Err(libc::EROFS)
    );
    let proj = ino(&c, "/work/proj");
    assert_eq!(
        c.rename((proj, os("a.txt")), (work, os("a.txt")), 0),
        Err(libc::EROFS)
    );
    let chmod = SetAttr {
        mode: Some(0o777),
        ..Default::default()
    };
    assert_eq!(c.setattr(work, &chmod, None).err(), Some(libc::EROFS));
}

#[test]
fn ops_across_links_are_exdev() {
    let (h1, h2) = (host(), host());
    let c = core();
    add(&c, "/one", h1.path(), false);
    add(&c, "/two", h2.path(), false);
    let (one, two) = (ino(&c, "/one"), ino(&c, "/two"));
    assert_eq!(
        c.rename((one, os("a.txt")), (two, os("x")), 0),
        Err(libc::EXDEV)
    );
    let a = ino(&c, "/one/a.txt");
    assert_eq!(c.link(a, two, os("x")).err(), Some(libc::EXDEV));

    // Same host dir linked twice, once ro: hardlinking out of the ro view into
    // the rw one would make the ro file writable.
    let c = core();
    add(&c, "/ro", h1.path(), true);
    add(&c, "/rw", h1.path(), false);
    let ro_a = ino(&c, "/ro/a.txt");
    let rw_a = ino(&c, "/rw/a.txt");
    assert_ne!(ro_a, rw_a, "inodes must not be shared between links");
    assert_eq!(
        c.link(ro_a, ino(&c, "/rw"), os("x")).err(),
        Some(libc::EXDEV)
    );
}

#[test]
fn symlinks_are_never_followed_on_the_host() {
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("secret"), "s3cret").unwrap();
    let secret_mode = || {
        fs::metadata(outside.path().join("secret"))
            .unwrap()
            .permissions()
            .mode()
    };
    let mode_before = secret_mode();
    let h = host();
    symlink(outside.path(), h.path().join("evildir")).unwrap();
    symlink(outside.path().join("secret"), h.path().join("evilfile")).unwrap();
    let c = core();
    add(&c, "/rw", h.path(), false);
    let dir = ino(&c, "/rw");

    let ed = c.lookup(dir, os("evildir")).unwrap();
    assert_eq!(ed.kind, FileType::Symlink);
    assert_eq!(c.lookup(ed.ino.0, os("secret")), Err(libc::ENOTDIR));
    assert_eq!(c.opendir(ed.ino.0).err(), Some(libc::ENOTDIR));
    assert_eq!(
        c.readlink(ed.ino.0).unwrap(),
        outside.path().as_os_str().as_encoded_bytes()
    );

    let ef = c.lookup(dir, os("evilfile")).unwrap();
    assert_eq!(ef.kind, FileType::Symlink);
    assert_eq!(c.open(ef.ino.0, RD), Err(libc::ELOOP));
    assert_eq!(c.open(ef.ino.0, WR | libc::O_TRUNC), Err(libc::ELOOP));
    assert_eq!(
        c.create(dir, os("evilfile"), 0o644, WR | libc::O_TRUNC, caller(0))
            .err(),
        Some(libc::ELOOP)
    );
    let trunc = SetAttr {
        size: Some(0),
        ..Default::default()
    };
    assert_eq!(c.setattr(ef.ino.0, &trunc, None).err(), Some(libc::EINVAL));
    let chmod = SetAttr {
        mode: Some(0o777),
        ..Default::default()
    };
    assert_eq!(
        c.setattr(ef.ino.0, &chmod, None).err(),
        Some(libc::EOPNOTSUPP)
    );
    assert_eq!(
        c.setxattr(ef.ino.0, os("user.x"), b"1", 0),
        Err(libc::EPERM)
    );
    assert_eq!(
        c.mkdir(dir, os("evildir"), 0o755, caller(0)).err(),
        Some(libc::EEXIST)
    );

    assert_eq!(
        fs::read_to_string(outside.path().join("secret")).unwrap(),
        "s3cret"
    );
    assert_eq!(secret_mode(), mode_before);
}

#[test]
fn removal_detaches_but_open_files_keep_working() {
    let h = host();
    let c = core();
    let base = c.inode_count();
    add(&c, "/work/proj", h.path(), false);
    let file = ino(&c, "/work/proj/a.txt");
    let fh = c.open(file, RD).unwrap();

    let inv = c.remove(Path::new("/work/proj"), false).unwrap();
    // /work was implicit and is now empty -> pruned, invalidate from the root
    assert_eq!(
        inv,
        vec![
            Inval::Entry {
                parent: ROOT_INO,
                name: "work".into()
            },
            Inval::Inode(ROOT_INO)
        ]
    );
    assert_eq!(c.lookup(ROOT_INO, os("work")), Err(libc::ENOENT));
    assert!(c.list().is_empty());

    // kernel still holds refs: everything it knows keeps working
    assert_eq!(c.read(fh, 0, 100).unwrap(), b"hello");
    assert_eq!(c.getattr(file).unwrap().size, 5);
    c.release(fh);

    // walk() looked up /work, /work/proj, a.txt once each: forget them all
    let mut kernel_refs = vec![];
    for n in c.inode_list_for_tests() {
        if n != ROOT_INO {
            kernel_refs.push(n);
        }
    }
    for n in kernel_refs {
        c.forget(n, 1);
    }
    assert_eq!(c.inode_count(), base);
    assert_eq!(c.getattr(file).err(), Some(libc::ESTALE));
}

#[test]
fn lookups_are_refcounted_and_hardlinks_deduplicated() {
    let h = host();
    fs::hard_link(h.path().join("a.txt"), h.path().join("a2.txt")).unwrap();
    let c = core();
    add(&c, "/p", h.path(), false);
    let p = ino(&c, "/p");
    let a = c.lookup(p, os("a.txt")).unwrap().ino.0;
    assert_eq!(c.lookup(p, os("a.txt")).unwrap().ino.0, a);
    assert_eq!(c.lookup(p, os("a2.txt")).unwrap().ino.0, a);
    c.forget(a, 2);
    assert!(c.getattr(a).is_ok(), "one reference left");
    c.forget(a, 1);
    assert_eq!(c.getattr(a).err(), Some(libc::ESTALE));
    // fresh lookup gets a fresh inode number (never reused)
    assert_ne!(c.lookup(p, os("a.txt")).unwrap().ino.0, a);
}

#[test]
fn implicit_dirs_are_pruned_explicit_ones_kept() {
    let h = host();
    let c = core();
    c.mkdir_virtual(Path::new("/mnt")).unwrap();
    add(&c, "/mnt/a/b", h.path(), false);
    let mnt = ino(&c, "/mnt");
    let inv = c.remove(Path::new("/mnt/a/b"), false).unwrap();
    assert_eq!(
        inv[0],
        Inval::Entry {
            parent: mnt,
            name: "a".into()
        }
    );
    assert_eq!(
        c.list(),
        vec![ListEntry::Dir {
            target: "/mnt".into()
        }]
    );
    assert_eq!(names(&c, mnt), [".", ".."]);
}

#[test]
fn control_plane_validation() {
    let h = host();
    let c = core();
    add(&c, "/work", h.path(), false);
    let e = c
        .add_link(Path::new("/work/x"), h.path(), false, false)
        .unwrap_err();
    assert!(e.contains("is a link"), "{e}");
    let e = c
        .add_link(Path::new("/work"), h.path(), false, false)
        .unwrap_err();
    assert!(e.contains("already exists"), "{e}");
    assert!(c.add_link(Path::new("/"), h.path(), false, false).is_err());
    assert!(
        c.add_link(Path::new("rel"), h.path(), false, false)
            .is_err()
    );
    assert!(
        c.add_link(Path::new("/a/../b"), h.path(), false, false)
            .is_err()
    );
    assert!(
        c.add_link(Path::new("/x"), Path::new("relative"), false, false)
            .is_err()
    );
    assert!(
        c.add_link(Path::new("/x"), &h.path().join("missing"), false, false)
            .is_err()
    );
    assert!(c.mkdir_virtual(Path::new("/work/sub")).is_err());
    assert!(c.mkdir_virtual(Path::new("/work")).is_err());

    // replace swaps the link (and its ro flag)
    let h2 = host();
    c.add_link(Path::new("/work"), h2.path(), true, true)
        .unwrap();
    assert_eq!(
        c.list(),
        vec![ListEntry::Link {
            target: "/work".into(),
            source: h2.path().into(),
            ro: true
        }]
    );

    // non-empty virtual dirs need recursive removal
    add(&c, "/deep/er/link", h.path(), false);
    c.mkdir_virtual(Path::new("/deep/keep")).unwrap();
    assert!(
        c.remove(Path::new("/deep"), false)
            .unwrap_err()
            .contains("non-empty")
    );
    c.remove(Path::new("/deep"), true).unwrap();
    assert_eq!(c.lookup(ROOT_INO, os("deep")), Err(libc::ENOENT));
    assert!(
        c.remove(Path::new("/deep"), true)
            .unwrap_err()
            .contains("no such")
    );
    assert!(c.remove(Path::new("/"), true).is_err());
}

#[test]
fn only_user_xattrs_pass_through() {
    let h = host();
    let c = core();
    add(&c, "/p", h.path(), false);
    let f = ino(&c, "/p/a.txt");
    match c.setxattr(f, os("user.coffee"), b"black", 0) {
        Err(libc::EOPNOTSUPP) => return, // host fs without user xattrs
        r => r.unwrap(),
    }
    assert_eq!(c.getxattr(f, os("user.coffee")).unwrap(), b"black");
    assert_eq!(c.listxattr(f).unwrap(), b"user.coffee\0");
    assert_eq!(c.setxattr(f, os("trusted.x"), b"1", 0), Err(libc::EPERM));
    assert_eq!(
        c.setxattr(f, os("security.capability"), b"1", 0),
        Err(libc::EPERM)
    );
    assert_eq!(c.getxattr(f, os("security.selinux")), Err(libc::ENODATA));
    c.removexattr(f, os("user.coffee")).unwrap();
    assert_eq!(c.getxattr(f, os("user.coffee")), Err(libc::ENODATA));
    assert_eq!(c.getxattr(ROOT_INO, os("user.coffee")), Err(libc::ENODATA));
}

#[test]
fn state_file_roundtrip() {
    let h = host();
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("state.json");
    let c1 = Arc::new(core());
    let ctl1 = Controller::new(c1.clone(), Box::new(NoInval), Some(state.clone()));
    ctl1.apply(Request::Mkdir {
        target: "/tmp".into(),
    })
    .unwrap();
    ctl1.apply(Request::Add {
        source: h.path().into(),
        target: "/w/p".into(),
        ro: true,
        replace: false,
    })
    .unwrap();
    ctl1.apply(Request::Add {
        source: h.path().join("a.txt"),
        target: "/f".into(),
        ro: false,
        replace: false,
    })
    .unwrap();

    let c2 = Arc::new(core());
    Controller::new(c2.clone(), Box::new(NoInval), Some(state.clone()))
        .start(vec![])
        .unwrap();
    assert_eq!(c1.list(), c2.list());
    assert_eq!(read_all(&c2, ino(&c2, "/w/p/a.txt")), b"hello");

    // stale sources are skipped on load rather than failing startup
    fs::remove_file(h.path().join("a.txt")).unwrap();
    let c3 = Arc::new(core());
    Controller::new(c3.clone(), Box::new(NoInval), Some(state))
        .start(vec![])
        .unwrap();
    assert_eq!(c3.list().len(), 2);
}

#[test]
fn control_socket_protocol() {
    let h = host();
    let dir = TempDir::new().unwrap();
    let sock = dir.path().join("i.sock");
    let c = Arc::new(core());
    let ctl_ = Arc::new(Controller::new(c.clone(), Box::new(NoInval), None));
    let l = ctl::bind(&sock).unwrap();
    assert_eq!(
        fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::thread::spawn(move || ctl::serve(ctl_, l));

    let add = Request::Add {
        source: h.path().into(),
        target: "/x".into(),
        ro: false,
        replace: false,
    };
    assert!(ctl::call(&sock, &add).unwrap().ok);
    let again = ctl::call(&sock, &add).unwrap();
    assert!(!again.ok && again.error.unwrap().contains("exists"));
    let ls = ctl::call(&sock, &Request::Ls).unwrap();
    assert_eq!(ls.entries.unwrap().len(), 1);
    assert!(
        ctl::call(
            &sock,
            &Request::Rm {
                target: "/x".into(),
                recursive: false,
                pending: false,
            }
        )
        .unwrap()
        .ok
    );
    assert!(c.list().is_empty());

    // raw protocol, several requests on one connection, garbage tolerated
    use std::io::{BufRead, BufReader, Write};
    let s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    let mut w = &s;
    writeln!(
        w,
        "not json\n{{\"op\":\"mkdir\",\"target\":\"/m\"}}\n{{\"op\":\"ls\"}}"
    )
    .unwrap();
    let mut lines = BufReader::new(&s).lines();
    assert!(lines.next().unwrap().unwrap().contains("bad request"));
    assert_eq!(lines.next().unwrap().unwrap(), r#"{"ok":true}"#);
    assert_eq!(
        lines.next().unwrap().unwrap(),
        r#"{"ok":true,"entries":[{"kind":"dir","target":"/m","origin":"runtime"}]}"#
    );

    // a second instance must not steal a live socket
    assert_eq!(
        ctl::bind(&sock).unwrap_err().kind(),
        std::io::ErrorKind::AddrInUse
    );
}

#[test]
fn stale_socket_is_replaced() {
    let dir = TempDir::new().unwrap();
    let sock: PathBuf = dir.path().join("s.sock");
    drop(ctl::bind(&sock).unwrap()); // listener gone, file remains
    assert!(sock.exists());
    ctl::bind(&sock).unwrap();
}

#[test]
fn files_keep_host_ownership() {
    let h = host();
    let c = core();
    add(&c, "/p", h.path(), false);
    let a = walk(&c, "/p/a.txt").unwrap();
    let md = fs::metadata(h.path().join("a.txt")).unwrap();
    assert_eq!((a.uid, a.gid, a.size), (md.uid(), md.gid(), md.size()));
}

#[test]
fn created_objects_get_the_requested_mode_whatever_the_process_umask() {
    let h = host();
    let c = core();
    add(&c, "/p", h.path(), false);
    let dir = ino(&c, "/p");
    // a restrictive umask on the calling thread must not leak into objects
    // created for a request: the kernel already applied the caller's umask
    with_umask(0o077, || {
        let (_, fh) = c.create(dir, os("f"), 0o666, WR, caller(0o002)).unwrap();
        c.release(fh);
        c.mkdir(dir, os("d"), 0o1775, caller(0o002)).unwrap();
        c.mknod(dir, os("p"), libc::S_IFIFO | 0o666, caller(0o022))
            .unwrap();
    });
    assert_eq!(mode_of(&h.path().join("f")), 0o664);
    assert_eq!(mode_of(&h.path().join("d")), 0o1775);
    assert_eq!(mode_of(&h.path().join("p")), 0o644);
}

#[test]
fn state_file_is_private_and_written_fresh() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("s.json");
    // a leftover (or planted) tmp file must not be reused or followed
    let victim = dir.path().join("victim");
    fs::write(&victim, "keep").unwrap();
    symlink(&victim, dir.path().join("s.tmp")).unwrap();
    with_umask(0, || {
        let c = Arc::new(core());
        Controller::new(c, Box::new(NoInval), Some(state.clone()))
            .apply(Request::Mkdir {
                target: "/x".into(),
            })
            .unwrap();
    });
    assert_eq!(mode_of(&state), 0o600);
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
    assert!(!dir.path().join("s.tmp").exists());
}

#[test]
fn state_file_writable_by_others_is_refused() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("s.json");
    fs::write(&state, r#"[{"kind":"dir","target":"/x"}]"#).unwrap();
    let load =
        || Controller::new(Arc::new(core()), Box::new(NoInval), Some(state.clone())).start(vec![]);
    for bad in [0o666, 0o620, 0o602] {
        fs::set_permissions(&state, fs::Permissions::from_mode(bad)).unwrap();
        let e = load().unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{bad:o}");
    }
    fs::set_permissions(&state, fs::Permissions::from_mode(0o644)).unwrap();
    load().unwrap();
}

#[test]
fn mountpoints_are_created_0755_and_symlinks_refused() {
    let dir = TempDir::new().unwrap();
    let mp = dir.path().join("containers/choom1");
    with_umask(0, || crate::mount::prepare_mountpoint(&mp)).unwrap();
    assert_eq!(mode_of(&dir.path().join("containers")), 0o755);
    assert_eq!(mode_of(&mp), 0o755);
    // existing dir: fine
    crate::mount::prepare_mountpoint(&mp).unwrap();
    // a symlink in its place (pointing at a real dir) is not
    let evil = dir.path().join("containers/choom2");
    symlink(dir.path(), &evil).unwrap();
    let e = crate::mount::prepare_mountpoint(&evil).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::NotADirectory);
}

fn ls(ctl: &Controller) -> Vec<Entry> {
    ctl.handle(Request::Ls).entries.unwrap()
}

fn link(target: &str, source: &Path) -> ListEntry {
    ListEntry::Link {
        target: target.into(),
        source: source.into(),
        ro: false,
    }
}

fn dir(target: &str) -> ListEntry {
    ListEntry::Dir {
        target: target.into(),
    }
}

fn saved(state: &Path) -> Vec<ListEntry> {
    serde_json::from_slice(&fs::read(state).unwrap()).unwrap()
}

#[test]
fn config_entries_are_applied_first_and_never_persisted() {
    let h = host();
    let d = TempDir::new().unwrap();
    let state = d.path().join("s.json");
    let config = vec![dir("/cfg"), link("/cfglink", h.path())];
    let ctl = |config: Vec<ListEntry>| {
        let c = Controller::new(Arc::new(core()), Box::new(NoInval), Some(state.clone()));
        c.start(config).unwrap();
        c
    };

    let c1 = ctl(config.clone());
    c1.apply(Request::Add {
        source: h.path().into(),
        target: "/rt".into(),
        ro: false,
        replace: false,
    })
    .unwrap();
    assert_eq!(saved(&state), vec![link("/rt", h.path())]);
    let origins: Vec<_> = ls(&c1)
        .iter()
        .map(|e| (e.entry.clone(), e.origin))
        .collect();
    assert_eq!(
        origins,
        vec![
            (dir("/cfg"), Origin::Config),
            (link("/cfglink", h.path()), Origin::Config),
            (link("/rt", h.path()), Origin::Runtime),
        ]
    );

    // dropping an entry from the config revokes it on the next start
    let c2 = ctl(vec![dir("/cfg")]);
    let live: Vec<_> = ls(&c2).into_iter().map(|e| e.entry).collect();
    assert_eq!(live, vec![dir("/cfg"), link("/rt", h.path())]);

    // state files from older versions also hold config entries: no conflict
    fs::write(
        &state,
        serde_json::to_vec(&[dir("/cfg"), link("/rt", h.path())]).unwrap(),
    )
    .unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
    let c3 = ctl(vec![dir("/cfg")]);
    assert!(ls(&c3).iter().all(|e| e.pending.is_none()));
}

#[test]
fn conflicts_with_the_config_become_pending_instead_of_failing_startup() {
    let h = host();
    let d = TempDir::new().unwrap();
    let state = d.path().join("s.json");
    // runtime state that conflicts with what the config now says
    fs::write(
        &state,
        serde_json::to_vec(&[link("/x", h.path()), link("/a/b", h.path())]).unwrap(),
    )
    .unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
    let c = Controller::new(Arc::new(core()), Box::new(NoInval), Some(state.clone()));
    c.start(vec![dir("/x"), link("/a", h.path())]).unwrap();

    let entries = ls(&c);
    let pending: Vec<_> = entries
        .iter()
        .filter_map(|e| Some((e.entry.target().to_owned(), e.pending.clone()?)))
        .collect();
    assert_eq!(pending.len(), 2, "{entries:?}");
    assert!(pending[0].1.contains("already exists"), "{pending:?}");
    assert!(pending[1].1.contains("is a link"), "{pending:?}");
    // config won, and the runtime entries are kept for later
    c.apply(Request::Mkdir {
        target: "/other".into(),
    })
    .unwrap();
    assert_eq!(
        saved(&state),
        vec![dir("/other"), link("/x", h.path()), link("/a/b", h.path())]
    );
}

#[test]
fn unavailable_sources_stay_pending_and_are_retried() {
    let base = TempDir::new().unwrap();
    let src = base.path().join("late");
    let away = base.path().join("away");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), "late").unwrap();
    let d = TempDir::new().unwrap();
    let state = d.path().join("s.json");
    let new = || Controller::new(Arc::new(core()), Box::new(NoInval), Some(state.clone()));

    let c1 = new();
    c1.start(vec![]).unwrap();
    c1.apply(Request::Add {
        source: src.clone(),
        target: "/late".into(),
        ro: false,
        replace: false,
    })
    .unwrap();

    // e.g. the filesystem holding the source isn't mounted yet at boot
    fs::rename(&src, &away).unwrap();
    let c2 = new();
    c2.start(vec![]).unwrap();
    let e = ls(&c2);
    assert_eq!(e.len(), 1);
    assert!(e[0].pending.as_deref().unwrap().contains("No such file"));
    assert_eq!(c2.retry(), 0);
    // other changes don't drop it from the state file
    c2.apply(Request::Mkdir {
        target: "/m".into(),
    })
    .unwrap();
    assert_eq!(saved(&state), vec![dir("/m"), link("/late", &src)]);

    fs::rename(&away, &src).unwrap();
    assert_eq!(c2.retry(), 1);
    assert!(ls(&c2).iter().all(|e| e.pending.is_none()));

    // a pending entry can be removed like a live one
    fs::rename(&src, &away).unwrap();
    let c3 = new();
    c3.start(vec![]).unwrap();
    c3.apply(Request::Rm {
        target: "/late".into(),
        recursive: false,
        pending: false,
    })
    .unwrap();
    assert_eq!(saved(&state), vec![dir("/m")]);
}

#[test]
fn corrupt_state_file_is_moved_aside() {
    let d = TempDir::new().unwrap();
    let state = d.path().join("s.json");
    fs::write(&state, "[{\"kind\":\"li").unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
    let c = Controller::new(Arc::new(core()), Box::new(NoInval), Some(state.clone()));
    c.start(vec![dir("/cfg")]).unwrap();
    assert_eq!(ls(&c).len(), 1);
    assert!(!state.exists());
    let aside: Vec<_> = fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(aside[0].starts_with("s.corrupt-"), "{aside:?}");
}

#[test]
fn link_arguments_split_at_the_last_colon_slash() {
    let l = |s| match crate::serve::parse_link(s, false).unwrap() {
        ListEntry::Link { source, target, .. } => (source, target),
        _ => unreachable!(),
    };
    let p = PathBuf::from;
    // no target: at /NAME
    assert_eq!(l("/srv/a"), (p("/srv/a"), p("/a")));
    assert_eq!(l("/srv/a:/x"), (p("/srv/a"), p("/x")));
    assert_eq!(l("/srv/a:b:/x"), (p("/srv/a:b"), p("/x")));
    assert_eq!(l("/srv/a:b"), (p("/srv/a:b"), p("/a:b")));
}

#[test]
fn fsyncdir_syncs_host_dirs_and_ignores_virtual_ones() {
    let h = host();
    let c = core();
    add(&c, "/v/p", h.path(), false);
    c.fsyncdir(ino(&c, "/v"), false).unwrap();
    c.fsyncdir(ino(&c, "/v/p"), true).unwrap();
    c.fsyncdir(ino(&c, "/v/p/sub"), false).unwrap();
}

fn rm(target: &str, recursive: bool, pending: bool) -> Request {
    Request::Rm {
        target: target.into(),
        recursive,
        pending,
    }
}

fn live(ctl: &Controller) -> Vec<ListEntry> {
    ls(ctl)
        .into_iter()
        .filter(|e| e.pending.is_none())
        .map(|e| e.entry)
        .collect()
}

#[test]
fn reload_applies_config_changes_and_leaves_runtime_entries_alone() {
    let (h, h2) = (host(), host());
    let d = TempDir::new().unwrap();
    let state = d.path().join("s.json");
    let cr = Arc::new(core());
    let c = Controller::new(cr.clone(), Box::new(NoInval), Some(state.clone()));
    c.start(vec![
        dir("/a"),
        link("/a/x", h.path()),
        link("/y", h.path()),
        dir("/keep"),
    ])
    .unwrap();
    c.apply(Request::Add {
        source: h.path().into(),
        target: "/keep/rt".into(),
        ro: false,
        replace: false,
    })
    .unwrap();

    // /a/x and /keep dropped, /y now points elsewhere, /new added
    c.reload(vec![
        dir("/a"),
        link("/y", h2.path()),
        link("/new", h.path()),
    ]);
    assert_eq!(
        live(&c),
        vec![
            dir("/a"),
            link("/keep/rt", h.path()),
            link("/new", h.path()),
            link("/y", h2.path()),
        ]
    );
    // /keep stays (implicitly) while the runtime link below it does...
    assert!(walk(&cr, "/keep").is_ok());
    c.apply(rm("/keep/rt", false, false)).unwrap();
    // ...and goes with it
    assert_eq!(walk(&cr, "/keep").err(), Some(libc::ENOENT));
    assert_eq!(saved(&state), Vec::<ListEntry>::new());
}

#[test]
fn reload_unblocks_runtime_entries_the_old_config_was_in_the_way_of() {
    let h = host();
    let d = TempDir::new().unwrap();
    let state = d.path().join("s.json");
    fs::write(&state, serde_json::to_vec(&[link("/c", h.path())]).unwrap()).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
    let c = Controller::new(Arc::new(core()), Box::new(NoInval), Some(state.clone()));
    c.start(vec![dir("/c")]).unwrap();
    assert_eq!(ls(&c).iter().filter(|e| e.pending.is_some()).count(), 1);

    c.reload(vec![]);
    c.retry();
    assert_eq!(live(&c), vec![link("/c", h.path())]);
    assert!(ls(&c).iter().all(|e| e.pending.is_none()));
}

#[test]
fn rm_pending_only_drops_the_pending_entry() {
    let h = host();
    let d = TempDir::new().unwrap();
    let state = d.path().join("s.json");
    fs::write(&state, serde_json::to_vec(&[link("/c", h.path())]).unwrap()).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
    let c = Controller::new(Arc::new(core()), Box::new(NoInval), Some(state.clone()));
    c.start(vec![dir("/c")]).unwrap();

    c.apply(rm("/c", false, true)).unwrap();
    assert_eq!(live(&c), vec![dir("/c")]);
    assert!(ls(&c).iter().all(|e| e.pending.is_none()));
    let e = c.apply(rm("/c", false, true)).unwrap_err();
    assert!(e.contains("nothing pending"), "{e}");
}

#[test]
fn status_reports_entries_pending_and_inodes() {
    let h = host();
    let c = Controller::new(Arc::new(core()), Box::new(NoInval), None);
    c.start(vec![
        link("/x", h.path()),
        link("/gone", &h.path().join("nope")),
    ])
    .unwrap();
    let s = c.handle(Request::Status).status.unwrap();
    assert_eq!((s.entries, s.pending), (1, 1));
    assert_eq!(s.inodes, 2); // root + /x
    assert_eq!(s.open_handles, 0);
    assert!(!s.io.passthrough);
}

// ---- root: the directory everything no link covers falls through to ----

fn set_root(c: &Core, r: &Path, ro: bool) -> Vec<Inval> {
    c.set_root(r, ro, false).unwrap()
}

/// root layout: host() plus work/old.txt
fn root_dir() -> TempDir {
    let r = host();
    fs::create_dir(r.path().join("work")).unwrap();
    fs::write(r.path().join("work/old.txt"), "old").unwrap();
    r
}

#[test]
fn root_falls_through_and_links_cover_it() {
    let (r, l) = (root_dir(), host());
    fs::write(l.path().join("only-in-link"), "l").unwrap();
    let c = core();
    let inv = set_root(&c, r.path(), false);
    assert!(inv.contains(&Inval::Inode(ROOT_INO)), "{inv:?}");

    // / is the root directory: its entries, its attributes
    assert_eq!(names(&c, ROOT_INO), [".", "..", "a.txt", "sub", "work"]);
    assert_eq!(read_all(&c, ino(&c, "/a.txt")), b"hello");
    assert_eq!(read_all(&c, ino(&c, "/sub/b.txt")), b"bee");
    let a = c.getattr(ROOT_INO).unwrap();
    assert_eq!(u32::from(a.perm), mode_of(r.path()));
    assert_eq!(a.uid, fs::metadata(r.path()).unwrap().uid());

    // a link covers the root's entry of the same name
    add(&c, "/sub", l.path(), false);
    assert_eq!(names(&c, ROOT_INO), [".", "..", "a.txt", "sub", "work"]);
    assert_eq!(
        names(&c, ino(&c, "/sub")),
        [".", "..", "a.txt", "only-in-link", "sub"]
    );
    assert_eq!(walk(&c, "/sub/b.txt").err(), Some(libc::ENOENT));

    // a link further down leaves the rest of the root's directory visible
    add(&c, "/work/proj", l.path(), false);
    let work = ino(&c, "/work");
    assert_eq!(names(&c, work), [".", "..", "old.txt", "proj"]);
    assert_eq!(read_all(&c, ino(&c, "/work/old.txt")), b"old");
    assert_eq!(read_all(&c, ino(&c, "/work/proj/only-in-link")), b"l");

    // writes outside the links land in the root, in / and in /work alike
    let (_, fh) = c
        .create(ROOT_INO, os("new.txt"), 0o644, WR, caller(0))
        .unwrap();
    c.write(fh, 0, b"new").unwrap();
    c.release(fh);
    assert_eq!(fs::read(r.path().join("new.txt")).unwrap(), b"new");
    c.mkdir(work, os("made"), 0o755, caller(0)).unwrap();
    assert!(r.path().join("work/made").is_dir());
    let chmod = SetAttr {
        mode: Some(0o711),
        ..Default::default()
    };
    c.setattr(work, &chmod, None).unwrap();
    assert_eq!(mode_of(&r.path().join("work")), 0o711);

    // within the root, renames work across directories; into a link they don't
    c.rename((ROOT_INO, os("new.txt")), (work, os("new.txt")), 0)
        .unwrap();
    assert!(r.path().join("work/new.txt").is_file());
    let proj = ino(&c, "/work/proj");
    assert_eq!(
        c.rename((work, os("new.txt")), (proj, os("x")), 0),
        Err(libc::EXDEV)
    );

    // what the tree covers can't be touched from inside: not the link, and
    // not what is under it in the root
    assert_eq!(c.rmdir(ROOT_INO, os("sub")), Err(libc::EBUSY));
    assert_eq!(c.unlink(work, os("proj")), Err(libc::EBUSY));
    assert_eq!(
        c.rename((ROOT_INO, os("a.txt")), (ROOT_INO, os("sub")), 0),
        Err(libc::EBUSY)
    );
    assert_eq!(
        c.rename((ROOT_INO, os("work")), (ROOT_INO, os("w2")), 0),
        Err(libc::EBUSY)
    );
    assert_eq!(
        c.mkdir(work, os("proj"), 0o755, caller(0)).err(),
        Some(libc::EEXIST)
    );
    assert!(r.path().join("sub/b.txt").is_file());

    // links gone: the root shows through again, and /work is plain passthrough
    c.remove(Path::new("/sub"), false).unwrap();
    assert_eq!(read_all(&c, ino(&c, "/sub/b.txt")), b"bee");
    c.remove(Path::new("/work/proj"), false).unwrap();
    assert_eq!(
        names(&c, ino(&c, "/work")),
        [".", "..", "made", "new.txt", "old.txt"]
    );
    c.unlink(ino(&c, "/work"), os("new.txt")).unwrap();
    assert!(!r.path().join("work/new.txt").exists());
}

#[test]
fn root_can_be_read_only_replaced_and_cleared() {
    let (r, l) = (root_dir(), host());
    let c = core();
    add(&c, "/work/proj", l.path(), false);
    set_root(&c, r.path(), true);
    // a root set after the links reaches the virtual dirs they made, too
    let work = ino(&c, "/work");
    assert_eq!(names(&c, work), [".", "..", "old.txt", "proj"]);
    assert_eq!(
        c.list(),
        vec![
            ListEntry::Root {
                source: r.path().into(),
                ro: true
            },
            link("/work/proj", l.path()),
        ]
    );

    // read-only: nowhere in the root, but a read-write link still is
    for parent in [ROOT_INO, work] {
        assert_eq!(
            c.create(parent, os("f"), 0o644, WR, caller(0)).err(),
            Some(libc::EROFS)
        );
    }
    assert_eq!(c.open(ino(&c, "/a.txt"), WR).err(), Some(libc::EROFS));
    let chmod = SetAttr {
        mode: Some(0o777),
        ..Default::default()
    };
    assert_eq!(c.setattr(ROOT_INO, &chmod, None).err(), Some(libc::EROFS));
    let proj = ino(&c, "/work/proj");
    let (_, fh) = c.create(proj, os("f"), 0o644, WR, caller(0)).unwrap();
    c.release(fh);

    // one root at a time, unless replaced
    let e = c.set_root(l.path(), false, false).unwrap_err();
    assert!(e.contains("already set"), "{e}");
    c.set_root(r.path(), false, true).unwrap();
    let (_, fh) = c.create(ROOT_INO, os("f"), 0o644, WR, caller(0)).unwrap();
    c.release(fh);
    assert!(r.path().join("f").is_file());

    // clearing it: back to the bare skeleton; open files keep working
    let fh = c.open(ino(&c, "/a.txt"), RD).unwrap();
    let inv = c.remove(Path::new("/"), false).unwrap();
    assert!(
        inv.contains(&Inval::Entry {
            parent: ROOT_INO,
            name: "a.txt".into()
        }),
        "{inv:?}"
    );
    assert_eq!(c.lookup(ROOT_INO, os("a.txt")), Err(libc::ENOENT));
    assert_eq!(c.getattr(ROOT_INO).unwrap().perm, 0o555);
    assert_eq!(names(&c, ino(&c, "/work")), [".", "..", "proj"]);
    assert_eq!(c.read(fh, 0, 100).unwrap(), b"hello");
    c.release(fh);
    assert!(
        c.remove(Path::new("/"), false)
            .unwrap_err()
            .contains("no root")
    );
    assert!(c.set_root(&r.path().join("a.txt"), false, false).is_err());
    assert!(c.add_link(Path::new("/"), r.path(), false, false).is_err());
}

#[test]
fn root_symlinks_never_become_backing() {
    let (r, outside) = (host(), host());
    symlink(outside.path(), r.path().join("work")).unwrap();
    let c = core();
    set_root(&c, r.path(), false);
    // as an entry it's a symlink, for the container to resolve
    assert_eq!(walk(&c, "/work").unwrap().kind, FileType::Symlink);
    // but a virtual /work doesn't fall through to where it points
    add(&c, "/work/proj", r.path(), false);
    assert_eq!(names(&c, ino(&c, "/work")), [".", "..", "proj"]);
    assert_eq!(walk(&c, "/work/a.txt").err(), Some(libc::ENOENT));
    assert_eq!(
        c.create(ino(&c, "/work"), os("f"), 0o644, WR, caller(0))
            .err(),
        Some(libc::EROFS)
    );
}

fn root(source: &Path, ro: bool) -> ListEntry {
    ListEntry::Root {
        source: source.into(),
        ro,
    }
}

#[test]
fn root_in_the_control_plane() {
    let (r, r2, h) = (root_dir(), host(), host());
    let d = TempDir::new().unwrap();
    let state = d.path().join("s.json");
    let ctl = |config: Vec<ListEntry>| {
        let cr = Arc::new(core());
        let c = Controller::new(cr.clone(), Box::new(NoInval), Some(state.clone()));
        c.start(config).unwrap();
        (c, cr)
    };

    // set at runtime: persisted and restored
    let (c1, _) = ctl(vec![]);
    c1.apply(Request::Root {
        source: r.path().into(),
        ro: false,
        replace: false,
    })
    .unwrap();
    c1.apply(Request::Add {
        source: h.path().into(),
        target: "/proj".into(),
        ro: false,
        replace: false,
    })
    .unwrap();
    assert_eq!(
        saved(&state),
        vec![root(r.path(), false), link("/proj", h.path())]
    );
    let (c2, cr2) = ctl(vec![]);
    assert_eq!(
        live(&c2),
        vec![root(r.path(), false), link("/proj", h.path())]
    );
    assert_eq!(read_all(&cr2, ino(&cr2, "/work/old.txt")), b"old");

    // a root in the config wins; the runtime one waits (next to a link
    // whose source is missing)
    let missing = link("/later", &h.path().join("missing"));
    let mut st = saved(&state);
    st.push(missing.clone());
    fs::write(&state, serde_json::to_vec(&st).unwrap()).unwrap();
    let (c3, cr3) = ctl(vec![root(r2.path(), true)]);
    let pending: Vec<_> = ls(&c3)
        .into_iter()
        .filter(|e| e.pending.is_some())
        .collect();
    assert_eq!(pending.len(), 2, "{pending:?}");
    assert_eq!(pending[0].entry, root(r.path(), false));
    assert_eq!(walk(&cr3, "/work").err(), Some(libc::ENOENT));
    // rm -r / only means the root: the pending root goes, nothing else
    c3.apply(rm("/", true, true)).unwrap();
    assert_eq!(saved(&state), vec![link("/proj", h.path()), missing]);

    // reload swaps the configured root in place
    c3.reload(vec![root(r.path(), false)]);
    assert_eq!(live(&c3)[0], root(r.path(), false));
    assert_eq!(read_all(&cr3, ino(&cr3, "/work/old.txt")), b"old");
    c3.reload(vec![]);
    assert_eq!(live(&c3), vec![link("/proj", h.path())]);
    assert_eq!(walk(&cr3, "/work").err(), Some(libc::ENOENT));
}

#[test]
fn root_and_config_arguments() {
    use clap::Parser;
    #[derive(Parser)]
    struct A {
        #[command(flatten)]
        a: crate::ServeArgs,
    }
    let parse = |args: &[&str]| {
        let a = A::try_parse_from(["x", "box"].iter().chain(args)).unwrap();
        crate::serve::config_entries(&a.a)
    };
    assert_eq!(
        parse(&["--root-ro", "/srv/r", "--link", "/srv/a"]).unwrap(),
        vec![
            root(Path::new("/srv/r"), true),
            link("/a", Path::new("/srv/a"))
        ]
    );
    let d = TempDir::new().unwrap();
    let cfg = d.path().join("c.json");
    fs::write(
        &cfg,
        serde_json::to_vec(&[root(Path::new("/srv/x"), false)]).unwrap(),
    )
    .unwrap();
    fs::set_permissions(&cfg, fs::Permissions::from_mode(0o600)).unwrap();
    let e = parse(&["--root", "/srv/r", "--config", cfg.to_str().unwrap()]).unwrap_err();
    assert!(e.contains("more than one root"), "{e}");
    assert!(A::try_parse_from(["x", "box", "--root", "/a", "--root-ro", "/b"]).is_err());
    assert!(crate::default_target(Path::new("/")).is_err());
    assert_eq!(
        crate::default_target(Path::new("/home/annika/project")).unwrap(),
        Path::new("/project")
    );
}
