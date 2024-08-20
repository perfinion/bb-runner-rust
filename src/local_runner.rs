use std::fs::File;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use tokio::signal::unix::{signal, SignalKind};
use tonic::Status;

use crate::child::{ResUse, Wait4};
use crate::proto::runner::RunRequest;

const WAIT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

fn workdir_file(run: &RunRequest, wdname: &String) -> Result<File, tonic::Status> {
    let wdpath: PathBuf = [&run.input_root_directory, &run.working_directory, &wdname]
        .iter()
        .collect();

    File::create(wdpath).or(Err(Status::internal("Failed to create stdout")))
}

/// SIGCHILD signal handlers are global for the whole process, you can't register a handler
/// specifically for one child only.
/// Additionally, the kernel can coalese signals. If two children exit, the kernel is allowed to
/// send only one single SIGCHILD.
/// Epoll on a PidFd would probably be more reliable, try that later.
///
/// buildbarn runner is just responsible for spawning children, It does not _do_ anything that
/// interesting, the children do all the intensive work, so a few extra syscalls every few
/// seconds are basically irrelevant.
///
/// TL;DR: Wait for SIGCHILD, and also just timeout and test once in a while anyway, will
/// eventually reap the child.
pub async fn wait_child(child: &mut Child) -> Result<ResUse, tonic::Status> {
    let mut sig = signal(SignalKind::child())?;
    let mut interval = tokio::time::interval(WAIT_INTERVAL);

    loop {
        // The first tick() always finishes immediately, so we can try the child right away in case
        // it has already finished.
        tokio::select! {
            _ = sig.recv() => {
                println!("Received SIGCHILD");
            }
            _ = interval.tick() => {
                println!("Sleep Finished");
            }
        };

        println!("w{}", child.id());
        match child.try_wait4() {
            Ok(None) => {}
            Ok(Some(e)) => return Ok(e),
            Err(e) => {
                println!("w{} err {}", child.id(), e);
                break;
            }
        }
    }

    Err(Status::internal("Wait failed"))
}

pub fn spawn_child(run: &RunRequest) -> Result<Child, tonic::Status> {
    let cwd: PathBuf = [&run.input_root_directory, &run.working_directory]
        .iter()
        .collect();

    let arg0: PathBuf = [
        &run.input_root_directory,
        &run.working_directory,
        &run.arguments[0],
    ]
    .iter()
    .collect();

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
        }
        Err(_) => Err(Status::internal("Failed to spawn child")),
    }
}
