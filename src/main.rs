#![cfg_attr(not(unix), allow(unused_imports))]

use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread;
use tonic::transport::Server;
use tracing::{self, error, warn};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio_stream::wrappers::UnixListenerStream;

use crate::proto::runner::runner_server::RunnerServer;
use crate::service::RunnerService;

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
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::remove_file(path).unwrap_or_else(|error| {
        if error.kind() != ErrorKind::NotFound {
            panic!("Failed to remove socket: {:?}", error);
        }
    });

    let socket = UnixListener::bind(path)?;
    Ok(UnixListenerStream::new(socket))
}

#[cfg(unix)]
// CLONE_NEWUSER requires that the calling process is not threaded
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // if RUST_LOG var is not set, default to debug
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::DEBUG.into())
        .from_env_lossy();
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let argv: Vec<_> = env::args().collect();
    if  argv.len() < 2 {
        error!("Missing config file!");
        error!("Usage: %s bb-runner-rust.jsonnet");
    }

    let _config = config::Configuration::new(&argv[1]);

    let base_path: PathBuf = match env::var("BBRUNNER_BASE_PATH") {
        Ok(val) => PathBuf::from(val),
        Err(_) => std::env::current_dir()?,
    };
    let sock_path = base_path.join("runner");

    let socket_stream: UnixListenerStream =
        bind_socket(sock_path.as_path()).unwrap_or_else(|error| {
            panic!("Failed to create socket: {:?}", error);
        });

    let nproc: u32 = match thread::available_parallelism() {
        Ok(p) => p.get() as u32,
        _ => 8,
    };
    warn!("Number of processors = {}", nproc);

    let bb_runner = RunnerService::new(base_path, nproc);
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
