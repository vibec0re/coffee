//! Per-thread credentials for creating objects on behalf of a FUSE caller.

use super::{Res, cvt};

/// Give the calling thread its own `fs_struct` (once) and a umask of 0 there.
/// The kernel has already applied the requester's umask to the mode it sends
/// us; the process-wide umask stays restrictive for the daemon's own files.
fn thread_umask_zero() -> Res<()> {
    use std::cell::Cell;
    thread_local! {
        static PRIVATE_FS: Cell<bool> = const { Cell::new(false) };
    }
    PRIVATE_FS.with(|p| {
        if !p.get() {
            cvt(unsafe { libc::unshare(libc::CLONE_FS) })?;
            p.set(true);
        }
        // every time: something else on this thread may have changed it
        unsafe { libc::umask(0) };
        Ok(())
    })
}

/// setfsuid/setfsgid act on the calling thread only (unlike glibc's
/// setresuid, which is broadcast to all threads). They don't report errors,
/// so read the value back to check it took.
fn set_fs_id(nr: libc::c_long, id: u32) -> Res<()> {
    unsafe { libc::syscall(nr, id) };
    let now = unsafe { libc::syscall(nr, u32::MAX) } as u32;
    if now == id { Ok(()) } else { Err(libc::EPERM) }
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
const CAP_DAC_OVERRIDE: u32 = 1;

/// Add `CAP_DAC_OVERRIDE` back to this thread's effective set (dropping fsuid
/// to non-zero clears it). The kernel has already done the permission check
/// against the caller under `default_permissions`; this lets the host
/// operation succeed when the caller's access comes from a supplementary
/// group, which the daemon's thread doesn't carry.
fn raise_dac_override() -> Res<()> {
    let mut h = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut d = [CapData::default(); 2];
    cvt(unsafe { libc::syscall(libc::SYS_capget, &mut h, d.as_mut_ptr()) } as libc::c_int)?;
    d[0].effective |= 1 << CAP_DAC_OVERRIDE;
    cvt(unsafe { libc::syscall(libc::SYS_capset, &mut h, d.as_ptr()) } as libc::c_int)?;
    Ok(())
}

/// While alive, objects created by this thread are owned by the FUSE caller:
/// the kernel assigns owner, group (incl. setgid-directory inheritance) and
/// mode itself, so there is no chown-after-create window to race.
///
/// Without `switch` (unprivileged daemon) only the umask is cleared and
/// objects belong to the daemon's user, as they must.
pub struct AsCaller {
    uid: bool,
    gid: bool,
}

impl AsCaller {
    pub fn enter(uid: u32, gid: u32, switch: bool) -> Res<AsCaller> {
        thread_umask_zero()?;
        let mut g = AsCaller {
            uid: false,
            gid: false,
        };
        if !switch {
            return Ok(g);
        }
        if gid != 0 {
            g.gid = true;
            set_fs_id(libc::SYS_setfsgid, gid)?;
        }
        if uid != 0 {
            g.uid = true;
            set_fs_id(libc::SYS_setfsuid, uid)?;
            raise_dac_override()?;
        }
        Ok(g)
    }
}

impl Drop for AsCaller {
    fn drop(&mut self) {
        // fsuid back to 0 restores the effective fs capabilities from the
        // permitted set. A thread stuck with someone else's identity would
        // create files for the wrong user later, so don't carry on.
        let ok = (!self.uid || set_fs_id(libc::SYS_setfsuid, 0).is_ok())
            && (!self.gid || set_fs_id(libc::SYS_setfsgid, 0).is_ok());
        if !ok {
            log::error!("could not restore filesystem credentials; aborting");
            std::process::abort();
        }
    }
}
