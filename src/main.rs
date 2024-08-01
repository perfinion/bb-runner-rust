#![cfg_attr(not(unix), allow(unused_imports))]

use std::fs::File;
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

        let mut child = spawn_child(&run)?;
        let pid = child.id();
        println!("Started process: {}", pid);

        let exit_status = wait_child(&mut child).await;

        let exit_code = match exit_status {
            Ok(e) => e.code(),
            Err(_) => Some(255),
        };
        println!("\nChild {} exit = {:?}", pid, exit_code);

        let mut runresp = RunResponse::default();
        match exit_code {
            Some(code) => runresp.exit_code = code,
            None => return Err(Status::internal("No Exit Code")),
        }

        Ok(tonic::Response::new(runresp))
    }
}

fn workdir_file(run: &RunRequest, wdname: &String) -> Result<File, tonic::Status> {
    let wdpath: PathBuf = [
        &run.input_root_directory,
        &run.working_directory,
        &wdname,
    ].iter().collect();

    File::create(wdpath).or(Err(Status::internal("Failed to create stdout")))
}

async fn wait_child(child: &mut std::process::Child) -> Result<std::process::ExitStatus, tonic::Status> {
    loop {
        match child.try_wait() {
            Ok(Some(e)) => return Ok(e),
            Ok(None) => {},
            Err(_) => break,
        };
        println!("w{} ", child.id());
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }
    Err(Status::internal("Wait failed"))
}

fn spawn_child(run: &RunRequest) -> Result<std::process::Child, tonic::Status> {

    let cwd: PathBuf = [
        &run.input_root_directory,
        &run.working_directory,
    ].iter().collect();

    let arg0: PathBuf = [
        &run.input_root_directory,
        &run.working_directory,
        &run.arguments[0],
    ].iter().collect();

    println!("Running cmd: {:?} {:?}", arg0, &run.arguments[1..]);

    let stdout_file = workdir_file(&run, &run.stdout_path)?;
    let stderr_file = workdir_file(&run, &run.stderr_path)?;

    let mut command = Command::new(&arg0);
    command.args(&run.arguments[1..]);
    command.current_dir(&cwd);
    command.env_clear();
    command.envs(&run.environment_variables);
    command.stdin(Stdio::null());
    command.stdout(stdout_file);
    command.stderr(stderr_file);

    match command.spawn() {
        Ok(mut child) => {
            drop(child.stdin.take());
            Ok(child)
        },
        Err(_) => Err(Status::internal("Failed to spawn child")),
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
