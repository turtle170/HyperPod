use std::convert::TryInto;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::ram::GuestRam;

const ELFMAG: &[u8; 4] = b"\x7fELF";
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;

pub struct LoadedKernel {
    pub entry: u64,
}

/// Minimal ELF64 little-endian loader sufficient for an uncompressed vmlinux.
/// Walks PT_LOAD segments and copies each into guest memory at its physical
/// address. Returns the kernel entry point.
pub fn load(path: &Path, ram: &GuestRam) -> Result<LoadedKernel> {
    let bytes = fs::read(path).with_context(|| format!("read kernel {}", path.display()))?;
    if bytes.len() < 64 || &bytes[..4] != ELFMAG {
        bail!("{} is not an ELF file", path.display());
    }
    if bytes[4] != ELFCLASS64 {
        bail!("{} is not ELF64", path.display());
    }
    if bytes[5] != ELFDATA2LSB {
        bail!("{} is not little-endian", path.display());
    }
    let e_machine = u16::from_le_bytes(bytes[18..20].try_into().unwrap());
    if e_machine != EM_X86_64 {
        bail!("{} is not x86_64 (e_machine = {e_machine})", path.display());
    }

    let e_entry = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    let e_phoff = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
    let e_phentsize = u16::from_le_bytes(bytes[54..56].try_into().unwrap()) as usize;
    let e_phnum = u16::from_le_bytes(bytes[56..58].try_into().unwrap()) as usize;

    if e_phentsize < 56 {
        bail!("unexpected program header size {e_phentsize}");
    }

    let mut loaded_any = false;
    for i in 0..e_phnum {
        let off = e_phoff
            .checked_add(i * e_phentsize)
            .context("program header offset overflow")?;
        if off + e_phentsize > bytes.len() {
            bail!("program header {i} runs past end of file");
        }
        let p_type = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        if p_type != PT_LOAD {
            continue;
        }
        let p_offset = u64::from_le_bytes(bytes[off + 8..off + 16].try_into().unwrap()) as usize;
        let p_paddr = u64::from_le_bytes(bytes[off + 24..off + 32].try_into().unwrap());
        let p_filesz = u64::from_le_bytes(bytes[off + 32..off + 40].try_into().unwrap()) as usize;
        let p_memsz = u64::from_le_bytes(bytes[off + 40..off + 48].try_into().unwrap()) as usize;

        if p_offset.checked_add(p_filesz).map_or(true, |e| e > bytes.len()) {
            bail!("PT_LOAD segment {i} file range out of bounds");
        }
        ram.write_slice(p_paddr, &bytes[p_offset..p_offset + p_filesz])?;
        if p_memsz > p_filesz {
            // BSS-style zero fill.
            let zeros = vec![0u8; p_memsz - p_filesz];
            ram.write_slice(p_paddr + p_filesz as u64, &zeros)?;
        }
        loaded_any = true;
    }
    if !loaded_any {
        bail!("no PT_LOAD segments in {}", path.display());
    }
    Ok(LoadedKernel { entry: e_entry })
}
