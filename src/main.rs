#![cfg_attr(not(unix), allow(unused_imports))]

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::Result as TonicResult;
use tonic::{transport::Server, Status};
use tracing::{self, debug, info, warn};

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio_stream::wrappers::UnixListenerStream;
#[cfg(unix)]
use tonic::transport::server::UdsConnectInfo;

use crate::child::ResUse;

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

#[derive(Clone, Debug)]
struct ProcessorQueue(Arc<Mutex<VecDeque<u32>>>);

impl ProcessorQueue {
    pub fn new(deque: VecDeque<u32>) -> Self {
        Self(Arc::new(Mutex::new(deque)))
    }

    pub async fn take_cpu(&self) -> TonicResult<u32> {
        let m = self.0.clone();
        let mut q = m.lock().await;
        q.pop_front()
            .ok_or(Status::resource_exhausted("No available concurrency slots"))
    }

    pub async fn give_cpu(&self, cpu: u32) {
        let m = self.0.clone();
        let mut q = m.lock().await;
        q.push_back(cpu)
    }
}

#[derive(Debug)]
struct RunnerService {
    processors: ProcessorQueue,
}

impl RunnerService {
    pub fn new(nproc: u32) -> RunnerService {
        let p: Vec<u32> = (0..nproc).collect();
        Self {
            processors: ProcessorQueue::new(p.into()),
        }
    }
}

#[tonic::async_trait]
impl Runner for RunnerService {
    #[tracing::instrument]
    async fn check_readiness(
        &self,
        request: tonic::Request<CheckReadinessRequest>,
    ) -> TonicResult<tonic::Response<()>> {
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
    #[tracing::instrument(
        skip_all,
        fields(input_root = %request.get_ref().input_root_directory)
    )]
    async fn run(
        &self,
        request: tonic::Request<RunRequest>,
    ) -> TonicResult<tonic::Response<RunResponse>> {
        let (meta, exts, run) = request.into_parts();
        info!("Run Request = {:#?}", run);

        debug!("MetadataMap: {:?}", meta);
        if let Some(conn_info) = exts.get::<UdsConnectInfo>() {
            debug!("Run Connection Info = {:?}", conn_info);
        }

        // If RPC is cancelled, this task is dropped immediately, must spawn child in a
        // separate task to be able to kill & reap child
        let token = CancellationToken::new();
        let _cancel_guard = token.clone().drop_guard();
        let procque = self.processors.clone();

        let childtask: JoinHandle<TonicResult<ResUse>> = tokio::spawn(async move {
            let processor = procque.take_cpu().await?;
            let mut child = spawn_child(processor, &run)?;
            let pid = child.id();
            debug!("Started process: {} job {}", pid, processor);

            let exit_resuse = wait_child(&mut child, token).await;
            info!("\nChild {} exit = {:#?}", pid, exit_resuse);

            procque.give_cpu(processor).await;
            exit_resuse
        });

        let exit_resuse = childtask
            .await
            .map_err(|_| Status::internal("No Exit Code"))?;

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
// CLONE_NEWUSER requires that the calling process is not threaded
#[tokio::main(flavor = "current_thread")]
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

    let nproc: u32 = match thread::available_parallelism() {
        Ok(p) => p.get() as u32,
        _ => 8,
    };
    warn!("Number of processors = {}", nproc);

    let bb_runner = RunnerService::new(nproc);
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
