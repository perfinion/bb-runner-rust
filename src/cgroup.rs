use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Result, Write};
use std::path::{Path, PathBuf};

use nix::unistd::Pid;
use tracing::{info, warn};

fn write_existing_file<P: AsRef<Path>, S: AsRef<str>>(path: P, contents: S) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .and_then(|mut f| f.write_all(contents.as_ref().as_bytes()))
}

#[derive(Debug, PartialEq)]
pub(crate) enum CgroupVersion {
    V1,
    V2,
}

pub(crate) fn detect_cgroup_version() -> Result<CgroupVersion> {
    // Check if cgroup v2 is mounted at /sys/fs/cgroup
    // cgroup v2 has a unified hierarchy with cgroup.controllers
    if Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        Ok(CgroupVersion::V2)
    } else {
        Ok(CgroupVersion::V1)
    }
}

/// Read the current process's cgroup path from /proc/self/cgroup.
/// For cgroup v2, the line is "0::/path".
fn current_cgroup_v2() -> Result<PathBuf> {
    let file = fs::File::open("/proc/self/cgroup")?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        // cgroup v2 unified hierarchy line starts with "0::"
        if let Some(path) = line.strip_prefix("0::") {
            return Ok(PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')));
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "Could not determine current cgroup v2 path",
    ))
}

/// Set up cgroup delegation for cgroup v2.
///
/// Moves the runner process into a child cgroup ("runner") so that sibling
/// cgroups can be created for jobs. This is required because cgroup v2's
/// "no internal process" constraint forbids a cgroup from both containing
/// processes and having child cgroups with controllers enabled.
///
/// Returns the delegated cgroup root (the original cgroup) under which
/// job cgroups will be created.
pub(crate) fn setup_delegation() -> Result<PathBuf> {
    let version = detect_cgroup_version()?;

    if version == CgroupVersion::V1 {
        warn!("cgroup delegation is a v2 concept; ignoring on v1");
        return Ok(PathBuf::from("/sys/fs/cgroup"));
    }

    let delegated_root = current_cgroup_v2()?;
    info!("Delegated cgroup root: {:?}", delegated_root);

    // Create a child cgroup for the runner process itself
    let runner_cgroup = delegated_root.join("runner");
    if !runner_cgroup.exists() {
        fs::create_dir(&runner_cgroup)?;
    }

    // Move all processes from the delegated root into the runner child cgroup.
    // This is necessary when running as pid 1 inside a container: the pid1
    // reaper parent is also in this cgroup and must be moved out before
    // subtree controllers can be enabled (cgroup v2 "no internal processes"
    // constraint).
    let procs = fs::read_to_string(delegated_root.join("cgroup.procs"))?;
    let runner_procs_path = runner_cgroup.join("cgroup.procs");
    for pid_str in procs.split_whitespace() {
        write_existing_file(&runner_procs_path, pid_str)?;
        info!("Moved pid {} into {:?}", pid_str, runner_cgroup);
    }

    // Enable controllers on the delegated root so job cgroups can use them
    // Read available controllers first
    let controllers_path = delegated_root.join("cgroup.controllers");
    let available = fs::read_to_string(&controllers_path).unwrap_or_default();
    let mut subtree_control = String::new();
    for controller in available.split_whitespace() {
        match controller {
            "cpuset" | "memory" | "cpu" | "io" | "pids" => {
                if !subtree_control.is_empty() {
                    subtree_control.push(' ');
                }
                subtree_control.push('+');
                subtree_control.push_str(controller);
            }
            _ => {}
        }
    }

    if !subtree_control.is_empty() {
        let subtree_control_path = delegated_root.join("cgroup.subtree_control");
        write_existing_file(&subtree_control_path, &subtree_control)?;
        info!(
            "Enabled subtree controllers: {} on {:?}",
            subtree_control, subtree_control_path
        );
    }

    Ok(delegated_root)
}

#[tracing::instrument(ret)]
pub(crate) fn move_child_cgroup(
    pid: Pid,
    jobcpu: &str,
    mem_max: Option<u32>,
    cgroup_root: Option<&Path>,
    cgroup_path: Option<&str>,
) -> Result<()> {
    let version = detect_cgroup_version()?;

    match version {
        CgroupVersion::V2 => move_child_cgroup_v2(pid, jobcpu, mem_max, cgroup_root, cgroup_path),
        CgroupVersion::V1 => move_child_cgroup_v1(pid, jobcpu, mem_max, cgroup_path),
    }
}

fn move_child_cgroup_v2(
    pid: Pid,
    jobcpu: &str,
    mem_max: Option<u32>,
    cgroup_root: Option<&Path>,
    cgroup_path: Option<&str>,
) -> Result<()> {
    let cgroup_name = cgroup_path.unwrap_or("bb_runner");
    let default_root = PathBuf::from("/sys/fs/cgroup").join(cgroup_name);
    let cgroup_root = cgroup_root.unwrap_or(&default_root);
    let cgroup_dir: PathBuf = cgroup_root.join(format!("job{jobcpu}"));
    if !cgroup_dir.exists() {
        fs::create_dir(&cgroup_dir)?;
    }

    write_existing_file(cgroup_dir.join("cgroup.procs"), pid.to_string())?;
    write_existing_file(cgroup_dir.join("cpuset.cpus"), jobcpu)?;
    write_existing_file(cgroup_dir.join("memory.swap.max"), "0")?;
    write_existing_file(cgroup_dir.join("memory.oom.group"), "1")?;
    write_existing_file(cgroup_dir.join("memory.peak"), "0")?;
    if let Some(m) = mem_max {
        write_existing_file(cgroup_dir.join("memory.max"), m.to_string())?;
    }

    Ok(())
}

fn move_child_cgroup_v1(pid: Pid, jobcpu: &str, mem_max: Option<u32>, cgroup_path: Option<&str>) -> Result<()> {
    let cgroup_name = cgroup_path.unwrap_or("bb_runner");
    // Cgroup v1 has many separate hierarchies
    let memory_cgroup_root = Path::new("/sys/fs/cgroup/memory").join(cgroup_name);
    let cpu_cgroup_root = Path::new("/sys/fs/cgroup/cpu,cpuacct").join(cgroup_name);
    let cpuset_cgroup_root = Path::new("/sys/fs/cgroup/cpuset").join(cgroup_name);

    let job_name = format!("job{jobcpu}");

    // Create cgroup directories in each hierarchy
    let memory_cgroup_dir = memory_cgroup_root.join(&job_name);
    let cpu_cgroup_dir = cpu_cgroup_root.join(&job_name);
    let cpuset_cgroup_dir = cpuset_cgroup_root.join(&job_name);

    if !memory_cgroup_dir.exists() {
        fs::create_dir_all(&memory_cgroup_dir)?;
    }
    if !cpu_cgroup_dir.exists() {
        fs::create_dir_all(&cpu_cgroup_dir)?;
    }
    if !cpuset_cgroup_dir.exists() {
        fs::create_dir_all(&cpuset_cgroup_dir)?;
    }

    // Add process to each cgroup
    write_existing_file(memory_cgroup_dir.join("cgroup.procs"), pid.to_string())?;
    write_existing_file(cpu_cgroup_dir.join("cgroup.procs"), pid.to_string())?;
    write_existing_file(cpuset_cgroup_dir.join("cgroup.procs"), pid.to_string())?;

    write_existing_file(cpuset_cgroup_dir.join("cpuset.cpus"), jobcpu)?;
    write_existing_file(memory_cgroup_dir.join("memory.swappiness"), "0")?;
    if let Some(m) = mem_max {
        write_existing_file(
            memory_cgroup_dir.join("memory.limit_in_bytes"),
            m.to_string(),
        )?;
        write_existing_file(
            memory_cgroup_dir.join("memory.memsw.limit_in_bytes"),
            m.to_string(),
        )?;
    }

    Ok(())
}

/// Clean up a job cgroup directory after the child has exited.
pub(crate) fn cleanup_job_cgroup(cgroup_root: &Path, jobcpu: &str, cgroup_path: Option<&str>) {
    let version = detect_cgroup_version().unwrap_or(CgroupVersion::V2);

    match version {
        CgroupVersion::V2 => {
            let cgroup_dir = cgroup_root.join(format!("job{jobcpu}"));
            if cgroup_dir.exists() {
                if let Err(e) = fs::remove_dir(&cgroup_dir) {
                    warn!("Failed to clean up cgroup {:?}: {}", cgroup_dir, e);
                }
            }
        }
        CgroupVersion::V1 => {
            let cgroup_name = cgroup_path.unwrap_or("bb_runner");
            // v1: clean up across all hierarchies
            for controller in &["memory", "cpu,cpuacct", "cpuset"] {
                let dir = Path::new("/sys/fs/cgroup").join(controller).join(cgroup_name).join(format!("job{jobcpu}"));
                if dir.exists() {
                    if let Err(e) = fs::remove_dir(&dir) {
                        warn!("Failed to clean up cgroup {:?}: {}", dir, e);
                    }
                }
            }
        }
    }
}
