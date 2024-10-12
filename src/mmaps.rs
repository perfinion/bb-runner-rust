use std::ffi::c_void;
use std::io::Error;
use std::num::NonZeroUsize;
use std::ptr::NonNull;
use std::slice;

use nix::errno::Errno;
use nix::sys::mman::{self, MapFlags, ProtFlags};

const PAGE_SIZE: usize = 4 * 1024 * 1024;

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
        if stack_size % PAGE_SIZE != 0 {
            return Err(Errno::EINVAL.into());
        }

        // One extra page as the guard page
        let mmap_size = stack_size + PAGE_SIZE;

        let mmap_base = unsafe {
            mman::mmap_anonymous(
                None,
                NonZeroUsize::new(mmap_size).ok_or(Errno::EINVAL)?,
                ProtFlags::PROT_NONE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_STACK,
            )
        }?;

        Ok(Self {
            stack_size: stack_size,
            mmap_size: mmap_size,
            mmap_base: mmap_base,
        })
    }

    pub fn as_slice(&'a self) -> Result<&'a mut [u8], Errno> {
        let rw = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE;

        unsafe {
            let stack_base = self.mmap_base.byte_add(PAGE_SIZE);

            mman::mprotect(stack_base, self.stack_size, rw)?;

            Ok(slice::from_raw_parts_mut(
                stack_base.cast().as_ptr(),
                self.stack_size,
            ))
        }
    }
}
