use std::fs::File;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;
use tonic::Result as TonicResult;
use tonic::Status;
use tracing::{self, debug, error, info, warn};

use crate::child::{Child, Command, Wait4};
use crate::proto::runner::RunRequest;
use crate::resource::ResUse;

const WAIT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

fn workdir_file(run: &RunRequest, wdname: &String) -> TonicResult<File> {
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
#[tracing::instrument(ret, fields(child = %child.id()))]
pub async fn wait_child(child: &mut Child, token: CancellationToken) -> TonicResult<ResUse> {
    let mut sig = signal(SignalKind::child())?;
    let mut interval = tokio::time::interval(WAIT_INTERVAL);
    let mut kill_sent: bool = false;

    loop {
        // The first tick() always finishes immediately, so we can try the child right away in case
        // it has already finished.
        tokio::select! {
            _ = sig.recv() => {
                debug!("Received SIGCHILD");
            }
            _ = interval.tick() => {}
            _ = token.cancelled(), if !kill_sent => {
                // The token was cancelled, send SIGKILL to start cleanup
                // Only need to kill the direct child, it is pid1 in the PID namespace which forces
                // cleanup of all processes in the namespace.
                match child.kill() {
                    Ok(_) => kill_sent = true,
                    _ => {},
                }
            }
        };

        info!(
            pid = child.id(),
            cancelled = token.is_cancelled(),
            kill_sent = kill_sent,
            "waiting"
        );
        match child.try_wait4() {
            Ok(None) => {}
            Ok(Some(e)) => return Ok(e),
            Err(e) => {
                error!(pid = child.id(), "wait error {}", e);
                break;
            }
        }
    }

    error!(pid = child.id(), "Failed to wait for child {}", child.id());
    Err(Status::internal("Wait failed"))
}

#[tracing::instrument(skip(run))]
pub fn spawn_child(processor: u32, run: &RunRequest) -> TonicResult<Child> {
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

    warn!("Running cmd: {:?} {:?}", arg0, &run.arguments[1..]);

    let stdout_file = workdir_file(&run, &run.stdout_path)?;
    let stderr_file = workdir_file(&run, &run.stderr_path)?;

    let mut command = std::process::Command::new(&arg0);
    command.args(&run.arguments[1..]);
    command.current_dir(&cwd);
    command.env_clear();
    command.envs(&run.environment_variables);
    command.stdin(Stdio::null());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());

    let cgname = format!("{processor}");
    Command::from(command)
        .stdout(stdout_file)
        .stderr(stderr_file)
        .hostname("localhost")
        .cgroup(cgname.as_str())
        .spawn()
        .map_err(|_| Status::internal("Failed to spawn child"))
}
