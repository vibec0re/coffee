//! Control socket: newline-delimited JSON over a unix stream socket that lives
//! on the host (never inside the container). One request per line, one
//! response per line; several requests per connection are fine.
//!
//! ```text
//! {"op":"add","source":"/srv/proj","target":"/work/proj","ro":true}
//! {"op":"rm","target":"/work/proj"}
//! {"op":"mkdir","target":"/tmp"}
//! {"op":"ls"}
//! ```
//!
//! Entries come from two places:
//! - **config** (`--config`, `--link`, `--mkdir`): declarative, applied first
//!   on every start and never written to the state file, so removing one from
//!   the config revokes it;
//! - **runtime** (the socket): persisted in the state file and replayed after
//!   the config. Where the two conflict, config wins.
//!
//! An entry that can't be applied (its source isn't there yet, or it
//! conflicts) is kept as *pending*: listed, persisted if it's a runtime one,
//! and retried periodically until it succeeds or is removed.

use crate::core::{Core, Inval, ListEntry};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

mod socket;
pub use socket::{bind, call, serve};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    Add {
        source: PathBuf,
        target: PathBuf,
        #[serde(default)]
        ro: bool,
        #[serde(default)]
        replace: bool,
    },
    Rm {
        target: PathBuf,
        #[serde(default)]
        recursive: bool,
    },
    Mkdir {
        target: PathBuf,
    },
    Ls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Config,
    Runtime,
}

/// One line of `ls`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(flatten)]
    pub entry: ListEntry,
    pub origin: Origin,
    /// Not in the tree: why the last attempt failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<Entry>>,
}

impl Response {
    fn ok() -> Self {
        Response {
            ok: true,
            ..Default::default()
        }
    }
    fn err(e: impl Into<String>) -> Self {
        Response {
            ok: false,
            error: Some(e.into()),
            ..Default::default()
        }
    }
}

/// Receives kernel cache invalidations (the FUSE notifier, or nothing in tests).
pub trait Invalidator: Send + Sync {
    fn invalidate(&self, invals: &[Inval]);
}

impl Invalidator for fuser::Notifier {
    fn invalidate(&self, invals: &[Inval]) {
        for i in invals {
            let r = match i {
                Inval::Entry { parent, name } => self.inval_entry(fuser::INodeNo(*parent), name),
                Inval::Inode(ino) => self.inval_inode(fuser::INodeNo(*ino), -1, 0),
            };
            // ENOENT just means the kernel had nothing cached
            if let Err(e) = r
                && e.raw_os_error() != Some(libc::ENOENT)
            {
                log::warn!("invalidate {i:?}: {e}");
            }
        }
    }
}

pub struct NoInval;
impl Invalidator for NoInval {
    fn invalidate(&self, _: &[Inval]) {}
}

/// Read a list of entries (state or config file). The file decides what
/// the (root) daemon exposes, so refuse one that anybody but root or us
/// could have written.
pub fn read_entries(path: &Path) -> std::io::Result<Vec<ListEntry>> {
    use std::os::unix::fs::MetadataExt;
    let mut f = std::fs::File::open(path)?;
    let md = f.metadata()?;
    let me = nix::unistd::geteuid().as_raw();
    if (md.uid() != me && md.uid() != 0) || md.mode() & 0o022 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing a file that others can write (owner {}, mode {:o}); \
                 check its contents, then chown/chmod it to 0600",
                md.uid(),
                md.mode() & 0o7777
            ),
        ));
    }
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut data)?;
    // every parse error, including a truncated file (EOF), is InvalidData
    serde_json::from_slice(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

struct Pending {
    entry: ListEntry,
    origin: Origin,
    error: String,
}

/// What the controller knows beyond the tree itself.
#[derive(Default)]
struct Book {
    /// Everything the config asked for.
    config: Vec<ListEntry>,
    /// Entries not (yet) in the tree, in the order they were asked for.
    pending: Vec<Pending>,
}

impl Book {
    fn origin(&self, e: &ListEntry) -> Origin {
        if self.config.contains(e) {
            Origin::Config
        } else {
            Origin::Runtime
        }
    }

    /// Forget pending entries at `target` (and below it if `subtree`).
    fn drop_pending(&mut self, target: &Path, subtree: bool) -> usize {
        let before = self.pending.len();
        self.pending.retain(|p| {
            let t = p.entry.target();
            !(t == target || subtree && t.starts_with(target))
        });
        before - self.pending.len()
    }
}

pub struct Controller {
    core: Arc<Core>,
    inval: Box<dyn Invalidator>,
    state_file: Option<PathBuf>,
    /// Also serializes mutations, so the state file always matches the tree.
    book: Mutex<Book>,
}

impl Controller {
    pub fn new(core: Arc<Core>, inval: Box<dyn Invalidator>, state_file: Option<PathBuf>) -> Self {
        Controller {
            core,
            inval,
            state_file,
            book: Mutex::new(Book::default()),
        }
    }

    /// Swap in the real invalidator once the filesystem is mounted.
    pub fn set_invalidator(&mut self, inval: Box<dyn Invalidator>) {
        self.inval = inval;
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        crate::sys::lock(&self.book)
    }

    fn apply_entry(&self, e: &ListEntry, replace: bool) -> Result<Vec<Inval>, String> {
        match e {
            ListEntry::Dir { target } => self.core.mkdir_virtual(target),
            ListEntry::Link { target, source, ro } => {
                self.core.add_link(target, source, *ro, replace)
            }
        }
    }

    /// Populate at startup: config first, then the state file. Nothing here
    /// fails startup except an untrustworthy state file; whatever can't be
    /// applied becomes pending.
    pub fn start(&self, config: Vec<ListEntry>) -> std::io::Result<()> {
        let mut b = self.book();
        for e in config {
            if let Err(error) = self.apply_entry(&e, false) {
                log::warn!("config: {}: {error} (pending)", e.target().display());
                b.pending.push(Pending {
                    entry: e.clone(),
                    origin: Origin::Config,
                    error,
                });
            }
            b.config.push(e);
        }
        for e in self.read_state()? {
            // older versions persisted config entries too
            if b.config.contains(&e) {
                continue;
            }
            if let Err(error) = self.apply_entry(&e, false) {
                log::warn!("state: {}: {error} (pending)", e.target().display());
                b.pending.push(Pending {
                    entry: e,
                    origin: Origin::Runtime,
                    error,
                });
            }
        }
        Ok(())
    }

    /// The state file's entries. A corrupt file is moved aside (and the
    /// instance starts without it) rather than keeping the daemon down.
    fn read_state(&self) -> std::io::Result<Vec<ListEntry>> {
        let Some(path) = &self.state_file else {
            return Ok(Vec::new());
        };
        match read_entries(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                let secs = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let aside = path.with_extension(format!("corrupt-{secs}"));
                log::error!(
                    "state {}: {e}; moved to {} and starting without it",
                    path.display(),
                    aside.display()
                );
                std::fs::rename(path, &aside)?;
                Ok(Vec::new())
            }
            r => r,
        }
    }

    pub fn handle(&self, req: Request) -> Response {
        let mut b = self.book();
        let r = match &req {
            Request::Ls => {
                return Response {
                    ok: true,
                    entries: Some(self.entries(&b)),
                    ..Default::default()
                };
            }
            Request::Add {
                source,
                target,
                ro,
                replace,
            } => self
                .core
                .add_link(target, source, *ro, *replace)
                .inspect(|_| _ = b.drop_pending(target, false)),
            Request::Mkdir { target } => self
                .core
                .mkdir_virtual(target)
                .inspect(|_| _ = b.drop_pending(target, false)),
            Request::Rm { target, recursive } => {
                let dropped = b.drop_pending(target, *recursive);
                match self.core.remove(target, *recursive) {
                    Err(_) if dropped > 0 => Ok(Vec::new()),
                    r => r,
                }
            }
        };
        match r {
            Ok(invals) => {
                // core lock is released here: the kernel may need to take
                // inode locks held by requests that are waiting on us
                self.inval.invalidate(&invals);
                log::info!("{req:?}");
                if let Err(e) = self.save(&b) {
                    log::error!("saving state: {e}");
                    return Response::err(format!("applied, but saving state failed: {e}"));
                }
                Response::ok()
            }
            Err(e) => Response::err(e),
        }
    }

    fn entries(&self, b: &Book) -> Vec<Entry> {
        let live = self.core.list().into_iter().map(|entry| Entry {
            origin: b.origin(&entry),
            entry,
            pending: None,
        });
        let pending = b.pending.iter().map(|p| Entry {
            entry: p.entry.clone(),
            origin: p.origin,
            pending: Some(p.error.clone()),
        });
        live.chain(pending).collect()
    }

    /// Try the pending entries again (in order: a parent may unblock a
    /// child). Returns how many went live.
    pub fn retry(&self) -> usize {
        let mut b = self.book();
        if b.pending.is_empty() {
            return 0;
        }
        let mut invals = Vec::new();
        let mut done = 0;
        for mut p in std::mem::take(&mut b.pending) {
            match self.apply_entry(&p.entry, false) {
                Ok(i) => {
                    log::info!("{}: now available", p.entry.target().display());
                    invals.extend(i);
                    done += 1;
                }
                Err(error) => {
                    p.error = error;
                    b.pending.push(p);
                }
            }
        }
        if done > 0 {
            self.inval.invalidate(&invals);
            if let Err(e) = self.save(&b) {
                log::error!("saving state: {e}");
            }
        }
        done
    }

    /// Write the runtime entries (live and pending) atomically and durably:
    /// a fresh 0600 tmp file (never one left behind by someone else), fsync,
    /// rename, fsync the directory.
    fn save(&self, b: &Book) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let Some(path) = &self.state_file else {
            return Ok(());
        };
        let entries: Vec<ListEntry> = self
            .entries(b)
            .into_iter()
            .filter(|e| e.origin == Origin::Runtime)
            .map(|e| e.entry)
            .collect();
        let data = serde_json::to_vec_pretty(&entries)?;
        let tmp = path.with_extension("tmp");
        match std::fs::remove_file(&tmp) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
        std::fs::File::open(dir.unwrap_or(Path::new(".")))?.sync_all()
    }

    #[cfg(test)]
    pub fn apply(&self, req: Request) -> Result<(), String> {
        let r = self.handle(req);
        if r.ok {
            Ok(())
        } else {
            Err(r.error.unwrap_or_default())
        }
    }
}

/// Retry pending entries every `every` for as long as the process runs.
pub fn spawn_retry(ctl: Arc<Controller>, every: Duration) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(every);
            ctl.retry();
        }
    });
}
