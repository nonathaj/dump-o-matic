//! Minimal read-only ISO 9660 reader.
//!
//! Enough to answer Stage 1 questions cheaply: the volume descriptor fields, the root
//! directory listing, and the contents of small well-known files (e.g. `SYSTEM.CNF` on
//! PlayStation discs). This is intentionally not a general-purpose filesystem driver —
//! it reads a handful of sectors and stops.

use dumo_core::VolumeInfo;
use std::io::{Read, Seek, SeekFrom};

pub const SECTOR_SIZE: u64 = 2048;
/// The volume descriptor set always begins at sector 16.
const FIRST_DESCRIPTOR_SECTOR: u64 = 16;
/// Give up after this many descriptors; real discs use a handful.
const MAX_DESCRIPTORS: u64 = 16;

const VD_PRIMARY: u8 = 1;
const VD_TERMINATOR: u8 = 255;

/// A single entry in a directory.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub extent_lba: u32,
    pub size: u32,
}

/// Result of reading the volume descriptors.
#[derive(Debug, Clone)]
pub struct Volume {
    pub info: VolumeInfo,
    pub root_extent: u32,
    pub root_size: u32,
}

fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

/// ISO 9660 pads text fields with spaces; empty means "not set".
fn text(b: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(b).trim().trim_end_matches('\0').to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Parse a 17-byte ISO 9660 date/time field into a readable timestamp.
///
/// The layout is 16 ASCII digits (`YYYYMMDDHHMMSSss`, the last pair being hundredths of
/// a second) followed by a **binary** byte giving the offset from GMT in 15-minute
/// steps, as a signed value. Rendering that final byte as text is wrong — it produces
/// stray characters like a trailing `$` for UTC+9 — so it is decoded, not printed.
fn datetime(b: &[u8]) -> Option<String> {
    if b.len() < 17 {
        return None;
    }
    let digits = &b[..16];
    if !digits.iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let f = |r: std::ops::Range<usize>| String::from_utf8_lossy(&digits[r]).to_string();
    let (y, mo, d) = (f(0..4), f(4..6), f(6..8));
    let (h, mi, s) = (f(8..10), f(10..12), f(12..14));

    // An all-zero field means "not recorded".
    if y == "0000" {
        return None;
    }

    let offset_quarters = b[16] as i8;
    let tz = if offset_quarters == 0 {
        "Z".to_string()
    } else {
        let total_min = i32::from(offset_quarters) * 15;
        format!(
            "{}{:02}:{:02}",
            if total_min < 0 { '-' } else { '+' },
            total_min.abs() / 60,
            total_min.abs() % 60
        )
    };

    Some(format!("{y}-{mo}-{d} {h}:{mi}:{s} {tz}"))
}

fn read_sector<R: Read + Seek>(r: &mut R, lba: u64) -> std::io::Result<[u8; SECTOR_SIZE as usize]> {
    let mut buf = [0u8; SECTOR_SIZE as usize];
    r.seek(SeekFrom::Start(lba * SECTOR_SIZE))?;
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Detect a UDF volume by its recognition sequence.
///
/// DVDs and Blu-rays are UDF; most also carry an ISO 9660 bridge structure, which is what
/// we actually parse. Knowing UDF is present is still useful evidence.
pub fn detect_udf<R: Read + Seek>(r: &mut R) -> bool {
    for lba in FIRST_DESCRIPTOR_SECTOR..FIRST_DESCRIPTOR_SECTOR + 4 {
        if let Ok(buf) = read_sector(r, lba) {
            let id = &buf[1..6];
            if id == b"NSR02" || id == b"NSR03" || id == b"BEA01" {
                return true;
            }
        }
    }
    false
}

/// Read the primary volume descriptor.
pub fn read_volume<R: Read + Seek>(r: &mut R) -> std::io::Result<Option<Volume>> {
    for i in 0..MAX_DESCRIPTORS {
        let buf = match read_sector(r, FIRST_DESCRIPTOR_SECTOR + i) {
            Ok(b) => b,
            // A short/unreadable descriptor area just means "no ISO 9660 here".
            Err(_) => return Ok(None),
        };

        if &buf[1..6] != b"CD001" {
            // Not an ISO 9660 descriptor; could be the UDF recognition sequence.
            continue;
        }
        match buf[0] {
            VD_TERMINATOR => break,
            VD_PRIMARY => {
                let root = &buf[156..190];
                let info = VolumeInfo {
                    volume_id: text(&buf[40..72]),
                    volume_set_id: text(&buf[190..318]),
                    publisher_id: text(&buf[318..446]),
                    application_id: text(&buf[574..702]),
                    created: datetime(&buf[813..830]),
                    volume_space_size: Some(le_u32(&buf, 80)),
                    logical_block_size: Some(le_u16(&buf, 128)),
                };
                return Ok(Some(Volume {
                    info,
                    root_extent: le_u32(root, 2),
                    root_size: le_u32(root, 10),
                }));
            }
            _ => continue,
        }
    }
    Ok(None)
}

/// List a directory given its extent and size.
pub fn read_dir<R: Read + Seek>(
    r: &mut R,
    extent_lba: u32,
    size: u32,
) -> std::io::Result<Vec<DirEntry>> {
    let mut entries = Vec::new();
    let sectors = size.div_ceil(SECTOR_SIZE as u32);

    for s in 0..u64::from(sectors) {
        let buf = match read_sector(r, u64::from(extent_lba) + s) {
            Ok(b) => b,
            Err(_) => break,
        };
        let mut off = 0usize;
        while off < buf.len() {
            let len = buf[off] as usize;
            // A zero length means the rest of this sector is padding.
            if len == 0 {
                break;
            }
            if off + len > buf.len() || len < 33 {
                break;
            }
            let rec = &buf[off..off + len];
            let name_len = rec[32] as usize;
            if 33 + name_len <= rec.len() {
                let raw = &rec[33..33 + name_len];
                // Names 0x00 and 0x01 are the "." and ".." self/parent entries.
                let is_special = name_len == 1 && (raw[0] == 0 || raw[0] == 1);
                if !is_special {
                    let is_dir = rec[25] & 0x02 != 0;
                    let name = String::from_utf8_lossy(raw)
                        .trim_end_matches(";1")
                        .trim()
                        .to_string();
                    entries.push(DirEntry {
                        name,
                        is_dir,
                        extent_lba: le_u32(rec, 2),
                        size: le_u32(rec, 10),
                    });
                }
            }
            off += len;
        }
    }
    Ok(entries)
}

/// Read a whole (small) file. Refuses anything large — this is for config files like
/// `SYSTEM.CNF`, not for content.
pub fn read_small_file<R: Read + Seek>(
    r: &mut R,
    entry: &DirEntry,
    max_bytes: u32,
) -> std::io::Result<Vec<u8>> {
    if entry.size > max_bytes {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(entry.size as usize);
    let sectors = entry.size.div_ceil(SECTOR_SIZE as u32);
    for s in 0..u64::from(sectors) {
        let buf = read_sector(r, u64::from(entry.extent_lba) + s)?;
        out.extend_from_slice(&buf);
    }
    out.truncate(entry.size as usize);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a synthetic ISO 9660 image with one PVD and a root directory.
    fn synthetic_iso() -> Vec<u8> {
        let root_lba: u32 = 20;
        let mut img = vec![0u8; (SECTOR_SIZE * 24) as usize];

        // --- Primary volume descriptor at sector 16 ---
        let pvd = (SECTOR_SIZE * 16) as usize;
        img[pvd] = VD_PRIMARY;
        img[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        img[pvd + 6] = 1;
        // Volume identifier, space padded to 32 bytes.
        let vol_id = b"TEST_DISC                       ";
        img[pvd + 40..pvd + 72].copy_from_slice(vol_id);
        // Volume space size (LE half of the both-endian field).
        img[pvd + 80..pvd + 84].copy_from_slice(&1234u32.to_le_bytes());
        img[pvd + 128..pvd + 130].copy_from_slice(&2048u16.to_le_bytes());
        let pub_id = b"A PUBLISHER";
        img[pvd + 318..pvd + 318 + pub_id.len()].copy_from_slice(pub_id);
        img[pvd + 318 + pub_id.len()..pvd + 446].fill(b' ');
        // Root directory record lives inside the PVD at offset 156.
        img[pvd + 156 + 2..pvd + 156 + 6].copy_from_slice(&root_lba.to_le_bytes());
        img[pvd + 156 + 10..pvd + 156 + 14].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());

        // --- Terminator at sector 17 ---
        let term = (SECTOR_SIZE * 17) as usize;
        img[term] = VD_TERMINATOR;
        img[term + 1..term + 6].copy_from_slice(b"CD001");

        // --- Root directory at sector 20 ---
        let mut off = (SECTOR_SIZE * u64::from(root_lba)) as usize;
        let mut push = |img: &mut Vec<u8>, off: &mut usize, name: &[u8], is_dir: bool| {
            let len = 33 + name.len();
            img[*off] = len as u8;
            img[*off + 2..*off + 6].copy_from_slice(&99u32.to_le_bytes());
            img[*off + 10..*off + 14].copy_from_slice(&512u32.to_le_bytes());
            img[*off + 25] = if is_dir { 0x02 } else { 0x00 };
            img[*off + 32] = name.len() as u8;
            img[*off + 33..*off + 33 + name.len()].copy_from_slice(name);
            *off += len;
        };
        // "." and ".." entries, which must be skipped by the parser.
        push(&mut img, &mut off, &[0u8], true);
        push(&mut img, &mut off, &[1u8], true);
        push(&mut img, &mut off, b"VIDEO_TS", true);
        push(&mut img, &mut off, b"SYSTEM.CNF;1", false);

        img
    }

    #[test]
    fn parses_primary_volume_descriptor() {
        let img = synthetic_iso();
        let vol = read_volume(&mut Cursor::new(&img)).unwrap().expect("a PVD");
        assert_eq!(vol.info.volume_id.as_deref(), Some("TEST_DISC"));
        assert_eq!(vol.info.publisher_id.as_deref(), Some("A PUBLISHER"));
        assert_eq!(vol.info.volume_space_size, Some(1234));
        assert_eq!(vol.info.logical_block_size, Some(2048));
        assert_eq!(vol.root_extent, 20);
    }

    #[test]
    fn lists_root_and_skips_self_and_parent() {
        let img = synthetic_iso();
        let vol = read_volume(&mut Cursor::new(&img)).unwrap().unwrap();
        let entries =
            read_dir(&mut Cursor::new(&img), vol.root_extent, vol.root_size).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["VIDEO_TS", "SYSTEM.CNF"]);
        assert!(entries[0].is_dir);
        assert!(!entries[1].is_dir);
    }

    /// Real field from the PS2 disc SLUS-20578, whose GMT-offset byte is 0x24 (UTC+9).
    /// Printed as text this leaks a trailing '$'.
    #[test]
    fn parses_iso_datetime_with_binary_gmt_offset() {
        let mut field = b"2002091717152800".to_vec();
        field.push(0x24);
        assert_eq!(
            datetime(&field).as_deref(),
            Some("2002-09-17 17:15:28 +09:00")
        );
    }

    #[test]
    fn datetime_handles_utc_and_negative_offsets() {
        let mut utc = b"2010102014565000".to_vec();
        utc.push(0);
        assert_eq!(datetime(&utc).as_deref(), Some("2010-10-20 14:56:50 Z"));

        let mut west = b"2010102014565000".to_vec();
        west.push((-20i8) as u8); // -5 hours
        assert_eq!(datetime(&west).as_deref(), Some("2010-10-20 14:56:50 -05:00"));
    }

    #[test]
    fn datetime_rejects_unset_and_malformed_fields() {
        let mut zeroed = b"0000000000000000".to_vec();
        zeroed.push(0);
        assert_eq!(datetime(&zeroed), None);

        let mut junk = b"not-a-timestamp!".to_vec();
        junk.push(0);
        assert_eq!(datetime(&junk), None);

        assert_eq!(datetime(b"short"), None);
    }

    #[test]
    fn non_iso_image_yields_no_volume() {
        let img = vec![0u8; (SECTOR_SIZE * 24) as usize];
        assert!(read_volume(&mut Cursor::new(&img)).unwrap().is_none());
    }

    #[test]
    fn truncated_image_does_not_panic() {
        let img = vec![0u8; 100];
        assert!(read_volume(&mut Cursor::new(&img)).unwrap().is_none());
    }
}
