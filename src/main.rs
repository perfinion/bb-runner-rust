#![cfg_attr(not(unix), allow(unused_imports))]

use std::env;
use std::io::Error;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tonic::transport::Server;
use tracing::{self, error, warn};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio_stream::wrappers::UnixListenerStream;

use crate::proto::runner::runner_server::RunnerServer;
use crate::service::RunnerService;

mod cgroup;
mod child;
mod config;
mod local_runner;
mod mmaps;
mod mounts;
mod resource;
mod service;

pub(crate) mod proto {
    pub(crate) mod resourceusage {
        tonic::include_proto!("buildbarn.resourceusage");
    }
    pub(crate) mod runner {
        tonic::include_proto!("buildbarn.runner");
    }
    pub(crate) const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("bb_descriptor");
}

fn bind_socket(path: &Path) -> Result<UnixListenerStream, Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if let Some(e) = std::fs::remove_file(path).err() {
        if e.kind() != ErrorKind::NotFound {
            return Err(Box::new(e));
        }
    }

    let socket = UnixListener::bind(path)?;
    Ok(UnixListenerStream::new(socket))
}

/// If we are pid 1 inside a container, fork a child to do the real work
/// and stay behind as a reaper.  Returns only in the forked child.
/// The parent loop reaps zombies and exits with the child's status.
fn maybe_reexec_as_pid1() {
    if std::process::id() != 1 {
        return;
    }

    // We are pid 1 – fork so the child can run the real program.
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Child) => {
            // Child continues with normal startup.
            return;
        }
        Ok(nix::unistd::ForkResult::Parent { child }) => {
            // Parent stays as pid 1 reaper.
            eprintln!("bb_runner: running as pid1, forked child {child}");
            std::process::exit(pid1_reap_loop(child));
        }
        Err(e) => {
            eprintln!("bb_runner: fork failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Reap all children.  When `main_child` exits, return its exit code.
fn pid1_reap_loop(main_child: nix::unistd::Pid) -> i32 {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

    loop {
        // Wait for any child (-1).  Block until at least one exits.
        match waitpid(None, Some(WaitPidFlag::empty())) {
            Ok(WaitStatus::Exited(pid, code)) => {
                if pid == main_child {
                    return code;
                }
            }
            Ok(WaitStatus::Signaled(pid, sig, _)) => {
                if pid == main_child {
                    // Translate signal death to 128 + signal number (shell convention).
                    return 128 + sig as i32;
                }
            }
            Ok(_) => {
                // Stopped / continued / other – keep reaping.
            }
            Err(nix::errno::Errno::ECHILD) => {
                // No more children at all – main child must have exited
                // before we could observe it (race).  Default to failure.
                eprintln!("bb_runner pid1: no children left");
                return 1;
            }
            Err(e) => {
                eprintln!("bb_runner pid1: waitpid error: {e}");
                return 1;
            }
        }
    }
}

#[cfg(unix)]
// CLONE_NEWUSER requires that the calling process is not threaded
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // If we are pid 1 (e.g. inside a container), fork so that we can act
    // as a proper init process that reaps zombies.  This returns only in
    // the forked child; the parent stays in a reap loop.
    maybe_reexec_as_pid1();
    // if RUST_LOG var is not set, default to debug
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::DEBUG.into())
        .from_env_lossy();
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let argv: Vec<_> = env::args().collect();
    if argv.len() < 2 {
        error!("Missing config file!");
        error!("Usage: %s bb-runner-rust.jsonnet");
        return Err(Error::new(ErrorKind::InvalidFilename, "Missing config file!").into());
    }

    let Some(mut config) = config::Configuration::new(&argv[1]) else {
        error!("Failed to parse configuration");
        return Err(
            Error::new(ErrorKind::InvalidFilename, "Failed to parse configuration!").into(),
        );
    };

    let cgroup_root: Option<PathBuf> = match config.cgroup.as_ref() {
        Some(cg) if cg.delegation => Some(cgroup::setup_delegation()?),
        _ => None,
    };
    config.cgroup_root = cgroup_root.map(|p| Arc::new(p));
    config.cgroup_path = config.cgroup.as_ref().map(|cg| cg.path.clone());

    let socket_stream = bind_socket(config.grpc_listen_path.as_ref())?;

    let bb_runner = RunnerService::new(config);
    let svc = RunnerServer::new(bb_runner);

    let reflection_svc = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
        .build_v1()?;

    warn!("Starting Buildbarn Runner ...");
    Server::builder()
        .add_service(svc)
        .add_service(reflection_svc)
        .serve_with_incoming(socket_stream)
        .await?;

    Ok(())
}

#[cfg(not(unix))]
fn main() {
    panic!("Only works on unix!");
}
