use prost_types::Any as PbAny;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::Result as TonicResult;
use tonic::Status;
use tracing::{self, debug, info, trace};

#[cfg(unix)]
use tonic::transport::server::UdsConnectInfo;

use crate::proto::resourceusage::PosixResourceUsage;
use crate::proto::runner::runner_server::Runner;
use crate::proto::runner::{CheckReadinessRequest, RunRequest, RunResponse};

use crate::local_runner::{spawn_child, wait_child};
use crate::resource::ExitResources;
use crate::config::Configuration;

#[derive(Clone, Debug)]
struct ProcessorQueue(Arc<Mutex<VecDeque<u32>>>);

#[derive(Debug)]
pub(crate) struct RunnerService {
    config: Configuration,
    processors: ProcessorQueue,
}

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

impl RunnerService {
    pub fn new(config: Configuration) -> RunnerService {
        let p: Vec<u32> = (0..config.num_cpus).collect();
        Self {
            config: config,
            // builddir: PathBuf::from(builddir.as_ref()).join("build"),
            processors: ProcessorQueue::new(p.into()),

        }
    }
}

#[tonic::async_trait]
impl Runner for RunnerService {
    #[tracing::instrument(skip_all)]
    async fn check_readiness(
        &self,
        request: tonic::Request<CheckReadinessRequest>,
    ) -> TonicResult<tonic::Response<()>> {
        let readyreq = request.get_ref();

        trace!("CheckReadiness = {:?}", request);

        if self.config.build_directory_path.join(&readyreq.path).exists() {
            trace!("CheckReadiness.path exists = {:?}", readyreq.path);
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
        let builddir = self.config.build_directory_path.clone();

        let childtask: JoinHandle<TonicResult<ExitResources>> = tokio::spawn(async move {
            let processor = procque.take_cpu().await?;
            let mut child = spawn_child(processor, builddir, &run)?;
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
