use std::process::ExitStatus;
use std::time::Duration;

use crate::proto::resourceusage::PosixResourceUsage;

/// Resources used by a process
#[derive(Clone, Copy, Debug)]
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

/// Resources used by a process and its exit status
#[derive(Clone, Copy, Debug)]
pub struct ResUse {
    /// Same as the one returned by [`wait`].
    ///
    /// [`wait`]: std::process::Child::wait
    pub status: ExitStatus,
    /// Resource used by the process and all its children
    pub rusage: ResourceUsage,
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
