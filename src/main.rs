#![cfg_attr(not(unix), allow(unused_imports))]

use std::env;
use std::io::Error;
use std::io::ErrorKind;
use std::path::Path;
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
    if argv.len() < 2 {
        error!("Missing config file!");
        error!("Usage: %s bb-runner-rust.jsonnet");
        return Err(Error::new(ErrorKind::InvalidFilename, "Missing config file!").into());
    }

    let Some(config) = config::Configuration::new(&argv[1]) else {
        error!("Failed to parse configuration");
        return Err(Error::new(ErrorKind::InvalidFilename, "Failed to parse configuration!").into());
    };

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
