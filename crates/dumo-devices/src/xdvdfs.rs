//! XDVDFS: the filesystem inside an Xbox 360 game image.
//!
//! A volume descriptor sits at the start of the image and names the root directory's
//! sector. Directories are tables of packed records, each holding a 2048-byte sector
//! number and a length. Entries are never allowed to straddle a sector boundary, so the
//! tail of a sector is padded with `0xFF` and the next record begins at the next boundary —
//! a parser that stops at the first padding marker silently loses the rest of a large
//! directory, so this one skips to the boundary and keeps going.
//!
//! The subtlety that shapes this module is **addressing**. A Games-on-Demand image holds
//! only the game partition, but its sector numbers are not relative to the start of that
//! image: they are relative to an origin some way before it, different for every title
//! (measured: 96 MB to 813 MB across eight packages). The header declares that origin, and
//! [`resolve_base`] treats the declaration as a hint and then *proves* it by parsing the
//! directory it points at. Getting this wrong does not fail loudly — it produces an image
//! that looks structurally fine and is entirely corrupt — which is why nothing here trusts
//! the field alone.

use crate::god::GodImage;
use crate::{DeviceError, Result};

/// Sector size of the filesystem, as distinct from the 0x1000 block size of the container.
pub const SECTOR: u64 = 2048;
/// Signature at both the start and the end of the volume descriptor sector.
pub const MAGIC: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";
/// Fixed size of a directory record's header, before its name.
const RECORD_HEADER: usize = 14;
/// Attribute bit marking a directory.
pub const ATTR_DIRECTORY: u8 = 0x10;
/// Offset of the start-sector field within a directory record.
pub const RECORD_SECTOR_FIELD: usize = 4;

/// Cap on directory tables walked, so a cyclic or hostile tree cannot spin forever.
const MAX_DIRECTORIES: usize = 100_000;
/// Cap on one directory table's size. Real ones are a few sectors.
const MAX_DIRECTORY_BYTES: u32 = 4 * 1024 * 1024;

/// The image's volume descriptor.
#[derive(Debug, Clone)]
pub struct VolumeDescriptor {
    pub root_sector: u32,
    pub root_size: u32,
    /// Windows FILETIME of image creation, kept as provenance.
    pub filetime: u64,
}

/// One entry in a directory.
#[derive(Debug, Clone)]
pub struct Record {
    pub name: String,
    pub start_sector: u32,
    pub size: u32,
    pub attributes: u8,
}

impl Record {
    pub fn is_dir(&self) -> bool {
        self.attributes & ATTR_DIRECTORY != 0
    }
}

/// Read the volume descriptor from the start of the image.
pub fn read_volume_descriptor(img: &GodImage<'_>) -> Result<VolumeDescriptor> {
    let mut sector = vec![0u8; SECTOR as usize];
    img.read_at(&mut sector, 0)?;
    // The signature appears twice, at both ends of the sector. Checking both is what
    // distinguishes a real descriptor from a block of data that happens to start with the
    // string.
    if &sector[0..20] != MAGIC || &sector[0x7ec..0x800] != MAGIC {
        return Err(DeviceError::Unsupported {
            item: "image".to_string(),
            detail: "no XDVDFS volume descriptor at the start of the image".to_string(),
        });
    }
    Ok(VolumeDescriptor {
        root_sector: u32le(&sector, 0x14),
        root_size: u32le(&sector, 0x18),
        filetime: u64le(&sector, 0x1c),
    })
}

/// Work out which block the filesystem's sector numbers are measured from.
///
/// `hint` is the header's declared offset. Every real package measured needed `hint - 1`,
/// but rather than hardcode that, each candidate is tested by reading the root directory
/// at the address it implies and requiring it to parse — and preferring one that contains
/// the executable every Xbox 360 game has. A candidate that merely parses is accepted only
/// if none contains it, and if nothing parses the extraction is refused.
pub fn resolve_base(img: &GodImage<'_>, vd: &VolumeDescriptor, hint: u64) -> Result<u64> {
    let mut fallback = None;
    for delta in [-1i64, 0, 1, -2, 2] {
        let Some(base) = hint.checked_add_signed(delta) else {
            continue;
        };
        let Some(offset) = sector_offset(vd.root_sector, base) else {
            continue;
        };
        if offset + vd.root_size as u64 > img.size() {
            continue;
        }
        let Ok(records) = read_directory(img, base, vd.root_sector, vd.root_size) else {
            continue;
        };
        // Every entry must address somewhere inside the image, or the base is wrong.
        if records
            .iter()
            .any(|r| sector_offset(r.start_sector, base).map_or(true, |o| o >= img.size()))
        {
            continue;
        }
        if records
            .iter()
            .any(|r| r.name.eq_ignore_ascii_case("default.xex"))
        {
            return Ok(base);
        }
        fallback = fallback.or(Some(base));
    }
    fallback.ok_or_else(|| DeviceError::Unsupported {
        item: "image".to_string(),
        detail: format!(
            "could not locate the root directory: no sector base near the declared {hint} \
             produces a readable directory. Refusing to extract, because a wrong base \
             yields an image that looks valid and is not"
        ),
    })
}

/// Byte offset of a filesystem sector within the image.
pub fn sector_offset(sector: u32, base_blocks: u64) -> Option<u64> {
    (sector as u64 * SECTOR).checked_sub(base_blocks * crate::god::BLOCK)
}

/// Read and parse one directory table.
pub fn read_directory(
    img: &GodImage<'_>,
    base_blocks: u64,
    sector: u32,
    size: u32,
) -> Result<Vec<Record>> {
    if size == 0 || size > MAX_DIRECTORY_BYTES {
        return Err(DeviceError::Corrupt {
            path: "directory".to_string(),
            detail: format!("directory table of {size} bytes"),
        });
    }
    let offset = sector_offset(sector, base_blocks).ok_or_else(|| DeviceError::Corrupt {
        path: "directory".to_string(),
        detail: format!("sector {sector} lies before the image base"),
    })?;
    let mut buf = vec![0u8; size as usize];
    img.read_at(&mut buf, offset)?;
    parse_directory(&buf).ok_or_else(|| DeviceError::Corrupt {
        path: "directory".to_string(),
        detail: format!("sector {sector} does not hold a directory table"),
    })
}

/// Parse a directory table's bytes. `None` if this is not one.
pub fn parse_directory(buf: &[u8]) -> Option<Vec<Record>> {
    Some(
        records_with_offsets(buf)?
            .into_iter()
            .map(|(_, r)| r)
            .collect(),
    )
}

/// Parse a directory table, keeping each record's byte offset within the table.
///
/// The offsets are what lets the ISO writer rewrite sector numbers in place using exactly
/// the same traversal that read them. Two traversals that disagreed — one to read, another
/// to rewrite — would corrupt whichever records they disagreed about.
pub fn records_with_offsets(buf: &[u8]) -> Option<Vec<(usize, Record)>> {
    let mut out = Vec::new();
    let mut offset = 0usize;

    while offset + RECORD_HEADER <= buf.len() {
        let left = u16le(buf, offset);
        let right = u16le(buf, offset + 2);
        let name_len = buf[offset + 13] as usize;

        // 0xFFFF/0xFFFF is padding to the end of the sector, not the end of the table.
        let padding = left == 0xffff && right == 0xffff;
        if padding || name_len == 0 || offset + RECORD_HEADER + name_len > buf.len() {
            let next = next_sector(offset);
            if next <= offset || next >= buf.len() {
                break;
            }
            offset = next;
            continue;
        }

        let name_bytes = &buf[offset + RECORD_HEADER..offset + RECORD_HEADER + name_len];
        // A wrong sector base lands on file data, which will not be printable ASCII. This
        // is the check that makes base resolution self-validating.
        if !name_bytes.iter().all(|b| (0x20..0x7f).contains(b)) {
            return None;
        }

        out.push((
            offset,
            Record {
                name: String::from_utf8_lossy(name_bytes).to_string(),
                start_sector: u32le(buf, offset + 4),
                size: u32le(buf, offset + 8),
                attributes: buf[offset + 12],
            },
        ));

        let advance = (RECORD_HEADER + name_len + 3) & !3;
        offset += advance;

        // No room for another record in this sector: move to the next boundary.
        if SECTOR as usize - (offset % SECTOR as usize) < RECORD_HEADER {
            let next = next_sector(offset);
            if next <= offset {
                break;
            }
            offset = next;
        }
    }

    (!out.is_empty()).then_some(out)
}

fn next_sector(offset: usize) -> usize {
    (offset / SECTOR as usize + 1) * SECTOR as usize
}

/// Every directory table in the image, breadth-first, as (sector, size).
///
/// Collected so the ISO writer can rewrite exactly those regions and nothing else.
pub fn collect_directories(
    img: &GodImage<'_>,
    base_blocks: u64,
    vd: &VolumeDescriptor,
) -> Result<Vec<(u32, u32)>> {
    let mut out = vec![(vd.root_sector, vd.root_size)];
    let mut seen = std::collections::BTreeSet::new();
    seen.insert(vd.root_sector);
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((vd.root_sector, vd.root_size));

    while let Some((sector, size)) = queue.pop_front() {
        let records = read_directory(img, base_blocks, sector, size)?;
        for r in records {
            if !r.is_dir() || r.size == 0 {
                continue;
            }
            // A directory reachable twice would otherwise be rewritten twice, which would
            // subtract the shift from its entries two times over.
            if !seen.insert(r.start_sector) {
                continue;
            }
            if out.len() >= MAX_DIRECTORIES {
                return Err(DeviceError::Corrupt {
                    path: "directory tree".to_string(),
                    detail: format!("more than {MAX_DIRECTORIES} directories"),
                });
            }
            out.push((r.start_sector, r.size));
            queue.push_back((r.start_sector, r.size));
        }
    }
    Ok(out)
}

/// Look up one path, `/`-separated, from the root.
pub fn find(
    img: &GodImage<'_>,
    base_blocks: u64,
    vd: &VolumeDescriptor,
    path: &str,
) -> Result<Option<Record>> {
    let mut current = Record {
        name: String::new(),
        start_sector: vd.root_sector,
        size: vd.root_size,
        attributes: ATTR_DIRECTORY,
    };
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if !current.is_dir() {
            return Ok(None);
        }
        let records = read_directory(img, base_blocks, current.start_sector, current.size)?;
        match records
            .into_iter()
            .find(|r| r.name.eq_ignore_ascii_case(part))
        {
            Some(r) => current = r,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// Read a file's contents, capped at `limit` bytes.
pub fn read_file(
    img: &GodImage<'_>,
    base_blocks: u64,
    record: &Record,
    limit: u64,
) -> Result<Vec<u8>> {
    let want = (record.size as u64).min(limit);
    let offset = sector_offset(record.start_sector, base_blocks).ok_or_else(|| {
        DeviceError::Corrupt {
            path: record.name.clone(),
            detail: "starts before the image base".to_string(),
        }
    })?;
    let mut buf = vec![0u8; want as usize];
    img.read_at(&mut buf, offset)?;
    Ok(buf)
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn u64le(b: &[u8], o: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, sector: u32, size: u32, attrs: u8) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&sector.to_le_bytes());
        v.extend_from_slice(&size.to_le_bytes());
        v.push(attrs);
        v.push(name.len() as u8);
        v.extend_from_slice(name.as_bytes());
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v
    }

    /// The real root directory measured on the drive.
    #[test]
    fn parses_a_measured_root_directory() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&record("data", 1_779_820, 2048, 0x10));
        buf.extend_from_slice(&record("$SystemUpdate", 1_780_044, 2048, 0x10));
        buf.extend_from_slice(&record("default.xex", 1_780_874, 6_270_976, 0x80));
        buf.resize(SECTOR as usize, 0xff);

        let records = parse_directory(&buf).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].name, "data");
        assert!(records[0].is_dir());
        assert_eq!(records[2].name, "default.xex");
        assert_eq!(records[2].size, 6_270_976);
        assert!(!records[2].is_dir());
    }

    /// The case a naive parser gets wrong: padding at a sector boundary is not the end of
    /// the table, and entries after it must still be found.
    #[test]
    fn padding_mid_table_does_not_end_the_directory() {
        let mut buf = vec![0xffu8; 2 * SECTOR as usize];
        let first = record("early.xex", 100, 2048, 0x80);
        buf[..first.len()].copy_from_slice(&first);
        let second = record("late.bin", 200, 4096, 0x80);
        let start = SECTOR as usize;
        buf[start..start + second.len()].copy_from_slice(&second);

        let records = parse_directory(&buf).unwrap();
        assert_eq!(records.len(), 2, "got {records:?}");
        assert_eq!(records[1].name, "late.bin");
    }

    /// File data must not be mistaken for a directory: this is what makes base resolution
    /// able to reject a wrong candidate.
    #[test]
    fn binary_data_is_not_parsed_as_a_directory() {
        let mut buf = vec![0u8; SECTOR as usize];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        // Name length is nonzero and the bytes are not printable.
        buf[13] = 8;
        assert!(parse_directory(&buf).is_none());
    }

    #[test]
    fn an_empty_table_is_not_a_directory() {
        assert!(parse_directory(&vec![0xffu8; SECTOR as usize]).is_none());
    }

    /// Sector addressing is relative to a base before the image, so the arithmetic has to
    /// subtract. These are Prey's measured values.
    #[test]
    fn sector_offsets_are_relative_to_the_base() {
        assert_eq!(sector_offset(398_920, 198_639), Some(3_362_816));
        // A sector before the base cannot be in this image, and must not wrap around.
        assert_eq!(sector_offset(1, 198_639), None);
    }
}
