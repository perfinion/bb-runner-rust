use std::ffi::c_void;
use std::io::Error;
use std::num::NonZeroUsize;
use std::ptr::NonNull;
use std::slice;
use std::sync::Once;

use nix::errno::Errno;
use nix::sys::mman::{self, MapFlags, ProtFlags};
use nix::unistd::{self, SysconfVar};

pub fn page_size() -> usize {
    static INIT: Once = Once::new();
    static mut PAGE_SIZE: usize = 0;

    unsafe {
        INIT.call_once(|| {
            PAGE_SIZE = match unistd::sysconf(SysconfVar::PAGE_SIZE) {
                Ok(Some(x)) => x as usize,
                _ => 4 * 1024,
            }
        });
        PAGE_SIZE
    }
}

#[derive(Debug)]
pub(crate) struct StackMap {
    pub stack_size: usize,
    pub mmap_size: usize,
    pub mmap_base: NonNull<c_void>,
}

impl Drop for StackMap {
    fn drop(&mut self) {
        match unsafe { mman::munmap(self.mmap_base, self.mmap_size) } {
            Ok(_) => (),
            Err(e) => panic!("munmap failed: {}", e),
        }
    }
}

impl<'a> StackMap {
    pub fn new(stack_size: usize) -> Result<Self, Error> {
        if stack_size % page_size() != 0 {
            return Err(Errno::EINVAL.into());
        }

        // One extra page as the guard page
        let mmap_size = stack_size + page_size();

        let mmap_base = unsafe {
            mman::mmap_anonymous(
                None,
                NonZeroUsize::new(mmap_size).ok_or(Errno::EINVAL)?,
                ProtFlags::PROT_NONE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_STACK,
            )
        }?;

        Ok(Self {
            stack_size,
            mmap_size,
            mmap_base,
        })
    }

    pub fn as_slice(&'a self) -> Result<&'a mut [u8], Errno> {
        let rw = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE;

        unsafe {
            let stack_base = self.mmap_base.byte_add(page_size());

            mman::mprotect(stack_base, self.stack_size, rw)?;

            Ok(slice::from_raw_parts_mut(
                stack_base.cast().as_ptr(),
                self.stack_size,
            ))
        }
    }
}
