//! Kernel FUSE passthrough (Linux 6.9+): once a file is open, its reads,
//! writes and mmaps go straight to the host file, without a round trip
//! through the daemon.
//!
//! The kernel allows one backing file per FUSE inode, so backing ids are kept
//! per inode and shared by all of its open handles. The kernel opens the
//! backing path anew for every FUSE open, with that open's flags, so a
//! read-only open stays read-only. Registering a backing file needs
//! `CAP_SYS_ADMIN`; without it (or on older kernels) all I/O goes through
//! the daemon as before.

use crate::sys;
use fuser::BackingId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

/// Counters for `coffeefs status`.
#[derive(Default, Debug)]
pub struct Stats {
    pub passthrough: AtomicBool,
    pub opens: AtomicU64,
    pub passthrough_opens: AtomicU64,
    pub daemon_reads: AtomicU64,
    pub daemon_writes: AtomicU64,
}

/// A copy of [`Stats`] for the control socket.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatsSnapshot {
    pub passthrough: bool,
    pub opens: u64,
    pub passthrough_opens: u64,
    pub daemon_reads: u64,
    pub daemon_writes: u64,
}

impl Stats {
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Relaxed);
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            passthrough: self.passthrough.load(Relaxed),
            opens: self.opens.load(Relaxed),
            passthrough_opens: self.passthrough_opens.load(Relaxed),
            daemon_reads: self.daemon_reads.load(Relaxed),
            daemon_writes: self.daemon_writes.load(Relaxed),
        }
    }
}

#[derive(Default)]
struct Backings {
    /// Backing id per inode, and how many open handles use it.
    by_ino: HashMap<u64, (Arc<BackingId>, usize)>,
    /// Inode per open handle in passthrough mode.
    by_fh: HashMap<u64, u64>,
}

#[derive(Default)]
pub(super) struct Passthrough {
    backings: Mutex<Backings>,
    pub(super) stats: Arc<Stats>,
}

impl Passthrough {
    /// The backing id to answer an open of `ino` (handle `fh`) with,
    /// registering one through `register` on the inode's first open.
    /// `None`: this open uses daemon I/O.
    pub(super) fn attach(
        &self,
        ino: u64,
        fh: u64,
        register: impl FnOnce() -> std::io::Result<BackingId>,
    ) -> Option<Arc<BackingId>> {
        if !self.stats.passthrough.load(Relaxed) {
            return None;
        }
        let mut b = sys::lock(&self.backings);
        let id = if let Some((id, n)) = b.by_ino.get_mut(&ino) {
            *n += 1;
            id.clone()
        } else {
            match register() {
                Ok(id) => {
                    let id = Arc::new(id);
                    b.by_ino.insert(ino, (id.clone(), 1));
                    id
                }
                Err(e) => {
                    self.failed(ino, &e);
                    return None;
                }
            }
        };
        b.by_fh.insert(fh, ino);
        Stats::bump(&self.stats.passthrough_opens);
        Some(id)
    }

    /// Handle `fh` was released; drop the inode's backing id with its last
    /// handle.
    pub(super) fn release(&self, fh: u64) {
        let mut b = sys::lock(&self.backings);
        let Some(ino) = b.by_fh.remove(&fh) else {
            return;
        };
        if let Some((_, n)) = b.by_ino.get_mut(&ino) {
            *n -= 1;
            if *n == 0 {
                b.by_ino.remove(&ino);
            }
        }
    }

    fn failed(&self, ino: u64, e: &std::io::Error) {
        if e.raw_os_error() == Some(libc::EPERM) {
            log::warn!("kernel passthrough not permitted ({e}); all I/O goes through the daemon");
            self.stats.passthrough.store(false, Relaxed);
        } else {
            // e.g. a backing file on a stacked fs (overlayfs)
            log::debug!("inode {ino}: no passthrough ({e}); its I/O goes through the daemon");
        }
    }
}
