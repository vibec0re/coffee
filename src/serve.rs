//! `coffeefs serve`: set up an instance, mount it, serve it until a signal.

use crate::core::{self, Core, ListEntry};
use crate::ctl::{self, Controller, NoInval};
use crate::fs::CoffeeFs;
use crate::{ServeArgs, absolute, instance_socket, mount, sys};
use fuser::{Config, MountOption, Owner, Session, SessionACL};
use nix::sys::signal::{SigSet, Signal};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// `SRC[:DST]`. DST is absolute, so the separator is the last ":/": SRC may
/// contain ':' (and even ":/"), DST may not contain ":/".
pub(crate) fn parse_link(s: &str, ro: bool) -> Result<ListEntry, String> {
    let (src, dst) = match s.rfind(":/") {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let source = absolute(Path::new(src))?;
    let target = dst.map_or_else(|| source.clone(), PathBuf::from);
    Ok(ListEntry::Link { target, source, ro })
}

/// Everything the command line asks for on every start. Malformed entries
/// are an admin error and fail startup; ones that merely can't be applied
/// yet become pending later.
fn config_entries(a: &ServeArgs) -> Result<Vec<ListEntry>, String> {
    let mut v = Vec::new();
    for c in &a.config {
        v.extend(ctl::read_entries(c).map_err(|e| format!("config {}: {e}", c.display()))?);
    }
    v.extend(a.mkdir.iter().map(|d| ListEntry::Dir { target: d.clone() }));
    for l in &a.link {
        v.push(parse_link(l, false)?);
    }
    for l in &a.link_ro {
        v.push(parse_link(l, true)?);
    }
    for e in &v {
        core::parse_target(e.target())?;
        if let ListEntry::Link { source, .. } = e
            && !source.is_absolute()
        {
            return Err(format!("{}: source must be absolute", source.display()));
        }
    }
    Ok(v)
}

/// Send a state change to systemd (`Type=notify` / `notify-reload`).
fn sd_notify(msg: &str) {
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
            sock.send_to_addr(msg.as_bytes(), &addr)
        } else {
            sock.send_to(msg.as_bytes(), &path)
        }
    })();
    if let Err(e) = r {
        log::warn!("sd_notify: {e}");
    }
}

fn monotonic_usec() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid pointer to a timespec
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
    u64::try_from(ts.tv_sec).unwrap_or(0) * 1_000_000
        + u64::try_from(ts.tv_nsec).unwrap_or(0) / 1000
}

/// Where an instance lives.
struct Instance {
    name: String,
    mountpoint: PathBuf,
    socket: PathBuf,
    /// Mount namespace to mount in (default: our own).
    ns_path: Option<PathBuf>,
}

impl Instance {
    fn resolve(a: &ServeArgs) -> Result<Instance, String> {
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
            (None, Some(_)) => {
                return Err("--mountpoint is required with --pid/--mount-ns".into());
            }
        };
        let socket = a.socket.clone().unwrap_or_else(|| instance_socket(&name));
        Ok(Instance {
            name,
            mountpoint,
            socket,
            ns_path,
        })
    }
}

/// Signals are consumed by a sigwait thread; block them before any thread
/// exists so every thread inherits the mask.
fn block_signals() -> Result<SigSet, String> {
    let mut sigs = SigSet::empty();
    for s in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP] {
        sigs.add(s);
    }
    sigs.thread_block()
        .map_err(|e| format!("blocking signals: {e}"))?;
    Ok(sigs)
}

pub fn serve(a: &ServeArgs) -> Result<(), String> {
    let inst = Instance::resolve(a)?;
    let config = config_entries(a)?;
    let sigs = block_signals()?;
    sys::raise_nofile();
    let ns = inst
        .ns_path
        .as_deref()
        .map(mount::open_ns)
        .transpose()
        .map_err(|e| e.to_string())?;
    // Our own files (state, socket dir, mountpoints) get explicit modes; this
    // is the backstop. FUSE worker threads switch to a private umask of 0
    // while creating on behalf of a request (sys::AsCaller).
    unsafe { libc::umask(0o022) };
    let listener = ctl::bind(&inst.socket)
        .map_err(|e| format!("control socket {}: {e}", inst.socket.display()))?;

    let euid = nix::unistd::geteuid();
    let owner = Owner {
        uid: euid.as_raw(),
        gid: nix::unistd::getegid().as_raw(),
    };
    let core = Arc::new(Core::new(owner, euid.is_root()));

    // Populate before mounting: no kernel caches to invalidate yet.
    let mut ctl = Controller::new(core.clone(), Box::new(NoInval), a.state.clone());
    ctl.start(config).map_err(|e| match &a.state {
        Some(p) => format!("state {}: {e}", p.display()),
        None => e.to_string(),
    })?;

    let ttl = Duration::from_secs_f64(a.ttl.max(0.0));
    let fs = CoffeeFs::new(core, ttl, !a.no_passthrough);
    ctl.set_stats(fs.stats());
    let session = mount_session(fs, &inst, ns.as_ref(), a.threads)?;
    ctl.set_invalidator(Box::new(session.notifier()));
    let ctl = Arc::new(ctl);
    {
        let ctl = ctl.clone();
        std::thread::spawn(move || ctl::serve(ctl, listener));
    }
    ctl::spawn_retry(ctl.clone(), Duration::from_secs_f64(a.retry.max(0.1)));

    log::info!(
        "instance {}: mounted at {}{}, control socket {}",
        inst.name,
        inst.mountpoint.display(),
        inst.ns_path
            .as_ref()
            .map(|p| format!(" in {}", p.display()))
            .unwrap_or_default(),
        inst.socket.display()
    );
    let reload = || {
        sd_notify(&format!("RELOADING=1\nMONOTONIC_USEC={}", monotonic_usec()));
        match config_entries(a) {
            Ok(config) => {
                ctl.reload(config);
                ctl.retry();
            }
            Err(e) => log::error!("reload: {e}; keeping the current config"),
        }
        sd_notify("READY=1");
    };
    let result = run_until_signal(session, sigs, &inst, ns.as_ref(), reload);
    let _ = std::fs::remove_file(&inst.socket);
    result
}

fn mount_session(
    fs: CoffeeFs,
    inst: &Instance,
    ns: Option<&OwnedFd>,
    threads: usize,
) -> Result<Session<CoffeeFs>, String> {
    let fsname = format!("coffeefs:{}", inst.name);
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName(fsname.clone()),
        MountOption::Subtype("coffeefs".into()),
        MountOption::DefaultPermissions,
        MountOption::NoDev,
        MountOption::NoSuid,
    ];
    config.acl = SessionACL::All;
    config.n_threads = Some(threads.max(1));
    config.clone_fd = true;

    let mp = &inst.mountpoint;
    let r = if ns.is_some() || nix::unistd::geteuid().is_root() {
        // Mount ourselves: works in a foreign namespace, and gives the proper
        // fuse.coffeefs type (fuser's own mount always says "fuse").
        (|| {
            let fuse = mount::open_dev_fuse()?;
            match ns {
                Some(ns) => mount::mount_in_ns(ns.as_fd(), mp, fuse.as_fd(), &fsname)?,
                None => mount::mount_here(mp, fuse.as_fd(), &fsname)?,
            }
            Session::from_fd(fs, fuse, SessionACL::All, config)
        })()
    } else {
        // unprivileged: let fuser go through fusermount3
        mount::clear_stale(mp)
            .and_then(|()| mount::prepare_mountpoint(mp))
            .and_then(|()| Session::new(fs, mp, &config))
    };
    r.map_err(|e| format!("mounting {}: {e}", mp.display()))
}

enum Event {
    Signal(Signal),
    Ended(std::io::Result<()>),
}

/// Serve until SIGINT/SIGTERM (unmount and exit) or the session ends.
/// SIGHUP reloads the config.
fn run_until_signal(
    session: Session<CoffeeFs>,
    sigs: SigSet,
    inst: &Instance,
    ns: Option<&OwnedFd>,
    reload: impl Fn(),
) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Event::Ended(session.run()));
        });
    }
    std::thread::spawn(move || {
        while let Ok(sig) = sigs.wait() {
            if tx.send(Event::Signal(sig)).is_err() {
                break;
            }
        }
    });
    sd_notify("READY=1");

    loop {
        match rx.recv() {
            Ok(Event::Signal(Signal::SIGHUP)) => {
                log::info!("SIGHUP: reloading the config");
                reload();
            }
            Ok(Event::Signal(sig)) => {
                log::info!("{sig}: unmounting");
                if let Err(e) = mount::detach(ns.map(|n| n.as_fd()), &inst.mountpoint) {
                    log::warn!("unmount {}: {e}", inst.mountpoint.display());
                }
                // Give the session a moment to wind down; if something in the
                // container still holds files open, exiting aborts the connection.
                let _ = rx.recv_timeout(Duration::from_secs(2));
                return Ok(());
            }
            Ok(Event::Ended(r)) => return r.map_err(|e| format!("session ended: {e}")),
            Err(_) => return Ok(()),
        }
    }
}
