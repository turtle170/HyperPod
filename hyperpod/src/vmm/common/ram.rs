use std::convert::TryFrom;

use anyhow::{bail, Context, Result};

/// Anonymous host-allocated buffer mapped 1:1 into guest physical address space
/// starting at GPA 0. The host pointer is what the underlying hypervisor
/// (`WHvMapGpaRange` on Windows, `KVM_SET_USER_MEMORY_REGION` on Linux) is told
/// to back the guest's RAM with.
pub struct GuestRam {
    base: *mut u8,
    size: usize,
}

// The buffer is only mutated through the hypervisor (which serializes vCPU
// access) and through `&self` write helpers used at boot time before the vCPU
// is started. Treat it as Send/Sync for the lifetime of the run.
unsafe impl Send for GuestRam {}
unsafe impl Sync for GuestRam {}

impl GuestRam {
    pub fn new(size_mib: u64) -> Result<Self> {
        let size = usize::try_from(size_mib)
            .ok()
            .and_then(|m| m.checked_mul(1024 * 1024))
            .context("guest memory size overflows usize")?;
        let base = alloc(size)?;
        Ok(Self { base, size })
    }

    pub fn host_ptr(&self) -> *mut u8 {
        self.base
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn write_slice(&self, gpa: u64, data: &[u8]) -> Result<()> {
        let off = usize::try_from(gpa).context("gpa overflow")?;
        let end = off.checked_add(data.len()).context("write overflow")?;
        if end > self.size {
            bail!("write [{off:#x}..{end:#x}) out of bounds (size {:#x})", self.size);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.base.add(off), data.len());
        }
        Ok(())
    }

    pub fn write_obj<T: Copy>(&self, gpa: u64, val: T) -> Result<()> {
        let size = std::mem::size_of::<T>();
        let bytes = unsafe { std::slice::from_raw_parts(&val as *const T as *const u8, size) };
        self.write_slice(gpa, bytes)
    }

    pub fn read_slice(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
        let off = usize::try_from(gpa).context("gpa overflow")?;
        let end = off.checked_add(buf.len()).context("read overflow")?;
        if end > self.size {
            bail!("read [{off:#x}..{end:#x}) out of bounds (size {:#x})", self.size);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(self.base.add(off), buf.as_mut_ptr(), buf.len());
        }
        Ok(())
    }

    pub fn read_obj<T: Copy>(&self, gpa: u64) -> Result<T> {
        let mut buf = vec![0u8; std::mem::size_of::<T>()];
        self.read_slice(gpa, &mut buf)?;
        Ok(unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const T) })
    }
}

impl Drop for GuestRam {
    fn drop(&mut self) {
        if !self.base.is_null() {
            free(self.base, self.size);
        }
    }
}

#[cfg(target_os = "windows")]
fn alloc(size: usize) -> Result<*mut u8> {
    use windows_sys::Win32::System::Memory::{
        VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE,
    };
    let p = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            size,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if p.is_null() {
        bail!("VirtualAlloc({size} bytes) failed");
    }
    Ok(p as *mut u8)
}

#[cfg(target_os = "windows")]
fn free(ptr: *mut u8, _size: usize) {
    use windows_sys::Win32::System::Memory::{VirtualFree, MEM_RELEASE};
    unsafe {
        VirtualFree(ptr as *mut _, 0, MEM_RELEASE);
    }
}

#[cfg(target_os = "linux")]
fn alloc(size: usize) -> Result<*mut u8> {
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        bail!("mmap({size} bytes) failed");
    }
    Ok(p as *mut u8)
}

#[cfg(target_os = "linux")]
fn free(ptr: *mut u8, size: usize) {
    unsafe {
        libc::munmap(ptr as *mut _, size);
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn alloc(_size: usize) -> Result<*mut u8> {
    bail!("guest memory allocation is unsupported on this platform")
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn free(_ptr: *mut u8, _size: usize) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_obj() {
        let ram = GuestRam::new(1).unwrap();
        ram.write_obj::<u64>(0x1000, 0xdead_beef_cafe_babe).unwrap();
        let v: u64 = ram.read_obj(0x1000).unwrap();
        assert_eq!(v, 0xdead_beef_cafe_babe);
    }

    #[test]
    fn out_of_bounds_write_errors() {
        let ram = GuestRam::new(1).unwrap();
        let too_far = (ram.size() as u64) - 4;
        assert!(ram.write_obj::<u64>(too_far, 0).is_err());
    }
}
