use std::fs::File;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use tonic::Status;
use nix::sys::wait::WaitPidFlag;

use crate::buildbarn_runner::RunRequest;


fn workdir_file(run: &RunRequest, wdname: &String) -> Result<File, tonic::Status> {
    let wdpath: PathBuf = [
        &run.input_root_directory,
        &run.working_directory,
        &wdname,
    ].iter().collect();

    File::create(wdpath).or(Err(Status::internal("Failed to create stdout")))
}

fn try_waitpid(pid: nix::unistd::Pid) -> Result<Option<ExitStatus>, std::io::Error> {
    // Returns Err() if a non-recoverable failure
    // Ok(None) if child is still alive, try again later
    // Ok(Some(ExitStatus)) if child has exited (normally or by signal)

    use nix::sys::wait::WaitStatus::*;
    let waitflags = WaitPidFlag::WNOHANG;

    match nix::sys::wait::waitpid(Some(pid), Some(waitflags)) {
        // Ok(Exited(x, status)) => {
        //     assert!(x == pid);
        //     return Ok(Some(ExitStatus::Exited(status as i8)));
        // },
        // Ok(Signaled(x, sig, core)) => {
        //     assert!(x == pid);
        //     println!("wait {} sig {} core {}", x, sig as i32, core);
        //     return Ok(Some(ExitStatus::Signaled(sig, core)));
        // },
        Ok(Continued(_)) => Ok(None),
        Ok(Stopped(_, _)) => Ok(None),
        Ok(PtraceSyscall(..)) => return Ok(None),
        Ok(StillAlive) => return Ok(None),
        Ok(_) => return Ok(None),  // What else is there to match?
        Err(nix::Error::EINTR) => return Ok(None),
        // Err(nix::Error::UnsupportedOperation) => {
        //     return Err(std::io::Error::new(std::io::ErrorKind::Other,
        //         "nix error: unsupported operation"));
        // },
        Err(e) => {
            return Err(std::io::Error::from(e));
        },
    }
}

pub async fn wait_child(child: &mut Child) -> Result<ExitStatus, tonic::Status> {
    loop {
        println!("w{} ", child.id());

        match child.try_wait() {
            Ok(None) => {},
            Ok(Some(e)) => {
                println!("w{} exited {}", child.id(), e);
                return Ok(e);
            },
            Err(e) => {
                println!("w{} err {}", child.id(), e);
                break;
            },
            // Err(_) => break,
        }

        // let pid = nix::unistd::Pid::from_raw(child.pid());
        // match try_waitpid(pid) {
        //     Ok(None) => {},
        //     Ok(Some(e)) => {
        //         println!("w{} exited {}", child.id(), e);
        //         return Ok(e);
        //     },
        //     Err(e) => {
        //         println!("w{} err {}", child.id(), e);
        //         break;
        //     },
        //     // Err(_) => break,
        // }

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    Err(Status::internal("Wait failed"))
}

pub fn spawn_child(run: &RunRequest) -> Result<Child, tonic::Status> {

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

