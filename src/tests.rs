//! Core + control-plane tests. These drive `Core` directly (no kernel
//! involved), so they run unprivileged and without /dev/fuse. The FUSE/kernel
//! side is covered by the NixOS VM test in nix/test.nix.

use crate::core::*;
use crate::ctl::{self, Controller, NoInval, Request};
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
        c.create(dir, os("n"), 0o644, 0, WR, me()).err(),
        Some(libc::EROFS)
    );
    assert_eq!(
        c.mkdir(dir, os("d"), 0o755, 0, me()).err(),
        Some(libc::EROFS)
    );
    assert_eq!(c.unlink(dir, os("a.txt")), Err(libc::EROFS));
    assert_eq!(c.rmdir(dir, os("sub")), Err(libc::EROFS));
    assert_eq!(
        c.rename(dir, os("a.txt"), dir, os("z"), 0),
        Err(libc::EROFS)
    );
    assert_eq!(
        c.symlink(dir, os("l"), Path::new("x"), me()).err(),
        Some(libc::EROFS)
    );
    let chmod = SetAttr {
        mode: Some(0o600),
        ..Default::default()
    };
    assert_eq!(c.setattr(file, chmod, None).err(), Some(libc::EROFS));
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
        .create(dir, os("new.txt"), 0o664, 0o002, libc::O_RDWR, me())
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
    let (_, fh) = c.create(dir, os("masked"), 0o666, 0o027, WR, me()).unwrap();
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
    let d = c.mkdir(dir, os("d"), 0o750, 0, me()).unwrap();
    assert_eq!(d.kind, FileType::Directory);
    assert!(h.path().join("d").is_dir());
    let l = c
        .symlink(dir, os("ln"), Path::new("new.txt"), me())
        .unwrap();
    assert_eq!(l.kind, FileType::Symlink);
    assert_eq!(c.readlink(l.ino.0).unwrap(), b"new.txt");

    // rename + hardlink (same inode via both names)
    c.rename(dir, os("new.txt"), d.ino.0, os("moved.txt"), 0)
        .unwrap();
    assert!(h.path().join("d/moved.txt").exists());
    assert_eq!(
        c.rename(
            dir,
            os("a.txt"),
            d.ino.0,
            os("moved.txt"),
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
    let after = c.setattr(moved.ino.0, sa, None).unwrap();
    assert_eq!((after.size, after.perm, after.mtime), (3, 0o600, t));
    assert_eq!(fs::read_to_string(h.path().join("hard")).unwrap(), "wri");

    // fallocate/lseek/copy_file_range via handles
    let src = c.open(moved.ino.0, RD).unwrap();
    let (_, dst) = c
        .create(dir, os("copy"), 0o644, 0, libc::O_RDWR, me())
        .unwrap();
    assert_eq!(c.copy_file_range(src, 0, dst, 0, 3, 0).unwrap(), 3);
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
        c.mknod(dir, os("dev"), libc::S_IFCHR | 0o666, 0, me())
            .err(),
        Some(libc::EPERM)
    );
    assert_eq!(
        c.mknod(dir, os("blk"), libc::S_IFBLK | 0o666, 0, me())
            .err(),
        Some(libc::EPERM)
    );
    let f = c
        .mknod(dir, os("fifo"), libc::S_IFIFO | 0o600, 0, me())
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
            c.create(parent, os("f"), 0o644, 0, WR, me()).err(),
            Some(libc::EROFS)
        );
        assert_eq!(
            c.mkdir(parent, os("d"), 0o755, 0, me()).err(),
            Some(libc::EROFS)
        );
        assert_eq!(
            c.symlink(parent, os("s"), Path::new("x"), me()).err(),
            Some(libc::EROFS)
        );
        assert_eq!(
            c.mknod(parent, os("p"), libc::S_IFIFO | 0o644, 0, me())
                .err(),
            Some(libc::EROFS)
        );
    }
    // links themselves can't be removed/renamed/replaced from inside
    assert_eq!(c.rmdir(work, os("proj")), Err(libc::EROFS));
    assert_eq!(c.unlink(work, os("proj")), Err(libc::EROFS));
    assert_eq!(
        c.rename(work, os("proj"), work, os("x"), 0),
        Err(libc::EROFS)
    );
    let proj = ino(&c, "/work/proj");
    assert_eq!(
        c.rename(proj, os("a.txt"), work, os("a.txt"), 0),
        Err(libc::EROFS)
    );
    let chmod = SetAttr {
        mode: Some(0o777),
        ..Default::default()
    };
    assert_eq!(c.setattr(work, chmod, None).err(), Some(libc::EROFS));
}

#[test]
fn ops_across_links_are_exdev() {
    let (h1, h2) = (host(), host());
    let c = core();
    add(&c, "/one", h1.path(), false);
    add(&c, "/two", h2.path(), false);
    let (one, two) = (ino(&c, "/one"), ino(&c, "/two"));
    assert_eq!(
        c.rename(one, os("a.txt"), two, os("x"), 0),
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
        c.create(dir, os("evilfile"), 0o644, 0, WR | libc::O_TRUNC, me())
            .err(),
        Some(libc::ELOOP)
    );
    let trunc = SetAttr {
        size: Some(0),
        ..Default::default()
    };
    assert_eq!(c.setattr(ef.ino.0, trunc, None).err(), Some(libc::EINVAL));
    let chmod = SetAttr {
        mode: Some(0o777),
        ..Default::default()
    };
    assert_eq!(
        c.setattr(ef.ino.0, chmod, None).err(),
        Some(libc::EOPNOTSUPP)
    );
    assert_eq!(
        c.setxattr(ef.ino.0, os("user.x"), b"1", 0),
        Err(libc::EPERM)
    );
    assert_eq!(
        c.mkdir(dir, os("evildir"), 0o755, 0, me()).err(),
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
        .load()
        .unwrap();
    assert_eq!(c1.list(), c2.list());
    assert_eq!(read_all(&c2, ino(&c2, "/w/p/a.txt")), b"hello");

    // stale sources are skipped on load rather than failing startup
    fs::remove_file(h.path().join("a.txt")).unwrap();
    let c3 = Arc::new(core());
    Controller::new(c3.clone(), Box::new(NoInval), Some(state))
        .load()
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
                recursive: false
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
        r#"{"ok":true,"entries":[{"kind":"dir","target":"/m"}]}"#
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
        let (_, fh) = c.create(dir, os("f"), 0o666, 0o002, WR, me()).unwrap();
        c.release(fh);
        c.mkdir(dir, os("d"), 0o1775, 0o002, me()).unwrap();
        c.mknod(dir, os("p"), libc::S_IFIFO | 0o666, 0o022, me())
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
    let load = || Controller::new(Arc::new(core()), Box::new(NoInval), Some(state.clone())).load();
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
