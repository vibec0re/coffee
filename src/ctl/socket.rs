//! The control socket itself: binding, serving connections, the client.

use super::{Controller, Request, Response};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

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
    // The brief window before chmod is covered by the SO_PEERCRED check on
    // every connection.
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
