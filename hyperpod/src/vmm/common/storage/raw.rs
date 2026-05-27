use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};

use super::{BlockBackend, SECTOR_SIZE};

/// Flat-file block backend. Backs `.raw` / `.img` / `.ext4` images and any
/// other format whose payload is the whole file at 512-byte sectors.
pub struct RawBackend {
    file: Mutex<File>,
    sectors: u64,
}

impl RawBackend {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open rootfs {}", path.display()))?;
        let len = file.metadata()?.len();
        if len == 0 {
            bail!("{} is empty", path.display());
        }
        if len % SECTOR_SIZE != 0 {
            bail!(
                "{} length {len} is not a multiple of {SECTOR_SIZE}",
                path.display()
            );
        }
        Ok(Self {
            file: Mutex::new(file),
            sectors: len / SECTOR_SIZE,
        })
    }
}

impl BlockBackend for RawBackend {
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
        f.read_exact(buf).context("read_sector")?;
        Ok(())
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
        f.write_all(buf).context("write_sector")?;
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        let mut f = self.file.lock().unwrap();
        f.flush().context("flush rootfs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;
    use tempfile::Builder;

    fn make_image(sectors: u64) -> tempfile::NamedTempFile {
        let mut f = Builder::new().suffix(".raw").tempfile().unwrap();
        let bytes = vec![0u8; (sectors * SECTOR_SIZE) as usize];
        IoWrite::write_all(&mut f, &bytes).unwrap();
        f
    }

    #[test]
    fn round_trip_sector() {
        let f = make_image(4);
        let be = RawBackend::open(f.path()).unwrap();
        let mut data = vec![0xa5u8; SECTOR_SIZE as usize];
        be.write_sector(2, &data).unwrap();
        data.fill(0);
        be.read_sector(2, &mut data).unwrap();
        assert!(data.iter().all(|&b| b == 0xa5));
    }

    #[test]
    fn rejects_misaligned_image() {
        let mut f = Builder::new().suffix(".raw").tempfile().unwrap();
        IoWrite::write_all(&mut f, &vec![0u8; 1024 + 1]).unwrap();
        assert!(RawBackend::open(f.path()).is_err());
    }

    #[test]
    fn rejects_out_of_range_sector() {
        let f = make_image(4);
        let be = RawBackend::open(f.path()).unwrap();
        let buf = vec![0u8; SECTOR_SIZE as usize];
        assert!(be.write_sector(4, &buf).is_err());
    }
}
