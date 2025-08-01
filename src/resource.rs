use std::process::ExitStatus;
use std::time::Duration;

use crate::proto::resourceusage::PosixResourceUsage;

/// Resources used by a process
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResourceUsage {
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

/// Resources used by a process and its exit status
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExitResources {
    /// Same as the one returned by [`wait`].
    ///
    /// [`wait`]: std::process::Child::wait
    pub status: ExitStatus,
    /// Resource used by the process and all its children
    pub rusage: ResourceUsage,
}

impl From<ResourceUsage> for PosixResourceUsage {
    fn from(val: ResourceUsage) -> Self {
        let mut pbres = PosixResourceUsage::default();
        if let Ok(n) = prost_types::Duration::try_from(val.utime) {
            pbres.user_time = Some(n);
        }

        if let Ok(n) = prost_types::Duration::try_from(val.stime) {
            pbres.system_time = Some(n);
        }

        if let Ok(n) = i64::try_from(val.maxrss) {
            pbres.maximum_resident_set_size = n;
        }

        pbres
    }
}
