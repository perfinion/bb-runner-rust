use std::convert::AsRef;
use std::fs::{self, File};
use std::path::Path;
use std::process::Stdio;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;
use tonic::Result as TonicResult;
use tonic::Status;
use tracing::{self, debug, error, info, warn};

use crate::child::{Child, Command, Wait4};
use crate::config::Configuration;
use crate::proto::runner::RunRequest;
use crate::resource::ExitResources;

const WAIT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

fn builddir_file<P: AsRef<Path>>(builddir: P, fname: &String) -> TonicResult<File> {
    let wdpath = builddir.as_ref().join(fname);

    File::create(&wdpath).or(Err(Status::internal(format!(
        "Failed to create file: {:?}",
        &wdpath
    ))))
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
pub(crate) async fn wait_child(
    child: &mut Child,
    token: CancellationToken,
) -> TonicResult<ExitResources> {
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
                if child.kill().is_ok() {
                    kill_sent = true;
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
pub(crate) fn spawn_child(
    processor: u32,
    child_cfg: &Configuration,
    run: &RunRequest,
) -> TonicResult<Child> {
    let builddir: &Path = child_cfg.build_directory_path.as_ref();

    let ird = builddir.join(&run.input_root_directory);
    let cwd = ird.join(&run.working_directory);
    let arg0 = cwd.join(&run.arguments[0]);
    let tmpdir = builddir.join(&run.temporary_directory).join("tmp");
    let homedir = builddir.join(&run.temporary_directory).join("home");
    fs::create_dir(&tmpdir).map_err(|_| Status::internal("Failed to create tmpdir"))?;
    fs::create_dir(&homedir).map_err(|_| Status::internal("Failed to create homedir"))?;

    warn!("Running cmd: {:?} {:?}", arg0, &run.arguments[1..]);

    let stdout_file = builddir_file(builddir, &run.stdout_path)?;
    let stderr_file = builddir_file(builddir, &run.stderr_path)?;

    let mut stdcmd = std::process::Command::new(&arg0);
    stdcmd.args(&run.arguments[1..]);
    stdcmd.current_dir(&cwd);
    stdcmd.env_clear();
    stdcmd.envs(&run.environment_variables);
    stdcmd.env("TMP", &tmpdir);
    stdcmd.env("HOME", &homedir);
    stdcmd.stdin(Stdio::null());
    stdcmd.stdout(Stdio::inherit());
    stdcmd.stderr(Stdio::inherit());

    let mut c = Command::from(stdcmd);
    c.stdout(stdout_file);
    c.stderr(stderr_file);
    c.hostname("localhost");
    c.cgroup(processor.to_string());
    c.memory_max(child_cfg.memory_max);
    c.rw_paths(&child_cfg.rw_paths);

    if let Some(p) = homedir.to_str() {
        c.rw_path(p);
    }

    if let Some(p) = ird.to_str() {
        c.rw_path(p);
    }

    if let Some(p) = tmpdir.to_str() {
        c.rw_path(p);
    }

    c.spawn().map_err(|_| Status::internal("Failed to spawn child"))
}
