#![cfg_attr(not(unix), allow(unused_imports))]

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tonic::{transport::Server, Status};

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio_stream::wrappers::UnixListenerStream;
#[cfg(unix)]
use tonic::transport::server::UdsConnectInfo;

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
        println!("\t{:?}", conn_info);
        println!("\targuments = {:?}", run.arguments);
        println!("\tenvironment_variables = {:?}", run.environment_variables);
        println!("\tworking_directory = {:?}", run.working_directory);
        println!("\tstdout_path = {:?}", run.stdout_path);
        println!("\tstderr_path = {:?}", run.stderr_path);
        println!("\tinput_root_directory = {:?}", run.input_root_directory);
        println!("\ttemporary_directory = {:?}", run.temporary_directory);
        println!("\tserver_logs_directory = {:?}", run.server_logs_directory);

        let mut cmd_cwd = PathBuf::new();
        cmd_cwd.push(&run.input_root_directory);
        cmd_cwd.push(&run.working_directory);

        let mut cmdpath = PathBuf::new();
        cmdpath.push(&run.input_root_directory);
        cmdpath.push(&run.working_directory);
        cmdpath.push(&run.arguments[0]);

        println!("Running cmd: {:?}", cmdpath);

        // let command = Command::new(&run.arguments[0])
        let command = Command::new(&cmdpath)
            .args(&run.arguments[1..])
            .current_dir(&cmd_cwd)
            .env_clear()
            .envs(&run.environment_variables)
            .output()
            .expect("Failed to execute command");
            // .stdout(Stdio::piped())

        println!("Return: {}", command.status);
        println!("==== Command stdout: ====");
        println!("{:?}", String::from_utf8_lossy(&command.stdout.as_slice()));
        println!("==== Command stderr: ====");
        println!("{:?}", String::from_utf8_lossy(&command.stderr.as_slice()));
        println!("==== End ====");

        let mut runresp = RunResponse::default();
        match command.status.code() {
            Some(code) => runresp.exit_code = code,
            None => return Err(Status::internal("No Exit Code")),
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
