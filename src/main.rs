#![cfg_attr(not(unix), allow(unused_imports))]

use std::io::ErrorKind;
use std::path::Path;
use tonic::{transport::Server, Status};
use tracing::{self, debug, info, warn};

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio_stream::wrappers::UnixListenerStream;
#[cfg(unix)]
use tonic::transport::server::UdsConnectInfo;

use prost_types::Any as PbAny;
use proto::resourceusage::PosixResourceUsage;
use proto::runner::runner_server::{Runner, RunnerServer};
use proto::runner::{CheckReadinessRequest, RunRequest, RunResponse};

pub mod proto {
    pub mod resourceusage {
        tonic::include_proto!("buildbarn.resourceusage");
    }
    pub mod runner {
        tonic::include_proto!("buildbarn.runner");
    }
}

mod local_runner;
use local_runner::{spawn_child, wait_child};

pub mod child;

#[derive(Debug)]
struct RunnerService;

#[tonic::async_trait]
impl Runner for RunnerService {
    #[tracing::instrument]
    async fn check_readiness(
        &self,
        request: tonic::Request<CheckReadinessRequest>,
    ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
        let readyreq = request.get_ref();

        debug!("CheckReadiness = {:?}", request);

        if Path::new(&readyreq.path).exists() {
            info!("CheckReadiness.path exists = {:?}", readyreq.path);
            return Ok(tonic::Response::new(()));
        }

        info!("CheckReadiness.path not found = {:?}", readyreq.path);
        Err(Status::internal("not ready"))
    }

    #[cfg(unix)]
    #[tracing::instrument]
    async fn run(
        &self,
        request: tonic::Request<RunRequest>,
    ) -> std::result::Result<tonic::Response<RunResponse>, tonic::Status> {
        let conn_info = request.extensions().get::<UdsConnectInfo>().unwrap();
        let run = request.get_ref();

        info!("Run Request = {:#?}", run);
        debug!("Run Connection Info = {:#?}", conn_info);

        let mut child = spawn_child(&run)?;
        let pid = child.id();
        debug!("Started process: {}", pid);

        let exit_resuse = wait_child(&mut child).await;
        info!("\nChild {} exit = {:#?}", pid, exit_resuse);

        let exit_code = match exit_resuse {
            Ok(ref e) => e.status.code(),
            Err(_) => Some(255),
        };

        let mut runresp = RunResponse::default();
        match exit_code {
            Some(code) => runresp.exit_code = code,
            None => return Err(Status::internal("No Exit Code")),
        }
        if let Ok(e) = exit_resuse {
            let pbres = e.rusage.into();
            if let Ok(r) = PbAny::from_msg::<PosixResourceUsage>(&pbres) {
                runresp.resource_usage = vec![r];
            };
        }

        Ok(tonic::Response::new(runresp))
    }
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
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use tracing_subscriber::{filter::LevelFilter, EnvFilter};

    // if RUST_LOG var is not set, default to debug
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::DEBUG.into())
        .from_env_lossy();
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let path = Path::new("/tmp/tonic/helloworld");
    let socket_stream: UnixListenerStream = bind_socket(path).unwrap_or_else(|error| {
        panic!("Failed to create socket: {:?}", error);
    });

    let bb_runner = RunnerService {};
    let svc = RunnerServer::new(bb_runner);

    warn!("Starting Buildbarn Runner ...");
    Server::builder()
        .add_service(svc)
        .serve_with_incoming(socket_stream)
        .await?;

    Ok(())
}

#[cfg(not(unix))]
fn main() {
    panic!("Only works on unix!");
}
