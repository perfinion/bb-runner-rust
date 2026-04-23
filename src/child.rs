use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Error, Result, Write};
use std::num::NonZeroU64;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
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

use crate::config::NetInterfaceConfig;
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
    mem_max: Option<NonZeroU64>,
    namespaces: CloneFlags,
    rw_paths: Vec<String>,
    hidden_paths: Vec<String>,
    net_interfaces: HashMap<String, NetInterfaceConfig>,
    run_as_user: Option<u32>,
    run_as_group: Option<u32>,
}

struct ChildData<'a> {
    cmd: &'a mut process::Command,
    read_pipe: BorrowedFd<'a>,
    stdout: Option<BorrowedFd<'a>>,
    stderr: Option<BorrowedFd<'a>>,
    hostname: Option<&'a str>,
    rw_paths: &'a Vec<String>,
    hidden_paths: &'a Vec<String>,
    net_interfaces: &'a HashMap<String, NetInterfaceConfig>,
    run_as_user: Option<u32>,
    run_as_group: Option<u32>,
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
            net_interfaces: HashMap::new(),
            run_as_user: None,
            run_as_group: None,
        }
    }
}

impl Command {
    pub fn spawn(&mut self) -> Result<Child> {
        let (read_pipe, write_pipe) = unistd::pipe2(OFlag::O_CLOEXEC)?;

        let mut child_data = ChildData {
            cmd: &mut self.inner,
            read_pipe: read_pipe.as_fd(),
            stdout: self.stdout.as_ref().map(|s| s.as_fd()),
            stderr: self.stderr.as_ref().map(|s| s.as_fd()),
            hostname: self.hostname.as_ref().map(String::as_ref),
            rw_paths: self.rw_paths.as_ref(),
            hidden_paths: self.hidden_paths.as_ref(),
            net_interfaces: &self.net_interfaces,
            run_as_user: self.run_as_user,
            run_as_group: self.run_as_group,
        };

        let pid = clone_pid1(self.namespaces, &mut child_data)?;
        drop(read_pipe);

        write_uid_map(pid, unistd::getuid(), self.run_as_user)?;
        write_gid_map(pid, unistd::getgid(), self.run_as_group)?;
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

    pub fn memory_max(&mut self, m: Option<NonZeroU64>) -> &mut Command {
        self.mem_max = m;
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

    pub fn net_interfaces(&mut self, ifaces: &HashMap<String, NetInterfaceConfig>) -> &mut Command {
        self.net_interfaces.extend(ifaces.iter().map(|(k, v)| (k.clone(), v.clone())));
        self
    }

    pub fn run_as_user(&mut self, uid: Option<u32>) -> &mut Command {
        self.run_as_user = uid;
        self
    }

    pub fn run_as_group(&mut self, gid: Option<u32>) -> &mut Command {
        self.run_as_group = gid;
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

fn write_uid_map(pid: Pid, outer_uid: Uid, run_as_uid: Option<u32>) -> Result<()> {
    let map = match run_as_uid {
        // Build runs as root inside; only uid 0 needed.
        Some(0) | None => format!("0 {} 1", outer_uid),
        // Outer is root: a single contiguous 1:1 range covers both uid 0 and the target.
        Some(uid) if outer_uid.as_raw() == 0 => format!("0 0 {}", uid + 1),
        // Outer is non-root: two ranges. Requires CAP_SETUID or subordinate UIDs.
        Some(uid) => format!("0 {} 1\n{} {} 1", outer_uid, uid, uid),
    };
    write_existing_file(format!("/proc/{pid}/uid_map"), map)
}

fn write_gid_map(pid: Pid, outer_gid: Gid, run_as_gid: Option<u32>) -> Result<()> {
    write_existing_file(format!("/proc/{pid}/setgroups"), "deny")?;
    let map = match run_as_gid {
        Some(0) | None => format!("0 {} 1", outer_gid),
        Some(gid) if outer_gid.as_raw() == 0 => format!("0 0 {}", gid + 1),
        Some(gid) => format!("0 {} 1\n{} {} 1", outer_gid, gid, gid),
    };
    write_existing_file(format!("/proc/{pid}/gid_map"), map)
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
    // Use raw syscall – libc::close_range() is not available under musl.
    match unsafe { libc::syscall(libc::SYS_close_range, first, c_uint::MAX, 0u32) } {
        0 => Ok(()),
        -1 => Err(Error::last_os_error()),
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
        libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFFLAGS as _, &ifr);
    };

    Ok(())
}

/// Helper: set an interface name into an ifreq struct.
fn set_ifr_name(ifr: &mut ifreq, name: &str) -> Result<()> {
    let name_bytes = name.as_bytes();
    if name_bytes.len() >= libc::IFNAMSIZ {
        return Err(Error::other("interface name too long"));
    }
    for (dst, src) in ifr.ifr_name.iter_mut().zip(name_bytes.iter()) {
        *dst = *src as _;
    }
    Ok(())
}

/// Helper: build a sockaddr_in from a host-byte-order IPv4 address.
fn make_sockaddr_in(addr: u32) -> libc::sockaddr_in {
    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    sa.sin_addr.s_addr = addr.to_be();
    sa
}

/// Create a dummy network interface using a netlink RTM_NEWLINK message.
fn netlink_create_dummy(name: &str) -> Result<()> {
    // ifinfomsg is not in the libc crate
    #[repr(C)]
    struct Ifinfomsg {
        ifi_family: u8,
        _pad: u8,
        ifi_type: u16,
        ifi_index: i32,
        ifi_flags: u32,
        ifi_change: u32,
    }

    let nl_fd = unsafe {
        libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE)
    };
    if nl_fd < 0 {
        return Err(Error::last_os_error());
    }
    // Wrap so it auto-closes on all exit paths.
    let nl_fd = unsafe { OwnedFd::from_raw_fd(nl_fd) };

    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as u16;
    if unsafe {
        libc::bind(
            nl_fd.as_raw_fd(),
            &sa as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    } < 0
    {
        return Err(Error::last_os_error());
    }

    // Helper: round up to the next multiple of 4 (NLA alignment).
    fn nla_align(len: usize) -> usize {
        (len + 3) & !3
    }

    let name_bytes = name.as_bytes();
    let ifname_nla_len = std::mem::size_of::<libc::nlattr>() + name_bytes.len() + 1; // +1 for NUL

    let info_kind_payload = b"dummy\0";
    let info_kind_nla_len = std::mem::size_of::<libc::nlattr>() + info_kind_payload.len();
    let linkinfo_nla_len = std::mem::size_of::<libc::nlattr>() + nla_align(info_kind_nla_len);

    let hdr_len = std::mem::size_of::<libc::nlmsghdr>()
        + nla_align(std::mem::size_of::<Ifinfomsg>())
        + nla_align(ifname_nla_len)
        + nla_align(linkinfo_nla_len);

    let mut buf = vec![0u8; hdr_len];
    let mut offset = 0;

    // nlmsghdr
    let hdr = unsafe { &mut *(buf.as_mut_ptr().add(offset) as *mut libc::nlmsghdr) };
    hdr.nlmsg_len = hdr_len as u32;
    hdr.nlmsg_type = libc::RTM_NEWLINK;
    hdr.nlmsg_flags =
        (libc::NLM_F_REQUEST | libc::NLM_F_CREATE | libc::NLM_F_EXCL | libc::NLM_F_ACK) as u16;
    hdr.nlmsg_seq = 1;
    hdr.nlmsg_pid = 0;
    offset += std::mem::size_of::<libc::nlmsghdr>();

    // ifinfomsg
    let ifi = unsafe { &mut *(buf.as_mut_ptr().add(offset) as *mut Ifinfomsg) };
    ifi.ifi_family = libc::AF_UNSPEC as u8;
    offset += nla_align(std::mem::size_of::<Ifinfomsg>());

    // IFLA_IFNAME
    let nla = unsafe { &mut *(buf.as_mut_ptr().add(offset) as *mut libc::nlattr) };
    nla.nla_len = ifname_nla_len as u16;
    nla.nla_type = libc::IFLA_IFNAME as u16;
    let payload_start = offset + std::mem::size_of::<libc::nlattr>();
    buf[payload_start..payload_start + name_bytes.len()].copy_from_slice(name_bytes);
    buf[payload_start + name_bytes.len()] = 0; // NUL
    offset += nla_align(ifname_nla_len);

    // IFLA_LINKINFO (nested)
    let nla = unsafe { &mut *(buf.as_mut_ptr().add(offset) as *mut libc::nlattr) };
    nla.nla_len = linkinfo_nla_len as u16;
    nla.nla_type = libc::IFLA_LINKINFO as u16 | libc::NLA_F_NESTED as u16;
    let nested_offset = offset + std::mem::size_of::<libc::nlattr>();

    // IFLA_INFO_KIND = "dummy"
    let nla = unsafe { &mut *(buf.as_mut_ptr().add(nested_offset) as *mut libc::nlattr) };
    nla.nla_len = info_kind_nla_len as u16;
    nla.nla_type = libc::IFLA_INFO_KIND as u16;
    let payload_start = nested_offset + std::mem::size_of::<libc::nlattr>();
    buf[payload_start..payload_start + info_kind_payload.len()].copy_from_slice(info_kind_payload);

    // Send
    if unsafe { libc::send(nl_fd.as_raw_fd(), buf.as_ptr() as *const _, buf.len(), 0) } < 0 {
        return Err(Error::last_os_error());
    }

    // Read ACK
    let mut resp = [0u8; 1024];
    let n = unsafe { libc::recv(nl_fd.as_raw_fd(), resp.as_mut_ptr() as *mut _, resp.len(), 0) };
    if n < 0 {
        return Err(Error::last_os_error());
    }
    if (n as usize) >= std::mem::size_of::<libc::nlmsghdr>() {
        let resp_hdr = unsafe { &*(resp.as_ptr() as *const libc::nlmsghdr) };
        if resp_hdr.nlmsg_type == libc::NLMSG_ERROR as u16 {
            let err_offset = std::mem::size_of::<libc::nlmsghdr>();
            if (n as usize) >= err_offset + 4 {
                let errno = unsafe { *(resp.as_ptr().add(err_offset) as *const i32) };
                if errno != 0 {
                    return Err(Error::from_raw_os_error(-errno));
                }
            }
        }
    }

    Ok(())
}

/// Configure a network interface: set IP address, netmask, and bring it up.
fn setup_net_interface(name: &str, cfg: &NetInterfaceConfig) -> Result<()> {
    let sock: OwnedFd = socket::socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None::<SockProtocol>,
    )?;

    let mut ifr: ifreq = unsafe { std::mem::zeroed() };
    set_ifr_name(&mut ifr, name)?;

    // Set IP address (pre-parsed in config)
    ifr.ifr_ifru.ifru_addr = unsafe { std::mem::transmute(make_sockaddr_in(cfg.ip)) };
    if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFADDR as _, &ifr) } < 0 {
        return Err(Error::last_os_error());
    }

    // Set netmask (pre-parsed in config)
    ifr.ifr_ifru.ifru_netmask = unsafe { std::mem::transmute(make_sockaddr_in(cfg.netmask)) };
    if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFNETMASK as _, &ifr) } < 0 {
        return Err(Error::last_os_error());
    }

    // Set flags: UP + optionally MULTICAST
    let mut flags = libc::IFF_UP as i16;
    if cfg.multicast {
        flags |= libc::IFF_MULTICAST as i16;
    }
    ifr.ifr_ifru.ifru_flags = flags;
    if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFFLAGS as _, &ifr) } < 0 {
        return Err(Error::last_os_error());
    }

    Ok(())
}

/// Create and configure all dummy network interfaces.
fn setup_net_interfaces(interfaces: &HashMap<String, NetInterfaceConfig>) -> Result<()> {
    for (name, cfg) in interfaces {
        trace!("Creating dummy interface {} with addr {}", name, cfg.addr);
        netlink_create_dummy(name)?;
        setup_net_interface(name, cfg)?;
    }
    Ok(())
}

fn child_pid1(child_data: &mut ChildData) -> Result<isize> {
    let pid = Pid::this();
    nix::unistd::setpgid(pid, pid)?;
    reset_signals()?;

    info!("In child, pid = {}, ppid = {}", pid, Pid::parent());

    // Block until the parent has configured our uid_map
    let mut buf = [0; 4];
    let _ = unistd::read(child_data.read_pipe, &mut buf);
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
    setup_net_interfaces(child_data.net_interfaces)?;

    info!("From child!! pid = {} uid = {}", pid, unistd::getuid());

    // Setup child stdio and close everything else
    if let Some(stdout) = child_data.stdout {
        let _ = unistd::dup2_stdout(stdout)?;
    }
    if let Some(stderr) = child_data.stderr {
        let _ = unistd::dup2_stderr(stderr)?;
    }
    close_range_fds((libc::STDERR_FILENO as c_uint) + 1)?;

    // Drop to the configured uid/gid for the build command.
    // pid1 setup (mounts, network, etc.) has already completed as root.
    if let Some(gid) = child_data.run_as_group {
        child_data.cmd.gid(gid);
    }
    if let Some(uid) = child_data.run_as_user {
        child_data.cmd.uid(uid);
    }

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
