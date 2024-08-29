use nix::sched::{unshare, CloneFlags};
use std::io::{Error, Result};
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{self, ExitStatus};
use std::time::Duration;

use crate::proto::resourceusage::PosixResourceUsage;

use nix::libc::timeval;
use nix::libc::{self, pid_t};

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
        self.inner.spawn().map(|mut inner| {
            drop(inner.stdin.take());
            let pid = inner.id();

            Child { inner, pid }
        })
    }
}

#[derive(Debug)]
pub struct Child {
    inner: process::Child,
    pid: u32,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.pid
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
