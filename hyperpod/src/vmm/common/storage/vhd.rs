use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};

use super::{BlockBackend, SECTOR_SIZE};

const FOOTER_LEN: u64 = 512;
const FOOTER_COOKIE: &[u8; 8] = b"conectix";
const DISK_TYPE_FIXED: u32 = 2;
const DISK_TYPE_DYNAMIC: u32 = 3;
const DISK_TYPE_DIFFERENCING: u32 = 4;

/// Fixed VHD ("conectix") backend: the disk image is `original_size` bytes
/// followed by a 512-byte footer at end-of-file. Reads/writes go straight to
/// the file at the corresponding offset; the footer is untouched.
pub struct VhdFixedBackend {
    file: Mutex<File>,
    sectors: u64,
}

impl VhdFixedBackend {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open VHD {}", path.display()))?;
        let total = file.metadata()?.len();
        if total < FOOTER_LEN {
            bail!("{} is too small to be a VHD", path.display());
        }

        file.seek(SeekFrom::Start(total - FOOTER_LEN))?;
        let mut footer = [0u8; FOOTER_LEN as usize];
        file.read_exact(&mut footer).context("read VHD footer")?;

        if &footer[..8] != FOOTER_COOKIE {
            bail!("{} is missing the VHD footer cookie", path.display());
        }
        let disk_type = u32::from_be_bytes(footer[60..64].try_into().unwrap());
        match disk_type {
            DISK_TYPE_FIXED => {}
            DISK_TYPE_DYNAMIC => bail!(
                "{} is a dynamic VHD; only fixed VHDs are supported by this backend",
                path.display()
            ),
            DISK_TYPE_DIFFERENCING => bail!(
                "{} is a differencing VHD; only fixed VHDs are supported by this backend",
                path.display()
            ),
            other => bail!("{} has unsupported VHD disk_type {other}", path.display()),
        }

        let original_size = u64::from_be_bytes(footer[40..48].try_into().unwrap());
        if original_size == 0 {
            bail!("{} reports zero original_size in VHD footer", path.display());
        }
        if original_size + FOOTER_LEN > total {
            bail!(
                "{} footer original_size={original_size} but file length is {total}",
                path.display()
            );
        }
        if original_size % SECTOR_SIZE != 0 {
            bail!(
                "{} original_size {original_size} not a multiple of {SECTOR_SIZE}",
                path.display()
            );
        }

        Ok(Self {
            file: Mutex::new(file),
            sectors: original_size / SECTOR_SIZE,
        })
    }
}

impl BlockBackend for VhdFixedBackend {
    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn read_sector(&self, sector: u64, buf: &mut [u8]) -> Result<()> {
        if sector >= self.sectors {
            bail!("read sector {sector} out of range ({} total)", self.sectors);
        }
        if buf.len() as u64 != SECTOR_SIZE {
            bail!("read buffer must be {SECTOR_SIZE} bytes");
        }
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(sector * SECTOR_SIZE))?;
        f.read_exact(buf).context("vhd read_sector")
    }

    fn write_sector(&self, sector: u64, buf: &[u8]) -> Result<()> {
        if sector >= self.sectors {
            bail!(
                "write sector {sector} out of range ({} total)",
                self.sectors
            );
        }
        if buf.len() as u64 != SECTOR_SIZE {
            bail!("write buffer must be {SECTOR_SIZE} bytes");
        }
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(sector * SECTOR_SIZE))?;
        f.write_all(buf).context("vhd write_sector")
    }

    fn flush(&self) -> Result<()> {
        let mut f = self.file.lock().unwrap();
        f.flush().context("vhd flush")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;
    use tempfile::Builder;

    /// Builds a minimal valid fixed-VHD: payload of `sectors * 512` bytes
    /// followed by a 512-byte footer with the required fields.
    fn make_fixed_vhd(sectors: u64) -> tempfile::NamedTempFile {
        let mut f = Builder::new().suffix(".vhd").tempfile().unwrap();
        let payload = vec![0u8; (sectors * SECTOR_SIZE) as usize];
        IoWrite::write_all(&mut f,&payload).unwrap();
        let mut footer = [0u8; 512];
        footer[..8].copy_from_slice(FOOTER_COOKIE);
        // original_size at offset 40 (big-endian u64).
        footer[40..48].copy_from_slice(&(sectors * SECTOR_SIZE).to_be_bytes());
        // current_size at offset 48.
        footer[48..56].copy_from_slice(&(sectors * SECTOR_SIZE).to_be_bytes());
        // disk_type=2 (fixed) at offset 60.
        footer[60..64].copy_from_slice(&2u32.to_be_bytes());
        IoWrite::write_all(&mut f,&footer).unwrap();
        f
    }

    #[test]
    fn open_and_round_trip_fixed_vhd() {
        let f = make_fixed_vhd(4);
        let be = VhdFixedBackend::open(f.path()).unwrap();
        assert_eq!(be.sector_count(), 4);
        let mut buf = vec![0x5au8; SECTOR_SIZE as usize];
        be.write_sector(1, &buf).unwrap();
        buf.fill(0);
        be.read_sector(1, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x5a));
    }

    #[test]
    fn rejects_dynamic_disk_type() {
        let mut f = Builder::new().suffix(".vhd").tempfile().unwrap();
        IoWrite::write_all(&mut f, &vec![0u8; (SECTOR_SIZE * 4) as usize]).unwrap();
        let mut footer = [0u8; 512];
        footer[..8].copy_from_slice(FOOTER_COOKIE);
        footer[40..48].copy_from_slice(&(SECTOR_SIZE * 4).to_be_bytes());
        footer[60..64].copy_from_slice(&3u32.to_be_bytes()); // dynamic
        IoWrite::write_all(&mut f, &footer).unwrap();
        let err = match VhdFixedBackend::open(f.path()) {
            Ok(_) => panic!("expected dynamic VHD to be rejected"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("dynamic"));
    }
}
