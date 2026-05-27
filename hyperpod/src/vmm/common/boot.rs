use anyhow::Result;

use super::ram::GuestRam;

// Low-memory layout for boot artefacts.
pub const BOOT_GDT_OFFSET: u64 = 0x500;
pub const BOOT_IDT_OFFSET: u64 = 0x520;
pub const ZERO_PAGE_START: u64 = 0x7000;
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;
pub const PML4_START: u64 = 0x9000;
pub const PDPTE_START: u64 = 0xa000;
pub const PDE_START: u64 = 0xb000;
pub const CMDLINE_START: u64 = 0x20000;
pub const HIMEM_START: u64 = 0x100000;

// Linux x86 boot protocol magic and offsets within boot_params.
const KERNEL_HDR_MAGIC: u32 = 0x5372_6448; // "HdrS"
const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
const KERNEL_LOADER_OTHER: u8 = 0xff;
const KERNEL_MIN_ALIGN: u32 = 0x0100_0000;
const BOOT_PROTOCOL_VERSION: u16 = 0x020c;
const E820_RAM: u32 = 1;

const OFFSET_E820_ENTRIES: u64 = 0x1e8;
const OFFSET_HDR_BOOT_FLAG: u64 = 0x1fe;
const OFFSET_HDR_HEADER: u64 = 0x202;
const OFFSET_HDR_VERSION: u64 = 0x206;
const OFFSET_HDR_TYPE_OF_LOADER: u64 = 0x210;
const OFFSET_HDR_CMD_LINE_PTR: u64 = 0x228;
const OFFSET_HDR_KERNEL_ALIGNMENT: u64 = 0x230;
const OFFSET_HDR_CMDLINE_SIZE: u64 = 0x238;
const OFFSET_E820_TABLE: u64 = 0x2d0;

pub fn write_gdt(ram: &GuestRam) -> Result<()> {
    // null / 64-bit kernel code (L=1) / kernel data / TSS.
    let entries: [u64; 4] = [
        0,
        0x00af_9b00_0000_ffff,
        0x00cf_9300_0000_ffff,
        0x0080_8900_0000_0067,
    ];
    for (i, entry) in entries.iter().enumerate() {
        ram.write_obj(BOOT_GDT_OFFSET + (i as u64) * 8, *entry)?;
    }
    ram.write_obj::<u64>(BOOT_IDT_OFFSET, 0)?;
    Ok(())
}

pub fn setup_page_tables(ram: &GuestRam) -> Result<()> {
    // 1 GiB identity map using 2 MiB huge pages.
    ram.write_obj::<u64>(PML4_START, PDPTE_START | 0x3)?;
    ram.write_obj::<u64>(PDPTE_START, PDE_START | 0x3)?;
    for i in 0u64..512 {
        let entry = (i << 21) | 0x83; // present | writable | huge
        ram.write_obj::<u64>(PDE_START + i * 8, entry)?;
    }
    Ok(())
}

pub fn write_cmdline(ram: &GuestRam, cmdline: &str) -> Result<()> {
    let bytes = cmdline.as_bytes();
    ram.write_slice(CMDLINE_START, bytes)?;
    ram.write_obj::<u8>(CMDLINE_START + bytes.len() as u64, 0)?;
    Ok(())
}

pub fn write_zero_page(ram: &GuestRam, cmdline_len: usize, ram_bytes: u64) -> Result<()> {
    let base = ZERO_PAGE_START;
    ram.write_obj::<u16>(base + OFFSET_HDR_BOOT_FLAG, KERNEL_BOOT_FLAG_MAGIC)?;
    ram.write_obj::<u32>(base + OFFSET_HDR_HEADER, KERNEL_HDR_MAGIC)?;
    ram.write_obj::<u16>(base + OFFSET_HDR_VERSION, BOOT_PROTOCOL_VERSION)?;
    ram.write_obj::<u8>(base + OFFSET_HDR_TYPE_OF_LOADER, KERNEL_LOADER_OTHER)?;
    ram.write_obj::<u32>(base + OFFSET_HDR_KERNEL_ALIGNMENT, KERNEL_MIN_ALIGN)?;
    ram.write_obj::<u32>(base + OFFSET_HDR_CMD_LINE_PTR, CMDLINE_START as u32)?;
    ram.write_obj::<u32>(base + OFFSET_HDR_CMDLINE_SIZE, cmdline_len as u32)?;

    let mut count: u8 = 0;
    if ram_bytes > HIMEM_START {
        let entry_off = base + OFFSET_E820_TABLE;
        ram.write_obj::<u64>(entry_off, HIMEM_START)?;
        ram.write_obj::<u64>(entry_off + 8, ram_bytes - HIMEM_START)?;
        ram.write_obj::<u32>(entry_off + 16, E820_RAM)?;
        count = 1;
    }
    ram.write_obj::<u8>(base + OFFSET_E820_ENTRIES, count)?;
    Ok(())
}
