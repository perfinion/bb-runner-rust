use std::fs::File;
use std::io::{Error, Result, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{self, ExitStatus};
use std::thread::sleep;
use std::time::Duration;

use tracing::info;

use nix::fcntl::OFlag;
use nix::libc::{self, pid_t, timeval};
use nix::mount::{self, MsFlags};
use nix::sched::{self, CloneFlags};
use nix::sys::prctl;
use nix::sys::signal::{self, SaFlags, SigHandler, SigSet, SigmaskHow, Signal};
use nix::unistd::{self, Gid, Pid, Uid};

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
    hostname: Option<String>,
    namespaces: CloneFlags,
}

struct ChildData<'a> {
    cmd: &'a mut process::Command,
    read_pipe: BorrowedFd<'a>,
    hostname: Option<&'a str>,
}

impl std::convert::From<process::Command> for Command {
    fn from(source: process::Command) -> Self {
        Self {
            inner: source,
            hostname: None,
            namespaces: CloneFlags::empty(),
        }
    }
}

impl Command {
    pub fn spawn(&mut self) -> Result<Child> {
        let (read_pipe, write_pipe) = unistd::pipe2(OFlag::O_CLOEXEC)?;

        let mut child_data = ChildData {
            cmd: &mut self.inner,
            read_pipe: read_pipe.as_fd(),
            hostname: self.hostname.as_ref().map(String::as_ref),
        };

        let pid = clone_pid1(&mut child_data)?;
        drop(read_pipe);

        write_uid_map(pid, unistd::getuid())?;
        write_gid_map(pid, unistd::getgid())?;
        sleep(Duration::from_secs(10));

        unistd::write(write_pipe, "A".as_bytes()).expect("start child");

        Ok(Child { pid })
    }

    pub fn hostname(&mut self, hostname: &str) -> &mut Command {
        self.hostname = Some(hostname.to_string());
        self.namespaces |= CloneFlags::CLONE_NEWUTS;
        self
    }
}

fn write_uid_map(pid: Pid, outer_uid: Uid) -> Result<()> {
    let uid_map_path = format!("/proc/{pid}/uid_map");
    let buf = format!("0 {outer_uid} 1");
    File::create(uid_map_path).and_then(|mut f| f.write_all(buf.as_bytes()))
}

fn write_gid_map(pid: Pid, outer_gid: Gid) -> Result<()> {
    let setgroups_path = format!("/proc/{pid}/setgroups");
    File::create(setgroups_path).and_then(|mut f| f.write_all(b"deny"))?;

    let gid_map_path = format!("/proc/{pid}/gid_map");
    let buf = format!("0 {outer_gid} 1");
    File::create(gid_map_path).and_then(|mut f| f.write_all(buf.as_bytes()))
}

/// Resets all signal handlers and masks so nothing is inherited from parents
/// Also sets parent death signal to SIGKILL
fn reset_signals() -> () {
    prctl::set_pdeathsig(Signal::SIGKILL).expect("pdeathsig");

    signal::sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)
        .expect("unblocking signals");

    let sadfl = signal::SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    let saign = signal::SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    for s in Signal::iterator() {
        match s {
            // SIGKILL and SIGSTOP are not handleable
            Signal::SIGKILL | Signal::SIGSTOP => {}
            // Ignore TTY signals
            Signal::SIGTTIN | Signal::SIGTTOU => unsafe {
                let _ = signal::sigaction(s, &saign);
            },
            // Dont care what they previously were
            s => unsafe {
                let _ = signal::sigaction(s, &sadfl);
            },
        }
    }
}

fn child_pid1(child_data: &mut ChildData) -> Result<isize> {
    let pid = Pid::this();
    nix::unistd::setpgid(pid, pid)?;
    reset_signals();

    info!("In child, pid = {}, ppid = {}", pid, Pid::parent());

    // Block until the parent has configured our uid_map
    let mut buf = [0; 8];
    let _ = unistd::read(child_data.read_pipe.as_raw_fd(), &mut buf);
    info!("Read from pipe: {:?}", buf);

    // cd / before mounting in case we were keeping something busy
    unistd::chdir("/")?;
    if let Some(h) = child_data.hostname {
        unistd::sethostname(h)?;
    }

    let mount_flags = MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV;
    mount::mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        mount_flags,
        None::<&'static str>,
    )?;

    for i in 0..2 {
        let pid = unistd::getpid();
        let uid = unistd::getuid();
        info!("From child!! {} pid = {} uid = {}", i, pid, uid);
        sleep(Duration::from_millis(1000));
    }

    let exitstatus = child_data.cmd.spawn()?.wait()?;

    // Child was killed, kill ourselves the same way to propagate upwards
    if let Some(sigi32) = exitstatus.signal() {
        let sig = Signal::try_from(sigi32)?;
        signal::kill(unistd::getpid(), Some(sig))?;
    }

    // Return childs code upwards
    Ok(exitstatus.code().ok_or(Error::other("Child failed"))? as isize)
}

fn clone_pid1(child_data: &mut ChildData) -> Result<Pid> {
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
            Box::new(move || child_pid1(child_data).unwrap_or(-1)),
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
