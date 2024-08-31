use std::io::{Error, Result};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{self, ExitStatus};
use std::thread::sleep;
use std::time::Duration;

use tracing::info;

use nix::fcntl::OFlag;
use nix::libc::{self, pid_t, timeval};
use nix::sched::{self, CloneFlags};
use nix::sys::prctl;
use nix::sys::signal::{self, SaFlags, SigHandler, SigSet, SigmaskHow, Signal};
use nix::mount::{self, MsFlags};
use nix::unistd::{self, Pid};

use crate::proto::resourceusage::PosixResourceUsage;

const RSS_MULTIPLIER: u64 = if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
    1
} else {
    1024
};

/// Resources used by a process
#[derive(Debug)]
pub struct ResourceUsage {
    /// User CPU time used
    ///
    /// Time spent in user-mode
    pub utime: Duration,
    /// System CPU time used
    ///
    /// Time spent in kernel-mode
    pub stime: Duration,
    /// Maximum resident set size, in bytes.
    ///
    /// Zero if not available on the platform.
    pub maxrss: u64,
}

impl Into<PosixResourceUsage> for ResourceUsage {
    fn into(self) -> PosixResourceUsage {
        let mut pbres = PosixResourceUsage::default();
        if let Ok(n) = prost_types::Duration::try_from(self.utime) {
            pbres.user_time = Some(n);
        }

        if let Ok(n) = prost_types::Duration::try_from(self.stime) {
            pbres.system_time = Some(n);
        }

        if let Ok(n) = i64::try_from(self.maxrss) {
            pbres.maximum_resident_set_size = n;
        }

        pbres
    }
}

/// Resources used by a process and its exit status
#[derive(Debug)]
pub struct ResUse {
    /// Same as the one returned by [`wait`].
    ///
    /// [`wait`]: std::process::Child::wait
    pub status: ExitStatus,
    /// Resource used by the process and all its children
    pub rusage: ResourceUsage,
}

/// Add wait for a process and return the resources it used.
pub trait Wait4 {
    /// As for [`wait`], it waits for the child to exit completely,
    /// returning the status that it exited with and an estimate of
    /// the time and memory resources it used.
    ///
    /// Like [`try_wait`], the stdin handle is not dropped.
    /// closed before waiting, refer to [`wait`] for the rationale
    /// for it.
    ///
    /// [`try_wait`]: std::process::Child::try_wait
    fn try_wait4(&mut self) -> Result<Option<ResUse>>;
}

#[derive(Debug)]
pub struct Command {
    inner: process::Command,
    namespaces: CloneFlags,
}

impl std::convert::From<process::Command> for Command {
    fn from(source: process::Command) -> Self {
        Self {
            inner: source,
            namespaces: CloneFlags::empty(),
        }
    }
}

impl Command {
    pub fn spawn(&mut self) -> Result<Child> {
        let (read_pipe, write_pipe) = unistd::pipe2(OFlag::O_CLOEXEC).expect("create pipe failed");
        let ret = clone_pid1(read_pipe).map(|pid| Child { pid });

        sleep(Duration::from_secs(10));

        unistd::write(write_pipe, "A".as_bytes()).expect("start child");

        ret
    }
}

/// Resets all signal handlers and masks so nothing is inherited from parents
/// Also sets parent death signal to SIGKILL
fn reset_signals() -> () {
    prctl::set_pdeathsig(Signal::SIGKILL).expect("pdeathsig");

    signal::sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)
        .expect("unblocking signals");

    let sa = signal::SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    for s in Signal::iterator() {
        match s {
            // SIGKILL and SIGSTOP are not handleable
            Signal::SIGKILL | Signal::SIGSTOP => {}
            // Dont care what they previously were
            s => unsafe {
                let _ = signal::sigaction(s, &sa);
            },
        }
    }
}

fn child_pid1(read_pipe: BorrowedFd) -> isize {
    let pid = unistd::Pid::this();
    nix::unistd::setpgid(pid, pid).expect("setpgid failed");
    reset_signals();

    let mut buf = [0; 8];
    let _ = unistd::read(read_pipe.as_raw_fd(), &mut buf);
    info!("Read from pipe: {:?}", buf);

    // cd / before mounting in case we were keeping something busy
    unistd::chdir("/").expect("cd / failed");
    unistd::sethostname("sandbox").expect("sethostname failed");

    let mount_flags = MsFlags::MS_NOSUID|MsFlags::MS_NOEXEC|MsFlags::MS_NODEV;
    mount::mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        mount_flags,
        None::<&'static str>,
    ).expect("mount proc failed");

    for i in 0..10 {
        let pid = unistd::getpid();
        let uid = unistd::getuid();
        info!("From child!! {} pid = {} uid = {}", i, pid, uid);
        sleep(Duration::from_millis(1000));
    }

    0
}

fn clone_pid1(read_pipe: OwnedFd) -> Result<Pid> {
    const STACK_SIZE: usize = 1024 * 1024;
    let stack: &mut [u8; STACK_SIZE] = &mut [0; STACK_SIZE];

    let sig = Some(Signal::SIGCHLD as i32);
    let clone_flags = CloneFlags::CLONE_NEWCGROUP
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWIPC
        | CloneFlags::CLONE_NEWNET
        | CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWUSER
        | CloneFlags::CLONE_NEWUTS;

    let child_pid = unsafe {
        sched::clone(
            Box::new(move || child_pid1(read_pipe.as_fd())),
            stack,
            clone_flags,
            sig,
        )
    };

    Ok(child_pid?)
}

#[derive(Debug)]
pub struct Child {
    pid: Pid,
}

impl Child {
    pub fn id(&self) -> u32 {
        pid_t::from(self.pid) as u32
    }
}

#[allow(clippy::useless_conversion)]
fn timeval_to_duration(val: timeval) -> Duration {
    let v = i64::from(val.tv_sec) * 1_000_000 + i64::from(val.tv_usec);
    Duration::from_micros(v as u64)
}

fn wait4(pid: pid_t, options: i32) -> Result<Option<ResUse>> {
    let mut status = 0;
    let mut rusage = std::mem::MaybeUninit::zeroed();

    let r = unsafe { libc::wait4(pid, &mut status, options, rusage.as_mut_ptr()) };

    if r < 0 {
        Err(Error::last_os_error())
    } else if r == 0 {
        Ok(None)
    } else {
        let rusage = unsafe { rusage.assume_init() };

        Ok(Some(ResUse {
            status: ExitStatus::from_raw(status),
            rusage: ResourceUsage {
                utime: timeval_to_duration(rusage.ru_utime),
                stime: timeval_to_duration(rusage.ru_stime),
                maxrss: (rusage.ru_maxrss as u64) * RSS_MULTIPLIER,
            },
        }))
    }
}

impl Wait4 for Child {
    fn try_wait4(&mut self) -> Result<Option<ResUse>> {
        let pid = self.id() as i32;

        wait4(pid, libc::WNOHANG)
    }
}
