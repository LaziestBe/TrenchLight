//! Reading `.mca` region files.
//!
//! A region file is a 8 KiB header followed by 4 KiB sectors:
//!
//! * header 0..4096 — 1024 entries of 4 bytes: 3-byte sector offset (in 4 KiB
//!   units) and 1-byte sector count, indexed `x + z*32` inside the region.
//! * header 4096..8192 — 1024 four-byte timestamps, ignored here.
//! * from 8192 — each chunk is `u32 length` (payload only), `u8 compression`,
//!   then the payload. Compression 1 = gzip, 2 = zlib, 3 = uncompressed.
//!
//! Measured on a real save: 3,818 chunks across 9 regions, all
//! compression 2. gzip and uncompressed are still handled because nothing
//! guarantees that stays true after a different Minecraft version writes here.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const SECTOR: usize = 4096;
const HEADER_SECTORS: usize = 2;

/// Region coordinates and a chunk coordinate inside that region.
///
/// Rust's integer division truncates towards zero, which is wrong for the
/// negative coordinates that half of a world has: chunk `-1` belongs to region
/// `-1`, but `-1 / 32 == 0`. Every conversion here goes through `div_euclid` and
/// `rem_euclid` for that reason.
pub fn region_of(chunk_x: i32, chunk_z: i32) -> (i32, i32) {
    (chunk_x.div_euclid(32), chunk_z.div_euclid(32))
}

pub fn index_in_region(chunk_x: i32, chunk_z: i32) -> usize {
    let x = chunk_x.rem_euclid(32) as usize;
    let z = chunk_z.rem_euclid(32) as usize;
    x + z * 32
}

pub fn region_file(dir: &Path, region_x: i32, region_z: i32) -> PathBuf {
    dir.join(format!("r.{region_x}.{region_z}.mca"))
}

pub struct Region {
    bytes: Vec<u8>,
    path: PathBuf,
}

impl Region {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file =
            File::open(&path).with_context(|| format!("opening region {}", path.display()))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .with_context(|| format!("reading region {}", path.display()))?;
        if bytes.len() < HEADER_SECTORS * SECTOR {
            bail!(
                "{} is {} bytes, smaller than the {}-byte region header",
                path.display(),
                bytes.len(),
                HEADER_SECTORS * SECTOR
            );
        }
        Ok(Region { bytes, path })
    }

    /// `(sector offset, sector count)` for a slot, or `None` when the chunk has
    /// never been written.
    pub fn slot(&self, chunk_x: i32, chunk_z: i32) -> Option<(usize, u8)> {
        let index = index_in_region(chunk_x, chunk_z);
        let base = index * 4;
        let offset = u32::from_be_bytes([0, self.bytes[base], self.bytes[base + 1], self.bytes[base + 2]])
            as usize;
        let count = self.bytes[base + 3];
        if offset == 0 || count == 0 {
            None
        } else {
            Some((offset, count))
        }
    }

    /// Decompressed chunk payload. `Ok(None)` means the slot is empty.
    pub fn chunk_bytes(&self, chunk_x: i32, chunk_z: i32) -> Result<Option<Vec<u8>>> {
        let Some((sector, count)) = self.slot(chunk_x, chunk_z) else {
            return Ok(None);
        };
        let start = sector * SECTOR;
        if start + 5 > self.bytes.len() {
            bail!(
                "{}: chunk ({chunk_x},{chunk_z}) claims sector {sector}, past the end of the file",
                self.path.display()
            );
        }
        let length = u32::from_be_bytes([
            self.bytes[start],
            self.bytes[start + 1],
            self.bytes[start + 2],
            self.bytes[start + 3],
        ]) as usize;
        let compression = self.bytes[start + 4];
        // The declared length is the payload only; the sector count is rounded
        // up to whole sectors, so it is an upper bound rather than an equality.
        if length == 0 || length > count as usize * SECTOR {
            bail!(
                "{}: chunk ({chunk_x},{chunk_z}) declares {length} bytes in {count} sectors",
                self.path.display()
            );
        }
        let payload_start = start + 5;
        let payload_end = payload_start + length;
        if payload_end > self.bytes.len() {
            bail!(
                "{}: chunk ({chunk_x},{chunk_z}) payload runs past the end of the file",
                self.path.display()
            );
        }
        let payload = &self.bytes[payload_start..payload_end];

        let mut out = Vec::with_capacity(length * 4);
        match compression {
            1 => {
                let mut decoder = flate2::read::GzDecoder::new(payload);
                decoder
                    .read_to_end(&mut out)
                    .with_context(|| format!("gunzip chunk ({chunk_x},{chunk_z})"))?;
            }
            2 => {
                let mut decoder = flate2::read::ZlibDecoder::new(payload);
                decoder
                    .read_to_end(&mut out)
                    .with_context(|| format!("inflate chunk ({chunk_x},{chunk_z})"))?;
            }
            3 => out.extend_from_slice(payload),
            other => bail!(
                "{}: chunk ({chunk_x},{chunk_z}) uses unknown compression {other}",
                self.path.display()
            ),
        }
        Ok(Some(out))
    }

    /// Decompressed and parsed chunk, or `None` for an empty slot.
    pub fn chunk(&self, chunk_x: i32, chunk_z: i32) -> Result<Option<crate::nbt::Nbt>> {
        match self.chunk_bytes(chunk_x, chunk_z)? {
            Some(bytes) => Ok(Some(crate::nbt::parse(&bytes).with_context(|| {
                format!("parsing NBT of chunk ({chunk_x},{chunk_z})")
            })?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_chunks_map_to_negative_regions() {
        assert_eq!(region_of(0, 0), (0, 0));
        assert_eq!(region_of(31, 31), (0, 0));
        assert_eq!(region_of(32, 0), (1, 0));
        // The bug this exists to prevent: -1/32 is 0 under truncating division.
        assert_eq!(region_of(-1, -1), (-1, -1));
        assert_eq!(region_of(-32, -33), (-1, -2));
        assert_eq!(region_of(6, -12), (0, -1));
    }

    #[test]
    fn index_matches_the_region_layout() {
        assert_eq!(index_in_region(0, 0), 0);
        assert_eq!(index_in_region(1, 0), 1);
        assert_eq!(index_in_region(0, 1), 32);
        // The spawn chunk of a real save: x%32 + (z%32)*32.
        assert_eq!(index_in_region(6, -12), 6 + 20 * 32);
    }

    #[test]
    fn empty_slot_is_none_not_an_error() {
        let region = Region {
            bytes: vec![0u8; HEADER_SECTORS * SECTOR],
            path: PathBuf::from("synthetic"),
        };
        assert!(region.slot(0, 0).is_none());
        assert!(region.chunk_bytes(0, 0).unwrap().is_none());
    }

    #[test]
    fn truncated_file_is_rejected_at_open() {
        let dir = std::env::temp_dir().join("mcl-world-region-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("small.mca");
        std::fs::write(&path, [0u8; 100]).unwrap();
        assert!(Region::open(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
