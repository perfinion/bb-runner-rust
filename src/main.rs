#![cfg_attr(not(unix), allow(unused_imports))]

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tonic::{transport::Server, Status};
use std::io::{Read, Write};
use std::fs::File;

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

        let mut child = spawn_child(&run).await.unwrap();

        // if let Ok(mut c) = command.spawn() {
        //     //child.wait().expect("command wasn't running");
        //     child = c;
        //     println!("Child has finished its execution!");
        // } else {
        //     return Err(Status::internal("Spawn failed"));
        // }

        // let mut child = match command.spawn() {
        //     Ok(mut c) => c,
        //     Err(_) => return Err(Status::internal("Spawn failed")),
        // };

        println!("Started process: {}", child.id());

        let exit_status = match child.wait() {
            Ok(e) => e,
            Err(_) => return Err(Status::internal("Wait failed")),
        };

        let mut stdout = String::new();
        match child.stdout {
            Some(mut s) => { let _ = s.read_to_string(&mut stdout); },
            None => {},
        };

        let mut stderr = String::new();
        match child.stderr {
            Some(mut s) => { let _ = s.read_to_string(&mut stderr); },
            None => {},
        };

        let stdout_path: PathBuf = [
            &run.input_root_directory,
            &run.working_directory,
            &run.stdout_path,
        ].iter().collect();
        let mut stdout_file = File::create(stdout_path).unwrap();

        let stderr_path: PathBuf = [
            &run.input_root_directory,
            &run.working_directory,
            &run.stderr_path,
        ].iter().collect();
        let mut stderr_file = File::create(stderr_path).unwrap();

        println!("Return: {:?}", exit_status.code());
        println!("==== Command stdout: ====");
        println!("{}", stdout);
        let _ = stdout_file.write_all(stdout.as_bytes());
        println!("==== Command stderr: ====");
        println!("{}", stderr);
        let _ = stderr_file.write_all(stderr.as_bytes());
        println!("==== End ====");

        let mut runresp = RunResponse::default();
        match exit_status.code() {
            Some(code) => runresp.exit_code = code,
            None => return Err(Status::internal("No Exit Code")),
        }

        Ok(tonic::Response::new(runresp))
    }
}

async fn spawn_child(run: &RunRequest) -> Result<std::process::Child, tonic::Status> {

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

    let mut command = Command::new(&arg0);
    command.args(&run.arguments[1..]);
    command.current_dir(&cwd);
    command.env_clear();
    command.envs(&run.environment_variables);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    // let mut child;
    if let Ok(c) = command.spawn() {
        //child.wait().expect("command wasn't running");
        // child = c;
        println!("Child has finished its execution!");
        return Ok(c);
    } else {
        return Err(Status::internal("Spawn failed"));
    }

    // return Ok(child);
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
