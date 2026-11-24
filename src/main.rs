mod core;
mod ctl;
mod fs;
mod mount;
mod serve;
mod sys;
#[cfg(test)]
mod tests;

use crate::core::ListEntry;
use crate::ctl::{Entry, Origin, Request};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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
    /// Persist runtime changes here and restore them on start.
    #[arg(long)]
    state: Option<PathBuf>,
    /// Entries to set up on every start: a JSON list in the state file's
    /// format (repeatable). Never written to the state file.
    #[arg(long, value_name = "FILE")]
    config: Vec<PathBuf>,
    /// Like --config: link SRC at DST (default: at SRC). Splits at the last
    /// ":/", so SRC may contain ':' (repeatable).
    #[arg(long, value_name = "SRC[:DST]")]
    link: Vec<String>,
    /// Like --link, read-only (repeatable).
    #[arg(long, value_name = "SRC[:DST]")]
    link_ro: Vec<String>,
    /// Like --config: an empty virtual directory (repeatable).
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
    /// Seconds between attempts to apply pending entries (sources that
    /// weren't there yet, conflicts).
    #[arg(long, default_value_t = 5.0)]
    retry: f64,
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

fn absolute(p: &Path) -> Result<PathBuf, String> {
    std::path::absolute(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    let r = match cli.cmd {
        Cmd::Serve(a) => serve::serve(&a),
        Cmd::Add {
            instance,
            source,
            target,
            ro,
            replace,
        } => absolute(&source).and_then(|source| {
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
        }),
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
    } else {
        for e in &entries {
            println!("{}", ls_line(e));
        }
    }
    Ok(())
}

fn ls_line(e: &Entry) -> String {
    let mut s = match &e.entry {
        ListEntry::Dir { target } => format!("{}/", target.display()),
        ListEntry::Link { target, source, ro } => format!(
            "{} -> {}{}",
            target.display(),
            source.display(),
            if *ro { " (ro)" } else { "" }
        ),
    };
    if e.origin == Origin::Config {
        s.push_str(" [config]");
    }
    if let Some(err) = &e.pending {
        s.push_str(&format!(" [pending: {err}]"));
    }
    s
}
