use tonic::{transport::Server, Request, Response, Status};

use buildbarn_runner::runner_server::{Runner, RunnerServer};
use buildbarn_runner::{CheckReadinessRequest, RunRequest, RunResponse};

pub mod buildbarn_runner {
    tonic::include_proto!("buildbarn.runner");
}


#[derive(Debug)]
struct RunnerService;


#[tonic::async_trait]
impl Runner for RunnerService {
    async fn check_readiness(
        &self,
        request: tonic::Request<CheckReadinessRequest>,
    ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
        unimplemented!{}
    }
    async fn run(
        &self,
        request: tonic::Request<RunRequest>,
    ) -> std::result::Result<tonic::Response<RunResponse>, tonic::Status> {
        unimplemented!{}
    }
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Hello, world from bb_runner!");
    let addr = "[::1]:10000".parse().unwrap();

    let bb_runner = RunnerService {
        // features: Arc::new(data::load()),
    };

    println!("Service created!");

    let svc = RunnerServer::new(bb_runner);

    println!("Server created!");

    println!("Running Server ...");

    Server::builder()
        .add_service(svc)
        .serve(addr)
        .await?;

    Ok(())
}
