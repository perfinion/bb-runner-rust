use tonic::{transport::Server, Request, Response, Status};

use buildbarn_runner::runner_server::{Runner, RunnerServer};
use buildbarn_runner::{RunRequest, RunResponse};

pub mod buildbarn_runner {
    tonic::include_proto!("buildbarn.runner");
}

fn main() {
    println!("Hello, world!");
}
