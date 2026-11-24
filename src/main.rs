mod core;
mod ctl;
mod fs;
mod mount;
mod sys;
#[cfg(test)]
mod tests;

use crate::core::{Core, ListEntry};
use crate::ctl::{Controller, NoInval, Request};
use clap::{Parser, Subcommand};
use fuser::{Config, MountOption, Owner, Session, SessionACL};
use nix::sys::signal::{SigSet, Signal};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// FUSE passthrough filesystem whose contents are wired up at runtime.
///
/// Each instance mounts an (initially empty) virtual tree. Host files and
/// directories are linked into it — and removed again — through a control
/// socket that stays outside the container.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run an instance: mount the filesystem and serve its control socket.
    Serve(ServeArgs),
    /// Link a host file or directory into an instance.
    Add {
        /// Instance name, or path to its control socket.
        instance: String,
        /// Host path to expose.
        source: PathBuf,
        /// Path inside the filesystem (default: same as SOURCE).
        target: Option<PathBuf>,
        /// Read-only: writes through this link fail with EROFS.
        #[arg(long)]
        ro: bool,
        /// Replace an existing link at TARGET.
        #[arg(long)]
        replace: bool,
    },
    /// Remove a link (or virtual directory).
    Rm {
        instance: String,
        target: PathBuf,
        /// Remove a virtual directory together with everything below it.
        #[arg(short, long)]
        recursive: bool,
    },
    /// Create an empty virtual directory (e.g. a mountpoint for the container).
    Mkdir { instance: String, target: PathBuf },
    /// List links and directories.
    Ls {
        instance: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args)]
struct ServeArgs {
    /// Instance name; picks the default socket and mountpoint.
    name: String,
    /// Where to mount [default: /containers/NAME].
    #[arg(short, long)]
    mountpoint: Option<PathBuf>,
    /// Control socket [default: /run/coffeefs/NAME.sock].
    #[arg(short, long)]
    socket: Option<PathBuf>,
    /// Persist links here and restore them on start.
    #[arg(long)]
    state: Option<PathBuf>,
    /// Initial link, SRC[:DST] (repeatable).
    #[arg(long, value_name = "SRC[:DST]")]
    link: Vec<String>,
    /// Initial read-only link, SRC[:DST] (repeatable).
    #[arg(long, value_name = "SRC[:DST]")]
    link_ro: Vec<String>,
    /// Initial empty virtual directory (repeatable).
    #[arg(long, value_name = "DIR")]
    mkdir: Vec<PathBuf>,
    /// Mount inside the mount namespace of this process (e.g. a container's
    /// init), instead of the daemon's own.
    #[arg(long, conflicts_with = "mount_ns")]
    pid: Option<u32>,
    /// Mount inside this mount namespace (an nsfs file such as /proc/PID/ns/mnt).
    #[arg(long)]
    mount_ns: Option<PathBuf>,
    /// FUSE worker threads.
    #[arg(long, default_value_t = 4)]
    threads: usize,
    /// Kernel attribute/entry cache timeout in seconds (host-side changes
    /// show up after at most this long).
    #[arg(long, default_value_t = 1.0)]
    ttl: f64,
}

fn socket_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("COFFEEFS_SOCKET_DIR") {
        return d.into();
    }
    if nix::unistd::geteuid().is_root() {
        return "/run/coffeefs".into();
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d).join("coffeefs"),
        None => format!("/tmp/coffeefs-{}", nix::unistd::geteuid()).into(),
    }
}

fn instance_socket(instance: &str) -> PathBuf {
    if instance.contains('/') {
        instance.into()
    } else {
        socket_dir().join(format!("{instance}.sock"))
    }
}

fn split_link(s: &str) -> (PathBuf, PathBuf) {
    match s.split_once(':') {
        Some((a, b)) => (a.into(), b.into()),
        None => (s.into(), s.into()),
    }
}

fn absolute(p: &Path) -> Result<PathBuf, String> {
    std::path::absolute(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    let r = match cli.cmd {
        Cmd::Serve(a) => serve(a),
        Cmd::Add {
            instance,
            source,
            target,
            ro,
            replace,
        } => (|| {
            let source = absolute(&source)?;
            let target = target.unwrap_or_else(|| source.clone());
            client(
                &instance,
                Request::Add {
                    source,
                    target,
                    ro,
                    replace,
                },
            )
        })(),
        Cmd::Rm {
            instance,
            target,
            recursive,
        } => client(&instance, Request::Rm { target, recursive }),
        Cmd::Mkdir { instance, target } => client(&instance, Request::Mkdir { target }),
        Cmd::Ls { instance, json } => ls(&instance, json),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("coffeefs: {e}");
            ExitCode::FAILURE
        }
    }
}

fn client(instance: &str, req: Request) -> Result<(), String> {
    let r = ctl::call(&instance_socket(instance), &req)?;
    if r.ok {
        Ok(())
    } else {
        Err(r.error.unwrap_or_else(|| "failed".into()))
    }
}

fn ls(instance: &str, json: bool) -> Result<(), String> {
    let r = ctl::call(&instance_socket(instance), &Request::Ls)?;
    if !r.ok {
        return Err(r.error.unwrap_or_default());
    }
    let entries = r.entries.unwrap_or_default();
    if json {
        println!("{}", serde_json::to_string_pretty(&entries).unwrap());
        return Ok(());
    }
    for e in entries {
        match e {
            ListEntry::Dir { target } => println!("{}/", target.display()),
            ListEntry::Link { target, source, ro } => {
                println!(
                    "{} -> {}{}",
                    target.display(),
                    source.display(),
                    if ro { " (ro)" } else { "" }
                )
            }
        }
    }
    Ok(())
}

/// Tell systemd (Type=notify) we're mounted, so units ordered after us —
/// e.g. a container bind-mounting the mountpoint — see the live filesystem.
fn sd_notify_ready() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixDatagram;
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let r = (|| {
        let sock = UnixDatagram::unbound()?;
        let b = path.as_bytes();
        if let Some(name) = b.strip_prefix(b"@") {
            use std::os::linux::net::SocketAddrExt;
            let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            sock.send_to_addr(b"READY=1", &addr)
        } else {
            sock.send_to(b"READY=1", &path)
        }
    })();
    if let Err(e) = r {
        log::warn!("sd_notify: {e}");
    }
}

enum Event {
    Signal(Signal),
    Ended(std::io::Result<()>),
}

fn serve(a: ServeArgs) -> Result<(), String> {
    let name = a.name.clone();
    if name.is_empty() || name.contains('/') {
        return Err("instance name must be non-empty and contain no '/'".into());
    }
    let ns_path = a
        .mount_ns
        .clone()
        .or(a.pid.map(|p| format!("/proc/{p}/ns/mnt").into()));
    let mountpoint = match (&a.mountpoint, &ns_path) {
        (Some(m), _) => m.clone(),
        (None, None) => format!("/containers/{name}").into(),
        (None, Some(_)) => return Err("--mountpoint is required with --pid/--mount-ns".into()),
    };
    let socket = a.socket.clone().unwrap_or_else(|| instance_socket(&name));
    let ttl = Duration::from_secs_f64(a.ttl.max(0.0));

    // Signals are consumed by a sigwait thread; block them before any thread
    // exists so every thread inherits the mask.
    let mut sigs = SigSet::empty();
    for s in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP] {
        sigs.add(s);
    }
    sigs.thread_block()
        .map_err(|e| format!("blocking signals: {e}"))?;
    sys::raise_nofile();

    let ns = ns_path
        .as_deref()
        .map(mount::open_ns)
        .transpose()
        .map_err(|e| e.to_string())?;
    // Our own files (state, socket dir, mountpoints) get explicit modes; this
    // is the backstop. FUSE worker threads switch to a private umask of 0
    // while creating on behalf of a request (sys::AsCaller).
    unsafe { libc::umask(0o022) };

    let listener =
        ctl::bind(&socket).map_err(|e| format!("control socket {}: {e}", socket.display()))?;

    let euid = nix::unistd::geteuid();
    let owner = Owner {
        uid: euid.as_raw(),
        gid: nix::unistd::getegid().as_raw(),
    };
    let core = Arc::new(Core::new(owner, euid.is_root()));

    // Populate before mounting: no kernel caches to invalidate yet.
    {
        let pre = Controller::new(core.clone(), Box::new(NoInval), a.state.clone());
        pre.load()
            .map_err(|e| format!("state {}: {e}", a.state.as_ref().unwrap().display()))?;
        for d in &a.mkdir {
            pre.apply(Request::Mkdir { target: d.clone() })?;
        }
        for (l, ro) in a
            .link
            .iter()
            .map(|l| (l, false))
            .chain(a.link_ro.iter().map(|l| (l, true)))
        {
            let (source, target) = split_link(l);
            pre.apply(Request::Add {
                source: absolute(&source)?,
                target,
                ro,
                replace: true,
            })?;
        }
    }

    let fs = fs::CoffeeFs {
        core: core.clone(),
        ttl,
    };
    let fsname = format!("coffeefs:{name}");
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName(fsname.clone()),
        MountOption::Subtype("coffeefs".into()),
        MountOption::DefaultPermissions,
        MountOption::NoDev,
        MountOption::NoSuid,
    ];
    config.acl = SessionACL::All;
    config.n_threads = Some(a.threads.max(1));
    config.clone_fd = true;

    let session = match &ns {
        Some(ns) => {
            let fuse = mount::open_dev_fuse().map_err(|e| e.to_string())?;
            mount::mount_in_ns(ns.as_fd(), &mountpoint, fuse.as_fd(), &fsname)
                .map_err(|e| e.to_string())?;
            Session::from_fd(fs, fuse, SessionACL::All, config)
        }
        // as root, mount ourselves (same path as above, proper fuse.coffeefs type)
        None if euid.is_root() => {
            let fuse = mount::open_dev_fuse().map_err(|e| e.to_string())?;
            mount::mount_here(&mountpoint, fuse.as_fd(), &fsname).map_err(|e| e.to_string())?;
            Session::from_fd(fs, fuse, SessionACL::All, config)
        }
        // unprivileged: let fuser go through fusermount3
        None => {
            mount::clear_stale(&mountpoint)
                .map_err(|e| format!("{}: {e}", mountpoint.display()))?;
            mount::prepare_mountpoint(&mountpoint)
                .map_err(|e| format!("{}: {e}", mountpoint.display()))?;
            Session::new(fs, &mountpoint, &config)
        }
    }
    .map_err(|e| format!("mounting {}: {e}", mountpoint.display()))?;

    let ctl = Arc::new(Controller::new(
        core,
        Box::new(session.notifier()),
        a.state.clone(),
    ));
    {
        let ctl = ctl.clone();
        std::thread::spawn(move || ctl::serve(ctl, listener));
    }

    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Event::Ended(session.run()));
        });
    }
    std::thread::spawn(move || {
        if let Ok(sig) = sigs.wait() {
            let _ = tx.send(Event::Signal(sig));
        }
    });

    log::info!(
        "instance {name}: mounted at {}{}, control socket {}",
        mountpoint.display(),
        ns_path
            .as_ref()
            .map(|p| format!(" in {}", p.display()))
            .unwrap_or_default(),
        socket.display()
    );
    sd_notify_ready();

    let result = match rx.recv() {
        Ok(Event::Signal(sig)) => {
            log::info!("{sig}: unmounting");
            if let Err(e) = mount::detach(ns.as_ref().map(|n| n.as_fd()), &mountpoint) {
                log::warn!("unmount {}: {e}", mountpoint.display());
            }
            // Give the session a moment to wind down; if something in the
            // container still holds files open, exiting aborts the connection.
            let _ = rx.recv_timeout(Duration::from_secs(2));
            Ok(())
        }
        Ok(Event::Ended(r)) => r.map_err(|e| format!("session ended: {e}")),
        Err(_) => Ok(()),
    };
    let _ = std::fs::remove_file(&socket);
    result
}
