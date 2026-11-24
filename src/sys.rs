//! Thin, errno-returning wrappers around the *at() syscalls the passthrough
//! layer is built on. Everything works on file descriptors so that no host
//! path is ever re-resolved (and no symlink ever followed) on behalf of the
//! container.

use std::ffi::{CStr, CString, OsStr};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;

pub type Errno = i32;
pub type Res<T> = Result<T, Errno>;

pub fn errno() -> Errno {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

pub fn io_errno(e: &std::io::Error) -> Errno {
    e.raw_os_error().unwrap_or(libc::EIO)
}

pub fn strerror(e: Errno) -> String {
    std::io::Error::from_raw_os_error(e).to_string()
}

fn cvt(r: libc::c_int) -> Res<libc::c_int> {
    if r < 0 { Err(errno()) } else { Ok(r) }
}

fn cvt_ssize(r: libc::ssize_t) -> Res<usize> {
    if r < 0 { Err(errno()) } else { Ok(r as usize) }
}

/// A single directory-entry name as handed to us by the kernel. Rejects
/// anything that could walk out of the parent directory.
pub fn name(n: &OsStr) -> Res<CString> {
    let b = n.as_bytes();
    if b.is_empty() || b == b"." || b == b".." || b.contains(&b'/') {
        return Err(libc::EINVAL);
    }
    CString::new(b).map_err(|_| libc::EINVAL)
}

pub fn cstring(s: &OsStr) -> Res<CString> {
    CString::new(s.as_bytes()).map_err(|_| libc::EINVAL)
}

/// `/proc/self/fd/N` — lets us re-open / chmod / xattr an O_PATH fd without
/// ever resolving a real path again.
pub fn proc_path(fd: BorrowedFd<'_>) -> CString {
    CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap()
}

pub fn open(path: &CStr, flags: i32, mode: u32) -> Res<OwnedFd> {
    let fd =
        cvt(unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC, mode as libc::c_uint) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn openat(dir: BorrowedFd<'_>, name: &CStr, flags: i32, mode: u32) -> Res<OwnedFd> {
    let fd = cvt(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Re-open the object behind `fd` (usually an O_PATH fd) with new flags.
pub fn reopen(fd: BorrowedFd<'_>, flags: i32) -> Res<OwnedFd> {
    open(&proc_path(fd), flags, 0)
}

/// stat the object behind `fd` itself (never follows a symlink).
pub fn fstat(fd: BorrowedFd<'_>) -> Res<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::zeroed();
    cvt(unsafe {
        libc::fstatat(
            fd.as_raw_fd(),
            c"".as_ptr(),
            st.as_mut_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    Ok(unsafe { st.assume_init() })
}

pub fn fstatat_nofollow(dir: BorrowedFd<'_>, name: &CStr) -> Res<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::zeroed();
    cvt(unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    Ok(unsafe { st.assume_init() })
}

pub fn fstatvfs(fd: BorrowedFd<'_>) -> Res<libc::statvfs> {
    let mut st = MaybeUninit::<libc::statvfs>::zeroed();
    cvt(unsafe { libc::fstatvfs(fd.as_raw_fd(), st.as_mut_ptr()) })?;
    Ok(unsafe { st.assume_init() })
}

/// chown the object behind `fd` itself. `u32::MAX` leaves a field unchanged.
pub fn fchown(fd: BorrowedFd<'_>, uid: u32, gid: u32) -> Res<()> {
    cvt(unsafe {
        libc::fchownat(
            fd.as_raw_fd(),
            c"".as_ptr(),
            uid,
            gid,
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    Ok(())
}

pub fn fchmod(fd: BorrowedFd<'_>, mode: u32) -> Res<()> {
    cvt(unsafe { libc::fchmod(fd.as_raw_fd(), mode as libc::mode_t) })?;
    Ok(())
}

pub fn chmod(path: &CStr, mode: u32) -> Res<()> {
    cvt(unsafe { libc::chmod(path.as_ptr(), mode as libc::mode_t) })?;
    Ok(())
}

pub fn ftruncate(fd: BorrowedFd<'_>, size: u64) -> Res<()> {
    let size = libc::off_t::try_from(size).map_err(|_| libc::EFBIG)?;
    cvt(unsafe { libc::ftruncate(fd.as_raw_fd(), size) })?;
    Ok(())
}

pub fn utimens(fd: BorrowedFd<'_>, times: &[libc::timespec; 2]) -> Res<()> {
    cvt(unsafe {
        libc::utimensat(
            fd.as_raw_fd(),
            c"".as_ptr(),
            times.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    })?;
    Ok(())
}

pub fn readlink(fd: BorrowedFd<'_>) -> Res<Vec<u8>> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
    let n = cvt_ssize(unsafe {
        libc::readlinkat(
            fd.as_raw_fd(),
            c"".as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    })?;
    buf.truncate(n);
    Ok(buf)
}

pub fn mkdirat(dir: BorrowedFd<'_>, name: &CStr, mode: u32) -> Res<()> {
    cvt(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) })?;
    Ok(())
}

pub fn mknodat(dir: BorrowedFd<'_>, name: &CStr, mode: u32) -> Res<()> {
    cvt(unsafe { libc::mknodat(dir.as_raw_fd(), name.as_ptr(), mode as libc::mode_t, 0) })?;
    Ok(())
}

pub fn symlinkat(target: &CStr, dir: BorrowedFd<'_>, name: &CStr) -> Res<()> {
    cvt(unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), name.as_ptr()) })?;
    Ok(())
}

pub fn unlinkat(dir: BorrowedFd<'_>, name: &CStr, flags: i32) -> Res<()> {
    cvt(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) })?;
    Ok(())
}

pub fn renameat2(
    olddir: BorrowedFd<'_>,
    oldname: &CStr,
    newdir: BorrowedFd<'_>,
    newname: &CStr,
    flags: u32,
) -> Res<()> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            olddir.as_raw_fd(),
            oldname.as_ptr(),
            newdir.as_raw_fd(),
            newname.as_ptr(),
            flags,
        )
    };
    if r < 0 { Err(errno()) } else { Ok(()) }
}

/// Hard-link the object behind `fd` as `newdir/newname`. Goes through
/// `/proc/self/fd` so it works without CAP_DAC_READ_SEARCH.
pub fn link_fd(fd: BorrowedFd<'_>, newdir: BorrowedFd<'_>, newname: &CStr) -> Res<()> {
    let p = proc_path(fd);
    cvt(unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            p.as_ptr(),
            newdir.as_raw_fd(),
            newname.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    })?;
    Ok(())
}

fn xattr_buf(mut f: impl FnMut(*mut libc::c_void, usize) -> libc::ssize_t) -> Res<Vec<u8>> {
    loop {
        let size = cvt_ssize(f(std::ptr::null_mut(), 0))?;
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; size];
        match cvt_ssize(f(buf.as_mut_ptr().cast(), size)) {
            Ok(n) => {
                buf.truncate(n);
                return Ok(buf);
            }
            // grew between the two calls
            Err(libc::ERANGE) => continue,
            Err(e) => return Err(e),
        }
    }
}

pub fn getxattr(path: &CStr, name: &CStr) -> Res<Vec<u8>> {
    xattr_buf(|p, n| unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), p, n) })
}

pub fn listxattr(path: &CStr) -> Res<Vec<u8>> {
    xattr_buf(|p, n| unsafe { libc::listxattr(path.as_ptr(), p.cast(), n) })
}

pub fn setxattr(path: &CStr, name: &CStr, value: &[u8], flags: i32) -> Res<()> {
    cvt(unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            flags,
        )
    })?;
    Ok(())
}

pub fn removexattr(path: &CStr, name: &CStr) -> Res<()> {
    cvt(unsafe { libc::removexattr(path.as_ptr(), name.as_ptr()) })?;
    Ok(())
}

pub fn fallocate(fd: BorrowedFd<'_>, mode: i32, offset: u64, len: u64) -> Res<()> {
    let off = libc::off_t::try_from(offset).map_err(|_| libc::EFBIG)?;
    let len = libc::off_t::try_from(len).map_err(|_| libc::EFBIG)?;
    cvt(unsafe { libc::fallocate(fd.as_raw_fd(), mode, off, len) })?;
    Ok(())
}

pub fn lseek(fd: BorrowedFd<'_>, offset: i64, whence: i32) -> Res<i64> {
    let r = unsafe { libc::lseek(fd.as_raw_fd(), offset, whence) };
    if r < 0 { Err(errno()) } else { Ok(r) }
}

pub fn copy_file_range(
    fd_in: BorrowedFd<'_>,
    off_in: u64,
    fd_out: BorrowedFd<'_>,
    off_out: u64,
    len: u64,
    flags: u32,
) -> Res<usize> {
    let mut oi = libc::loff_t::try_from(off_in).map_err(|_| libc::EFBIG)?;
    let mut oo = libc::loff_t::try_from(off_out).map_err(|_| libc::EFBIG)?;
    cvt_ssize(unsafe {
        libc::copy_file_range(
            fd_in.as_raw_fd(),
            &mut oi,
            fd_out.as_raw_fd(),
            &mut oo,
            len as usize,
            flags,
        )
    })
}

/// Raise RLIMIT_NOFILE to the hard limit: every inode the kernel holds a
/// reference to pins one O_PATH fd.
pub fn raise_nofile() {
    unsafe {
        let mut rl = MaybeUninit::<libc::rlimit>::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, rl.as_mut_ptr()) == 0 {
            let mut rl = rl.assume_init();
            rl.rlim_cur = rl.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &rl);
        }
    }
}
