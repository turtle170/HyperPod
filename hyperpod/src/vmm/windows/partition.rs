//! Thin RAII wrappers over the Windows Hypervisor Platform (WHPX) C API.
//! Every unsafe call is funnelled through these helpers so the orchestrator in
//! `vm.rs` can stay readable.

use std::ffi::c_void;

use anyhow::{bail, Result};
use windows_sys::Win32::System::Hypervisor::{
    WHvCreatePartition, WHvCreateVirtualProcessor, WHvDeletePartition, WHvMapGpaRange,
    WHvRunVirtualProcessor, WHvSetPartitionProperty, WHvSetVirtualProcessorRegisters,
    WHvSetupPartition, WHvMapGpaRangeFlagExecute, WHvMapGpaRangeFlagRead, WHvMapGpaRangeFlagWrite,
    WHV_PARTITION_HANDLE, WHV_PARTITION_PROPERTY_CODE, WHV_REGISTER_NAME, WHV_REGISTER_VALUE,
    WHV_RUN_VP_EXIT_CONTEXT,
};

fn check(hr: i32, op: &'static str) -> Result<()> {
    if hr == 0 {
        Ok(())
    } else {
        bail!("{op} failed: HRESULT 0x{:08x}", hr as u32);
    }
}

pub struct Partition(WHV_PARTITION_HANDLE);

impl Partition {
    pub fn create() -> Result<Self> {
        let mut h: WHV_PARTITION_HANDLE = 0;
        check(unsafe { WHvCreatePartition(&mut h) }, "WHvCreatePartition")?;
        Ok(Self(h))
    }

    pub fn set_property<T: Copy>(
        &self,
        code: WHV_PARTITION_PROPERTY_CODE,
        value: &T,
    ) -> Result<()> {
        check(
            unsafe {
                WHvSetPartitionProperty(
                    self.0,
                    code,
                    value as *const T as *const c_void,
                    std::mem::size_of::<T>() as u32,
                )
            },
            "WHvSetPartitionProperty",
        )
    }

    pub fn setup(&self) -> Result<()> {
        check(unsafe { WHvSetupPartition(self.0) }, "WHvSetupPartition")
    }

    pub fn map_memory(&self, host_ptr: *mut u8, gpa: u64, size: usize) -> Result<()> {
        let flags = WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute;
        check(
            unsafe {
                WHvMapGpaRange(
                    self.0,
                    host_ptr as *const c_void,
                    gpa,
                    size as u64,
                    flags,
                )
            },
            "WHvMapGpaRange",
        )
    }

    pub fn create_vcpu(&self, index: u32) -> Result<()> {
        check(
            unsafe { WHvCreateVirtualProcessor(self.0, index, 0) },
            "WHvCreateVirtualProcessor",
        )
    }

    pub fn set_registers(
        &self,
        vp: u32,
        names: &[WHV_REGISTER_NAME],
        values: &[WHV_REGISTER_VALUE],
    ) -> Result<()> {
        debug_assert_eq!(names.len(), values.len());
        check(
            unsafe {
                WHvSetVirtualProcessorRegisters(
                    self.0,
                    vp,
                    names.as_ptr(),
                    names.len() as u32,
                    values.as_ptr(),
                )
            },
            "WHvSetVirtualProcessorRegisters",
        )
    }

    pub fn run_vcpu(&self, vp: u32) -> Result<WHV_RUN_VP_EXIT_CONTEXT> {
        let mut ctx: WHV_RUN_VP_EXIT_CONTEXT = unsafe { std::mem::zeroed() };
        check(
            unsafe {
                WHvRunVirtualProcessor(
                    self.0,
                    vp,
                    &mut ctx as *mut _ as *mut c_void,
                    std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
                )
            },
            "WHvRunVirtualProcessor",
        )?;
        Ok(ctx)
    }
}

impl Drop for Partition {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                WHvDeletePartition(self.0);
            }
        }
    }
}
