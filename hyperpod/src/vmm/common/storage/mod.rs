// Backends are constructed up front to validate the rootfs path, but the
// sector-level methods stay quiet until virtio-blk is wired into the run loop.
#![allow(dead_code)]

pub mod raw;
pub mod vhd;

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};

pub const SECTOR_SIZE: u64 = 512;

/// A virtual block device backing store. Implementations cover concrete
/// on-disk formats (raw images, fixed-VHD, ...).
pub trait BlockBackend: Send + Sync {
    fn sector_count(&self) -> u64;
    fn read_sector(&self, sector: u64, buf: &mut [u8]) -> Result<()>;
    fn write_sector(&self, sector: u64, buf: &[u8]) -> Result<()>;
    fn flush(&self) -> Result<()>;
}

/// What format a path resolves to. Useful for `hyperpod status` and tests; the
/// actual `open` dispatcher returns an opened backend ready to serve I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Raw,
    VhdFixed,
    VhdDynamic,
    Vhdx,
}

const VHD_FOOTER_COOKIE: &[u8; 8] = b"conectix";
const VHDX_HEAD_MAGIC: &[u8; 8] = b"vhdxfile";

/// Detect the format from extension first, then content (footer cookie for
/// VHD, head magic for VHDX). Errors only on I/O failure.
pub fn detect_format(path: &Path) -> Result<Format> {
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        match ext.to_ascii_lowercase().as_str() {
            "raw" | "img" | "ext4" => return Ok(Format::Raw),
            "vhd" => return classify_vhd(path),
            "vhdx" => return Ok(Format::Vhdx),
            _ => {}
        }
    }
    let mut head = [0u8; 8];
    let mut f =
        OpenOptions::new().read(true).open(path).with_context(|| format!("open {}", path.display()))?;
    let read = f.read(&mut head).context("read disk image header")?;
    if read >= 8 && &head == VHDX_HEAD_MAGIC {
        return Ok(Format::Vhdx);
    }
    let total = f.metadata()?.len();
    if total >= 512 {
        f.seek(SeekFrom::Start(total - 512))?;
        let mut cookie = [0u8; 8];
        f.read_exact(&mut cookie)?;
        if &cookie == VHD_FOOTER_COOKIE {
            drop(f);
            return classify_vhd(path);
        }
    }
    Ok(Format::Raw)
}

fn classify_vhd(path: &Path) -> Result<Format> {
    let mut f = OpenOptions::new().read(true).open(path)?;
    let total = f.metadata()?.len();
    if total < 512 {
        bail!("{} is too small to be a VHD (no footer)", path.display());
    }
    f.seek(SeekFrom::Start(total - 512))?;
    let mut footer = [0u8; 512];
    f.read_exact(&mut footer).context("read VHD footer")?;
    if &footer[..8] != VHD_FOOTER_COOKIE {
        bail!("{} has no VHD footer cookie", path.display());
    }
    let disk_type = u32::from_be_bytes(footer[60..64].try_into().unwrap());
    match disk_type {
        2 => Ok(Format::VhdFixed),
        3 | 4 => Ok(Format::VhdDynamic),
        other => bail!("VHD disk_type {other} is not supported"),
    }
}

/// Open the backing file at `path` as a block backend, dispatching by format.
pub fn open(path: &Path) -> Result<Box<dyn BlockBackend>> {
    let fmt = detect_format(path)?;
    match fmt {
        Format::Raw => Ok(Box::new(raw::RawBackend::open(path)?)),
        Format::VhdFixed => Ok(Box::new(vhd::VhdFixedBackend::open(path)?)),
        Format::VhdDynamic => bail!(
            "{} is a dynamic VHD; HyperPod 0.1.x only supports fixed VHD. \
             Convert with `qemu-img convert -O vpc -o subformat=fixed`.",
            path.display()
        ),
        Format::Vhdx => bail!(
            "{} is VHDX; HyperPod 0.1.x does not yet support VHDX. \
             Convert with `qemu-img convert -O raw` or `-O vpc -o subformat=fixed`.",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn detects_raw_by_extension() {
        let mut f = tempfile::Builder::new().suffix(".raw").tempfile().unwrap();
        f.write_all(&vec![0u8; SECTOR_SIZE as usize * 4]).unwrap();
        assert_eq!(detect_format(f.path()).unwrap(), Format::Raw);
    }

    #[test]
    fn detects_vhdx_by_magic_without_extension() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"vhdxfile").unwrap();
        f.write_all(&vec![0u8; 4096]).unwrap();
        assert_eq!(detect_format(f.path()).unwrap(), Format::Vhdx);
    }

    #[test]
    fn open_rejects_vhdx_with_clear_message() {
        let mut f = tempfile::Builder::new().suffix(".vhdx").tempfile().unwrap();
        f.write_all(b"vhdxfile").unwrap();
        f.write_all(&vec![0u8; 4096]).unwrap();
        let err = match open(f.path()) {
            Ok(_) => panic!("expected open() to reject VHDX"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("VHDX"), "{err:#}");
    }
}
