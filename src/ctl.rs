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

use crate::core::{Core, Inval, ListEntry};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<ListEntry>>,
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

pub struct Controller {
    pub core: Arc<Core>,
    pub inval: Box<dyn Invalidator>,
    pub state_file: Option<PathBuf>,
    /// serializes mutations so the state file always matches the tree
    lock: Mutex<()>,
}

impl Controller {
    pub fn new(core: Arc<Core>, inval: Box<dyn Invalidator>, state_file: Option<PathBuf>) -> Self {
        Controller {
            core,
            inval,
            state_file,
            lock: Mutex::new(()),
        }
    }

    pub fn handle(&self, req: Request) -> Response {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let r = match &req {
            Request::Ls => {
                return Response {
                    ok: true,
                    entries: Some(self.core.list()),
                    ..Default::default()
                };
            }
            Request::Add {
                source,
                target,
                ro,
                replace,
            } => self.core.add_link(target, source, *ro, *replace),
            Request::Rm { target, recursive } => self.core.remove(target, *recursive),
            Request::Mkdir { target } => self.core.mkdir_virtual(target),
        };
        match r {
            Ok(invals) => {
                // core lock is released here: the kernel may need to take
                // inode locks held by requests that are waiting on us
                self.inval.invalidate(&invals);
                log::info!("{req:?}");
                if let Err(e) = self.save() {
                    log::error!("saving state: {e}");
                    return Response::err(format!("applied, but saving state failed: {e}"));
                }
                Response::ok()
            }
            Err(e) => Response::err(e),
        }
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.state_file else {
            return Ok(());
        };
        let data = serde_json::to_vec_pretty(&self.core.list())?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, path)
    }

    /// Replay a state file. Missing file = empty; entries whose source has
    /// vanished are skipped with a warning (and dropped on the next save).
    pub fn load(&self) -> std::io::Result<()> {
        let Some(path) = &self.state_file else {
            return Ok(());
        };
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let entries: Vec<ListEntry> = serde_json::from_slice(&data)?;
        for e in entries {
            let r = match &e {
                ListEntry::Dir { target } => self.core.mkdir_virtual(target),
                ListEntry::Link { target, source, ro } => {
                    self.core.add_link(target, source, *ro, false)
                }
            };
            if let Err(err) = r {
                log::warn!("state: skipping {e:?}: {err}");
            }
        }
        Ok(())
    }

    pub fn apply(&self, req: Request) -> Result<(), String> {
        let r = self.handle(req);
        if r.ok {
            Ok(())
        } else {
            Err(r.error.unwrap_or_default())
        }
    }
}

fn peer_allowed(s: &UnixStream) -> bool {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    match getsockopt(s, PeerCredentials) {
        Ok(c) => {
            let me = nix::unistd::geteuid().as_raw();
            c.uid() == 0 || c.uid() == me
        }
        Err(_) => false,
    }
}

fn serve_conn(ctl: &Controller, stream: UnixStream) -> std::io::Result<()> {
    if !peer_allowed(&stream) {
        let mut s = &stream;
        let _ = writeln!(
            s,
            "{}",
            serde_json::to_string(&Response::err("permission denied"))?
        );
        return Ok(());
    }
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    let reader = BufReader::new(&stream);
    let mut w = &stream;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => ctl.handle(req),
            Err(e) => Response::err(format!("bad request: {e}")),
        };
        writeln!(w, "{}", serde_json::to_string(&resp)?)?;
    }
    Ok(())
}

/// Bind the control socket. Refuses to steal a socket another live instance
/// is serving; removes stale ones.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("{} is served by a running instance", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    if let Some(dir) = path.parent()
        && !dir.exists()
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    // Don't touch the process umask here (it's process-wide and the FUSE
    // side relies on it being 0). The brief window before chmod is covered
    // by the SO_PEERCRED check on every connection.
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

pub fn serve(ctl: Arc<Controller>, listener: UnixListener) {
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let ctl = ctl.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve_conn(&ctl, stream) {
                        log::debug!("control connection: {e}");
                    }
                });
            }
            Err(e) => log::warn!("control accept: {e}"),
        }
    }
}

/// Client side: send one request, return the response.
pub fn call(socket: &Path, req: &Request) -> Result<Response, String> {
    let s = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let mut w = &s;
    writeln!(w, "{}", serde_json::to_string(req).unwrap()).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&s)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| format!("bad response {line:?}: {e}"))
}
