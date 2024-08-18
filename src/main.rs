#![cfg_attr(not(unix), allow(unused_imports))]

use std::io::ErrorKind;
use std::path::Path;
use tonic::{transport::Server, Status};

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio_stream::wrappers::UnixListenerStream;
#[cfg(unix)]
use tonic::transport::server::UdsConnectInfo;

use proto::resourceusage::PosixResourceUsage;
use proto::runner::runner_server::{Runner, RunnerServer};
use proto::runner::{CheckReadinessRequest, RunRequest, RunResponse};
use prost_types::Any as PbAny;

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
    async fn check_readiness(
        &self,
        request: tonic::Request<CheckReadinessRequest>,
    ) -> std::result::Result<tonic::Response<()>, tonic::Status> {

        let readyreq = request.get_ref();

        println!("CheckReadiness = {:?}", request);

        if Path::new(&readyreq.path).exists() {
            println!("CheckReadiness.path exists = {:?}", readyreq.path);
            return Ok(tonic::Response::new(()));
        }

        println!("CheckReadiness.path not found = {:?}", readyreq.path);
        Err(Status::internal("not ready"))
    }

    #[cfg(unix)]
    async fn run(
        &self,
        request: tonic::Request<RunRequest>,
    ) -> std::result::Result<tonic::Response<RunResponse>, tonic::Status> {
        let conn_info = request.extensions().get::<UdsConnectInfo>().unwrap();
        let run = request.get_ref();

        println!("=== Run ===");
        println!("Connection Info = {:#?}", conn_info);
        println!("Run Request = {:#?}", run);

        let mut child = spawn_child(&run)?;
        let pid = child.id();
        println!("Started process: {}", pid);

        let exit_resuse = wait_child(&mut child).await;
        println!("\nChild {} exit = {:#?}", pid, exit_resuse);

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
            let mut pbres = PosixResourceUsage::default();
            if let Ok(n) = prost_types::Duration::try_from(e.rusage.utime) {
                pbres.user_time = Some(n);
            }

            if let Ok(n) = prost_types::Duration::try_from(e.rusage.stime) {
                pbres.system_time = Some(n);
            }

            if let Ok(n) = i64::try_from(e.rusage.maxrss) {
                pbres.maximum_resident_set_size = n;
            }

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
    println!("Hello, world from bb_runner!");

    let path = Path::new("/tmp/tonic/helloworld");
    let socket_stream: UnixListenerStream = bind_socket(path).unwrap_or_else(|error| {
        panic!("Failed to create socket: {:?}", error);
    });

    let bb_runner = RunnerService {};
    let svc = RunnerServer::new(bb_runner);

    println!("Starting Runner ...");
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
