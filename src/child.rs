use std::fs::{File, OpenOptions};
use std::io::{Error, Result, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{self, ExitStatus};
use std::time::Duration;

use tracing::{error, info, trace};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::libc::{self, c_uint, ifreq, pid_t, timeval};
use nix::mount::{self, MsFlags};
use nix::sched::{self, CloneFlags};
use nix::sys::prctl;
use nix::sys::signal::{self, SaFlags, SigHandler, SigSet, SigmaskHow, Signal};
use nix::sys::socket::{self, AddressFamily, SockFlag, SockProtocol, SockType};
use nix::unistd::{self, Gid, Pid, Uid};

use crate::mmaps::StackMap;
use crate::mounts::{MntEntOpener, MntEntWrapper};
use crate::resource::{ExitResources, ResourceUsage};

const RSS_MULTIPLIER: u64 = if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
    1
} else {
    1024
};

/// Add wait for a process and return the resources it used.
pub(crate) trait Wait4 {
    /// As for [`wait`], it waits for the child to exit completely,
    /// returning the status that it exited with and an estimate of
    /// the time and memory resources it used.
    ///
    /// Like [`try_wait`], the stdin handle is not dropped.
    /// closed before waiting, refer to [`wait`] for the rationale
    /// for it.
    ///
    /// [`try_wait`]: std::process::Child::try_wait
    fn try_wait4(&mut self) -> Result<Option<ExitResources>>;
}

#[derive(Debug)]
pub(crate) struct Command {
    inner: process::Command,
    stdout: Option<File>,
    stderr: Option<File>,
    hostname: Option<String>,
    cgroup: Option<String>,
    cgroup_root: Option<PathBuf>,
    cgroup_path: Option<String>,
    mem_max: Option<u32>,
    namespaces: CloneFlags,
    rw_paths: Vec<String>,
    hidden_paths: Vec<String>,
}

struct ChildData<'a> {
    cmd: &'a mut process::Command,
    read_pipe: BorrowedFd<'a>,
    stdout: Option<RawFd>,
    stderr: Option<RawFd>,
    hostname: Option<&'a str>,
    rw_paths: &'a Vec<String>,
    hidden_paths: &'a Vec<String>,
}

impl std::convert::From<process::Command> for Command {
    fn from(source: process::Command) -> Self {
        Self {
            inner: source,
            stdout: None,
            stderr: None,
            hostname: None,
            cgroup: None,
            cgroup_root: None,
            cgroup_path: None,
            mem_max: None,
            namespaces: CloneFlags::CLONE_NEWPID
                | CloneFlags::CLONE_NEWIPC
                | CloneFlags::CLONE_NEWNET
                | CloneFlags::CLONE_NEWNS
                | CloneFlags::CLONE_NEWUSER,
            rw_paths: Vec::new(),
            hidden_paths: Vec::new(),
        }
    }
}

impl Command {
    pub fn spawn(&mut self) -> Result<Child> {
        let (read_pipe, write_pipe) = unistd::pipe2(OFlag::O_CLOEXEC)?;

        let mut child_data = ChildData {
            cmd: &mut self.inner,
            read_pipe: read_pipe.as_fd(),
            stdout: self.stdout.as_ref().map(|s| s.as_raw_fd()),
            stderr: self.stderr.as_ref().map(|s| s.as_raw_fd()),
            hostname: self.hostname.as_ref().map(String::as_ref),
            rw_paths: self.rw_paths.as_ref(),
            hidden_paths: self.hidden_paths.as_ref(),
        };

        let pid = clone_pid1(self.namespaces, &mut child_data)?;
        drop(read_pipe);

        write_uid_map(pid, unistd::getuid())?;
        write_gid_map(pid, unistd::getgid())?;
        if let Some(cg) = self.cgroup.as_ref().map(String::as_ref) {
            crate::cgroup::move_child_cgroup(pid, cg, self.mem_max, self.cgroup_root.as_deref(), self.cgroup_path.as_deref())?;
        }

        unistd::write(write_pipe, b"A")?;

        Ok(Child { pid })
    }

    pub fn stdout(&mut self, f: File) -> &mut Command {
        self.stdout = Some(f);
        self
    }

    pub fn stderr(&mut self, f: File) -> &mut Command {
        self.stderr = Some(f);
        self
    }

    pub fn cgroup<S: Into<String>>(&mut self, cg: S) -> &mut Command {
        self.cgroup = Some(cg.into());
        self.namespaces |= CloneFlags::CLONE_NEWCGROUP;
        self
    }

    pub fn cgroup_root(&mut self, root: PathBuf) -> &mut Command {
        self.cgroup_root = Some(root);
        self
    }

    pub fn cgroup_path<S: Into<String>>(&mut self, path: S) -> &mut Command {
        self.cgroup_path = Some(path.into());
        self
    }

    pub fn memory_max(&mut self, m: u32) -> &mut Command {
        self.mem_max = Some(m);
        self
    }

    pub fn hostname(&mut self, hostname: &str) -> &mut Command {
        self.hostname = Some(hostname.to_string());
        self.namespaces |= CloneFlags::CLONE_NEWUTS;
        self
    }

    pub fn rw_path<S: Into<String>>(&mut self, path: S) -> &mut Command {
        self.rw_paths.push(path.into());
        self
    }

    pub fn rw_paths(&mut self, paths: &[String]) -> &mut Command {
        self.rw_paths.extend_from_slice(paths);
        self
    }

    pub fn hidden_paths(&mut self, paths: &[String]) -> &mut Command {
        self.hidden_paths.extend_from_slice(paths);
        self
    }
}

fn write_existing_file<P: AsRef<Path>, S: AsRef<str>>(path: P, contents: S) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .and_then(|mut f| f.write_all(contents.as_ref().as_bytes()))
}

fn write_uid_map(pid: Pid, outer_uid: Uid) -> Result<()> {
    write_existing_file(format!("/proc/{pid}/uid_map"), format!("0 {outer_uid} 1"))
}

fn write_gid_map(pid: Pid, outer_gid: Gid) -> Result<()> {
    write_existing_file(format!("/proc/{pid}/setgroups"), "deny")?;
    write_existing_file(format!("/proc/{pid}/gid_map"), format!("0 {outer_gid} 1"))
}

/// Resets all signal handlers and masks so nothing is inherited from parents
/// Also sets parent death signal to SIGKILL
fn reset_signals() -> Result<()> {
    prctl::set_pdeathsig(Signal::SIGKILL)?;

    signal::sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)?;

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

    Ok(())
}

fn close_range_fds(first: c_uint) -> Result<()> {
    match unsafe { nix::libc::close_range(first, c_uint::MAX, 0) } {
        0 => Ok(()),
        -1 => Err(Error::from(nix::errno::Errno::last())),
        _ => Err(Error::other("close_range failed")),
    }
}

/// Bind-mount any `rw_path` that is not already a mountpoint onto itself.
/// This creates a separate mount so the parent can be remounted RO while
/// the rw_path stays RW.
fn bind_mount_rw_paths(rw_paths: &[String]) -> Result<()> {
    let mntent = MntEntOpener::new(Path::new("/proc/self/mounts"))?;
    let entries: Vec<MntEntWrapper> = mntent.list_all()?;
    let mountpoints: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.mnt_dir.as_str()).collect();

    for rw in rw_paths {
        if !mountpoints.contains(rw.as_str()) {
            let p = Path::new(rw.as_str());
            if p.exists() {
                trace!("Bind-mounting rw_path {} onto itself", rw);
                mount::mount(
                    Some(p),
                    p,
                    None::<&'static str>,
                    MsFlags::MS_BIND | MsFlags::MS_REC,
                    None::<&'static str>,
                )?;
            }
        }
    }

    Ok(())
}

/// Hide directories by mounting a tmpfs over each path.
/// This makes the original contents inaccessible to the child.
/// Paths that also appear in `rw_paths` are mounted read-write;
/// all others are mounted read-only.
fn mount_hidden_paths(hidden_paths: &[String], rw_paths: &[String]) -> Result<()> {
    for hidden in hidden_paths {
        let p = Path::new(hidden.as_str());
        if p.exists() {
            let mut flags = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
            if !rw_paths.iter().any(|rw| hidden == rw) {
                flags |= MsFlags::MS_RDONLY;
            }
            trace!("Hiding path {} with tmpfs (ro={})", hidden, flags.contains(MsFlags::MS_RDONLY));
            mount::mount(
                Some("tmpfs"),
                p,
                Some("tmpfs"),
                flags,
                Some("size=0"),
            )?;
        }
    }

    Ok(())
}

fn remount_all_readonly(rw_paths: &[String]) -> Result<()> {
    bind_mount_rw_paths(rw_paths)?;

    // Re-read mounts now that we may have added new bind mounts.
    let mntent = MntEntOpener::new(Path::new("/proc/self/mounts"))?;

    let entries: Vec<MntEntWrapper> = mntent.list_all()?;
    for ent in entries {
        trace!("Mount Entry = {} = {:?}", ent.mnt_dir, ent);
        if rw_paths.iter().any(|x| ent.mnt_dir.starts_with(x)) {
            trace!("Leaving ReadWrite Entry = {} = {:?}", ent.mnt_dir, ent);
            continue;
        }

        // https://github.com/bazelbuild/bazel/blob/788b6080f54c6ca5093526023dfd9b12b90403f8/src/main/tools/linux-sandbox-pid1.cc#L346
        // MS_REMOUNT does not allow us to change certain flags. This means, we have
        // to first read them out and then pass them in back again. There seems to
        // be no better way than this (an API for just getting the mount flags of a
        // mount entry as a bitmask would be great).

        match mount::mount(
            None::<&'static str>,
            Path::new(ent.mnt_dir.as_str()),
            None::<&'static str>,
            ent.mnt_flags | MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY,
            None::<&'static str>,
        ) {
            Ok(_) => {}
            Err(Errno::EACCES) | Err(Errno::EPERM) | Err(Errno::EINVAL) | Err(Errno::ENOENT)
            | Err(Errno::ESTALE) | Err(Errno::ENODEV) => {
                // See: https://github.com/bazelbuild/bazel/blob/788b6080f54c6ca5093526023dfd9b12b90403f8/src/main/tools/linux-sandbox-pid1.cc#L376
                info!("Failed to remount {}, ignored", ent.mnt_dir);
            }
            Err(e) => {
                error!("Failure to remount {}, errno = {}", ent.mnt_dir, e);
                return Err(e.into());
            }
        }
    }

    Ok(())
}

fn net_loopback_up() -> Result<()> {
    let sock: OwnedFd = socket::socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None::<SockProtocol>,
    )?;

    let mut ifr: ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in ifr.ifr_name.iter_mut().zip(b"lo\0".iter()) {
        *dst = *src as _;
    }

    unsafe {
        ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as i16;
        libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFFLAGS, &ifr);
    };

    Ok(())
}

fn child_pid1(child_data: &mut ChildData) -> Result<isize> {
    let pid = Pid::this();
    nix::unistd::setpgid(pid, pid)?;
    reset_signals()?;

    info!("In child, pid = {}, ppid = {}", pid, Pid::parent());

    // Block until the parent has configured our uid_map
    let mut buf = [0; 4];
    let _ = unistd::read(child_data.read_pipe.as_raw_fd(), &mut buf);
    info!("Read from pipe: {:?}", buf);

    // cd / before mounting in case we were keeping something busy
    unistd::chdir("/")?;

    // Fully isolate our namespace from parent
    mount::mount(
        None::<&'static str>,
        "/",
        None::<&'static str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&'static str>,
    )?;

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

    remount_all_readonly(child_data.rw_paths)?;
    mount_hidden_paths(child_data.hidden_paths, child_data.rw_paths)?;
    net_loopback_up()?;

    info!("From child!! pid = {} uid = {}", pid, unistd::getuid());

    // Setup child stdio and close everything else
    if let Some(stdout) = child_data.stdout {
        let _ = unistd::dup2(stdout, libc::STDOUT_FILENO)?;
    }
    if let Some(stderr) = child_data.stderr {
        let _ = unistd::dup2(stderr, libc::STDERR_FILENO)?;
    }
    close_range_fds((libc::STDERR_FILENO as c_uint) + 1)?;

    let mut child = child_data.cmd.spawn()?;

    // File descriptors are for child, close everything in pid1
    close_range_fds(0)?;
    let exitstatus = child.wait()?;

    // Child was killed, kill ourselves the same way to propagate upwards
    if let Some(sigi32) = exitstatus.signal() {
        let sig = Signal::try_from(sigi32)?;
        signal::kill(unistd::getpid(), Some(sig))?;
    }

    // Return childs code upwards
    Ok(exitstatus.code().ok_or(Error::other("Child failed"))? as isize)
}

fn clone_pid1(clone_flags: CloneFlags, child_data: &mut ChildData) -> Result<Pid> {
    let stack = StackMap::new(1024 * 1024)?; // 1 MB stacks
    info!("Stack: {:?}", stack);

    let sig = Some(Signal::SIGCHLD as i32);

    let child_pid = unsafe {
        sched::clone(
            Box::new(move || child_pid1(child_data).unwrap_or(-1)),
            stack.as_slice()?,
            clone_flags,
            sig,
        )
    };

    Ok(child_pid?)
}

#[derive(Debug)]
pub(crate) struct Child {
    pid: Pid,
}

impl Child {
    pub fn id(&self) -> u32 {
        pid_t::from(self.pid) as u32
    }

    pub fn kill(&mut self) -> Result<()> {
        Ok(signal::kill(self.pid, Some(Signal::SIGKILL))?)
    }
}

#[allow(clippy::useless_conversion)]
fn timeval_to_duration(val: timeval) -> Duration {
    let v = i64::from(val.tv_sec) * 1_000_000 + i64::from(val.tv_usec);
    Duration::from_micros(v as u64)
}

fn wait4(pid: pid_t, options: i32) -> Result<Option<ExitResources>> {
    let mut status = 0;
    let mut rusage = std::mem::MaybeUninit::zeroed();

    let r = unsafe { libc::wait4(pid, &mut status, options, rusage.as_mut_ptr()) };

    if r < 0 {
        Err(Error::last_os_error())
    } else if r == 0 {
        Ok(None)
    } else {
        let rusage = unsafe { rusage.assume_init() };

        Ok(Some(ExitResources {
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
    fn try_wait4(&mut self) -> Result<Option<ExitResources>> {
        let pid = self.id() as i32;

        wait4(pid, libc::WNOHANG)
    }
}
