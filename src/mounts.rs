use std::ffi::{CStr, CString};
use std::io::Error;
use std::path::Path;
use std::vec::Vec;

use nix::libc::{self, mntent, FILE};
use nix::mount::MsFlags;

pub(crate) struct MntEntOpener(*mut FILE);

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct MntEntWrapper {
    pub mnt_fsname: String, // name of mounted filesystem
    pub mnt_dir: String,    // filesystem path prefix
    pub mnt_type: String,   // mount type (see mntent.h)
    pub mnt_opts: String,   // mount options (see mntent.h)
    pub mnt_freq: i32,      // dump frequency in days
    pub mnt_passno: i32,    // pass number on parallel fsck
    pub mnt_flags: MsFlags, // Mount Flags to pass to mount(2)
}

impl std::convert::From<*mut mntent> for MntEntWrapper {
    fn from(source: *mut mntent) -> Self {
        let mut flags = MsFlags::empty();
        if !unsafe { libc::hasmntopt(source, c"nosuid".as_ptr()).is_null() } {
            flags |= MsFlags::MS_NOSUID;
        }
        if !unsafe { libc::hasmntopt(source, c"nodev".as_ptr()).is_null() } {
            flags |= MsFlags::MS_NODEV;
        }
        if !unsafe { libc::hasmntopt(source, c"noexec".as_ptr()).is_null() } {
            flags |= MsFlags::MS_NOEXEC;
        }
        if !unsafe { libc::hasmntopt(source, c"noatime".as_ptr()).is_null() } {
            flags |= MsFlags::MS_NOATIME;
        }
        if !unsafe { libc::hasmntopt(source, c"nodiratime".as_ptr()).is_null() } {
            flags |= MsFlags::MS_NODIRATIME;
        }
        if !unsafe { libc::hasmntopt(source, c"relatime".as_ptr()).is_null() } {
            flags |= MsFlags::MS_RELATIME;
        }

        Self {
            mnt_fsname: String::from(
                unsafe { CStr::from_ptr((*source).mnt_fsname) }.to_string_lossy(),
            ),
            mnt_dir: String::from(unsafe { CStr::from_ptr((*source).mnt_dir) }.to_string_lossy()),
            mnt_type: String::from(unsafe { CStr::from_ptr((*source).mnt_type) }.to_string_lossy()),
            mnt_opts: String::from(unsafe { CStr::from_ptr((*source).mnt_opts) }.to_string_lossy()),
            mnt_freq: unsafe { (*source).mnt_freq },
            mnt_passno: unsafe { (*source).mnt_passno },
            mnt_flags: flags,
        }
    }
}

impl Drop for MntEntOpener {
    fn drop(&mut self) {
        match unsafe { libc::endmntent(self.0) } {
            1 => (),
            ret @ _ => panic!("endmntent returned {}, expected 1", ret),
        }
    }
}

impl MntEntOpener {
    pub fn new(path: &Path) -> Result<Self, Error> {
        let path_c = CString::new(path.to_str().ok_or(Error::other("unknown path"))?)?;

        let mounts = unsafe { libc::setmntent(path_c.as_ptr(), c"r".as_ptr()) };
        if mounts.is_null() {
            Err(Error::last_os_error())
        } else {
            Ok(Self(mounts))
        }
    }

    pub fn list_all(&self) -> Result<Vec<MntEntWrapper>, Error> {
        let mut entries = Vec::<MntEntWrapper>::new();

        loop {
            let mnt: *mut mntent = unsafe { libc::getmntent(self.0) };
            if mnt.is_null() {
                break;
            }

            entries.push(MntEntWrapper::from(mnt));
        }

        Ok(entries)
    }
}
