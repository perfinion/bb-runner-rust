use std::ffi::c_void;
use std::io::Error;
use std::num::{NonZero, NonZeroUsize};
use std::ptr::NonNull;
use std::slice;

use nix::errno::Errno;
use nix::libc;
use nix::sys::mman::{self, MapFlags, ProtFlags};

#[derive(Debug)]
pub(crate) struct StackMap {
    pub stack_size: usize,
    pub mmap_base: NonNull<c_void>,
}

impl Drop for StackMap {
    fn drop(&mut self) {
        match unsafe { mman::munmap(self.mmap_base, self.stack_size as libc::size_t) } {
            Ok(_) => (),
            Err(e) => panic!("munmap failed: {}", e),
        }
    }
}

impl<'a> StackMap {
    pub fn new(stack_size: usize) -> Result<Self, Error> {
        const PAGE_SIZE: usize = 4 * 1024 * 1024;
        if stack_size % PAGE_SIZE != 0 {
            return Err(Errno::EINVAL.into());
        }

        let stack = unsafe {
            mman::mmap_anonymous(
                None,
                NonZeroUsize::new(stack_size).ok_or(Errno::EINVAL)?,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_STACK,
            )
        }?;

        Ok(Self {
            stack_size: stack_size,
            mmap_base: stack,
        })
    }

    pub fn as_slice(&'a self) -> &'a mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.mmap_base.as_ptr() as *mut u8, self.stack_size) }
    }
}
