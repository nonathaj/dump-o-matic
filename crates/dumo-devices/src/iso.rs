//! Converting a Games-on-Demand package into a single XDVDFS `.iso`.
//!
//! The package holds the game partition and nothing else, and its sector numbers are
//! relative to an origin before the start of that data (see [`crate::xdvdfs`]). A plain
//! concatenation of the data blocks is therefore *not* a loadable image: the volume
//! descriptor lands at offset 0 where readers expect 0x10000, and every sector number
//! points somewhere that does not exist. Converting properly means two changes:
//!
//! 1. Put 32 sectors (0x10000) in front, so the volume descriptor sits where an XDVDFS
//!    reader looks for it.
//! 2. Subtract a single constant from every sector number — `base * 2 - 32` — so the
//!    addresses point at where the data now is.
//!
//! Nothing else moves: every byte of every file is copied through untouched, and only the
//! directory tables and the volume descriptor are rewritten.
//!
//! This reproduces what the (abandoned) God2Iso tool produced, which was confirmed rather
//! than assumed: against an existing God2Iso conversion of the same title, the payload
//! matched byte-for-byte at points spanning all 6.8 GB, and rewriting the root directory
//! produced a table with an identical SHA-256. The only deliberate difference is in the
//! reserved region, where this tool writes its own name instead of God2Iso's.

use crate::god::{GodImage, BLOCK};
use crate::xcontent::XContent;
use crate::{xdvdfs, DeviceError, Result};
use sha2::{Digest, Sha256};
use std::io::Write;

/// Bytes reserved before the volume descriptor, as XDVDFS requires.
pub const RESERVED: u64 = 0x10000;
/// Sector at which the volume descriptor must land.
const VD_SECTOR: u32 = 32;
/// Offset in the reserved region where the producing tool names itself.
const SIGNATURE_OFFSET: usize = 0x7A69;
/// ISO 9660 primary volume descriptor location, for the compatibility skin.
const PVD_OFFSET: usize = 0x8000;

/// What the conversion will do, worked out before anything is written.
pub struct IsoPlan {
    /// Block base the package's sector numbers are relative to.
    pub base_blocks: u64,
    /// Constant subtracted from every sector number.
    pub shift_sectors: u32,
    /// Size of the image data, excluding the reserved region.
    pub image_bytes: u64,
    /// Directory tables and the volume descriptor, already rewritten, keyed by their
    /// offset within the image data.
    patches: Vec<(u64, Vec<u8>)>,
    /// Number of directory tables rewritten, for reporting.
    pub directories: usize,
    /// Number of sector references rewritten, for reporting.
    pub references: usize,
}

impl IsoPlan {
    /// Total size of the finished `.iso`.
    pub fn total_bytes(&self) -> u64 {
        RESERVED + self.image_bytes
    }
}

/// Work out how to convert this package, without writing anything.
pub fn plan(img: &GodImage<'_>, header: &XContent) -> Result<IsoPlan> {
    let vd = xdvdfs::read_volume_descriptor(img)?;
    let base_blocks = xdvdfs::resolve_base(img, &vd, header.data_block_offset_hint())?;

    // The volume descriptor sits at the very start of the package's data, which is base*2
    // in the package's own sector numbering. Moving it to sector 32 is the whole shift.
    let vd_sector = base_blocks
        .checked_mul(2)
        .and_then(|s| u32::try_from(s).ok())
        .ok_or_else(|| DeviceError::Unsupported {
            item: "image".to_string(),
            detail: format!("block base {base_blocks} is implausibly large"),
        })?;
    let shift_sectors = vd_sector
        .checked_sub(VD_SECTOR)
        .ok_or_else(|| DeviceError::Unsupported {
            item: "image".to_string(),
            detail: format!(
                "the image is addressed from sector {vd_sector}, which leaves no room for \
                 the 32 reserved sectors an XDVDFS image requires"
            ),
        })?;

    let mut patches: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut references = 0usize;

    // The volume descriptor's own pointer to the root directory.
    let mut vd_sector_bytes = vec![0u8; xdvdfs::SECTOR as usize];
    img.read_at(&mut vd_sector_bytes, 0)?;
    let new_root = shifted(vd.root_sector, shift_sectors)?;
    vd_sector_bytes[0x14..0x18].copy_from_slice(&new_root.to_le_bytes());
    patches.push((0, vd_sector_bytes));
    references += 1;

    // Every directory table in the tree.
    let directories = xdvdfs::collect_directories(img, base_blocks, &vd)?;
    for (sector, size) in &directories {
        let offset = xdvdfs::sector_offset(*sector, base_blocks).ok_or_else(|| {
            DeviceError::Corrupt {
                path: "directory".to_string(),
                detail: format!("sector {sector} lies before the image base"),
            }
        })?;
        let mut buf = vec![0u8; *size as usize];
        img.read_at(&mut buf, offset)?;

        let records = xdvdfs::records_with_offsets(&buf).ok_or_else(|| DeviceError::Corrupt {
            path: "directory".to_string(),
            detail: format!("sector {sector} stopped parsing as a directory"),
        })?;
        for (record_offset, record) in records {
            let new = shifted(record.start_sector, shift_sectors)?;
            let field = record_offset + xdvdfs::RECORD_SECTOR_FIELD;
            buf[field..field + 4].copy_from_slice(&new.to_le_bytes());
            references += 1;
        }
        patches.push((offset, buf));
    }

    patches.sort_by_key(|(offset, _)| *offset);

    Ok(IsoPlan {
        base_blocks,
        shift_sectors,
        image_bytes: img.size(),
        patches,
        directories: directories.len(),
        references,
    })
}

/// Shift one sector number, refusing anything that would land in the reserved region.
///
/// A reference below sector 32 after shifting would mean the base is wrong, and would
/// overwrite the volume descriptor's own space. Better to stop than to emit it.
fn shifted(sector: u32, shift: u32) -> Result<u32> {
    let new = sector
        .checked_sub(shift)
        .filter(|s| *s >= VD_SECTOR)
        .ok_or_else(|| DeviceError::Corrupt {
            path: "directory".to_string(),
            detail: format!(
                "sector {sector} shifted by {shift} falls inside the reserved area, so the \
                 image base cannot be right"
            ),
        })?;
    Ok(new)
}

/// What was written, and the evidence it was written faithfully.
pub struct IsoReport {
    pub bytes_written: u64,
    /// SHA-256 of the whole file, which is what the pipeline records.
    pub sha256: String,
    /// SHA-256 of the image data alone, excluding the reserved region.
    ///
    /// Recorded separately so a conversion can be compared against one produced by another
    /// tool: the reserved region carries a producer signature and will differ, while the
    /// image itself should be identical.
    pub image_sha256: String,
    /// Blocks read and checked against the package's hash tree.
    pub blocks_verified: u64,
}

/// Write the `.iso`, verifying every block against the package's hash tree as it goes.
pub fn write(
    img: &GodImage<'_>,
    plan: &IsoPlan,
    out: &mut dyn Write,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<IsoReport> {
    let total = plan.total_bytes();
    let mut whole = Sha256::new();
    let mut image_only = Sha256::new();
    let mut written = 0u64;

    let reserved = reserved_region(plan);
    out.write_all(&reserved).map_err(io("writing iso header"))?;
    whole.update(&reserved);
    written += reserved.len() as u64;
    progress(written, total);

    let mut buf = [0u8; BLOCK as usize];
    let mut patch_index = 0usize;
    let mut active: Vec<&(u64, Vec<u8>)> = Vec::new();

    for block in 0..img.block_count() {
        img.read_block_verified(block, &mut buf)?;

        let start = block * BLOCK;
        let end = start + BLOCK;

        // Bring in patches that begin before this block ends, and drop those already past.
        while patch_index < plan.patches.len() && plan.patches[patch_index].0 < end {
            active.push(&plan.patches[patch_index]);
            patch_index += 1;
        }
        active.retain(|(offset, bytes)| offset + bytes.len() as u64 > start);

        for (offset, bytes) in &active {
            let patch_end = offset + bytes.len() as u64;
            let from = (*offset).max(start);
            let to = patch_end.min(end);
            if from >= to {
                continue;
            }
            let in_block = (from - start) as usize;
            let in_patch = (from - offset) as usize;
            let len = (to - from) as usize;
            buf[in_block..in_block + len].copy_from_slice(&bytes[in_patch..in_patch + len]);
        }

        out.write_all(&buf).map_err(io("writing iso"))?;
        whole.update(buf);
        image_only.update(buf);
        written += BLOCK;
        progress(written, total);
    }

    out.flush().map_err(io("flushing iso"))?;

    Ok(IsoReport {
        bytes_written: written,
        sha256: hex(&whole.finalize()),
        image_sha256: hex(&image_only.finalize()),
        blocks_verified: img.block_count(),
    })
}

/// Build the 32 reserved sectors that precede the volume descriptor.
///
/// XDVDFS ignores this area entirely, so its contents are free. Three things go in, all
/// following the shape God2Iso established so its output and ours are the same kind of
/// file: a small descriptor recording the image length, this tool's name, and an ISO 9660
/// primary volume descriptor so that generic tools recognise the file as an image at all.
fn reserved_region(plan: &IsoPlan) -> Vec<u8> {
    let mut r = vec![0u8; RESERVED as usize];

    r[0..4].copy_from_slice(b"XSF\x1a");
    r[4..8].copy_from_slice(&0x400u32.to_le_bytes());
    r[8..16].copy_from_slice(&plan.image_bytes.to_le_bytes());
    r[16..20].copy_from_slice(&2u32.to_le_bytes());

    let signature = format!("dump-o-matic v{}", env!("CARGO_PKG_VERSION"));
    let bytes = signature.as_bytes();
    r[SIGNATURE_OFFSET..SIGNATURE_OFFSET + bytes.len()].copy_from_slice(bytes);

    write_iso9660_skin(&mut r, plan.total_bytes());
    r
}

/// Write an ISO 9660 primary volume descriptor describing an empty volume.
///
/// Cosmetic by design: it makes the file identify as an image to tools that look for
/// `CD001`, while the real filesystem is the XDVDFS at sector 32. The root directory record
/// is left zeroed, exactly as God2Iso left it — there is no ISO 9660 tree here to point at,
/// and inventing one would be a lie in a different direction.
fn write_iso9660_skin(region: &mut [u8], total_bytes: u64) {
    let pvd = PVD_OFFSET;
    region[pvd] = 1; // primary volume descriptor
    region[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
    region[pvd + 6] = 1; // version

    // Text fields are space-filled, as the format requires rather than NUL-filled.
    for range in [
        8..40,     // system identifier
        40..72,    // volume identifier
        190..318,  // volume set identifier
        318..446,  // publisher
        446..574,  // data preparer
        574..702,  // application
        702..739,  // copyright file
        739..776,  // abstract file
        776..813,  // bibliographic file
    ] {
        for i in range {
            region[pvd + i] = b' ';
        }
    }

    let sectors = (total_bytes / xdvdfs::SECTOR) as u32;
    region[pvd + 80..pvd + 84].copy_from_slice(&sectors.to_le_bytes());
    region[pvd + 84..pvd + 88].copy_from_slice(&sectors.to_be_bytes());
    // Volume set size, sequence number and logical block size, each stored both ways round.
    region[pvd + 120..pvd + 122].copy_from_slice(&1u16.to_le_bytes());
    region[pvd + 122..pvd + 124].copy_from_slice(&1u16.to_be_bytes());
    region[pvd + 124..pvd + 126].copy_from_slice(&1u16.to_le_bytes());
    region[pvd + 126..pvd + 128].copy_from_slice(&1u16.to_be_bytes());
    region[pvd + 128..pvd + 130].copy_from_slice(&(xdvdfs::SECTOR as u16).to_le_bytes());
    region[pvd + 130..pvd + 132].copy_from_slice(&(xdvdfs::SECTOR as u16).to_be_bytes());

    // Dates are all "unspecified", which the format spells as ASCII zeros.
    for start in [813usize, 830, 847, 864] {
        for i in 0..16 {
            region[pvd + start + i] = b'0';
        }
        region[pvd + start + 16] = 0;
    }
    region[pvd + 881] = 1; // file structure version

    // Volume descriptor set terminator, in the next sector.
    let term = pvd + xdvdfs::SECTOR as usize;
    region[term] = 0xff;
    region[term + 1..term + 6].copy_from_slice(b"CD001");
    region[term + 6] = 1;
}

fn io(what: &'static str) -> impl Fn(std::io::Error) -> DeviceError {
    move |source| DeviceError::Io {
        path: what.to_string(),
        source,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The measured relationship, from a real package and a real God2Iso conversion of it:
    /// Call of Duty World at War, base 25,612 blocks, shift 51,192 sectors, and a root
    /// directory that moved from sector 1,783,934 to 1,732,742.
    #[test]
    fn shift_matches_the_reference_conversion() {
        let base_blocks: u64 = 25_612;
        let shift = (base_blocks * 2 - 32) as u32;
        assert_eq!(shift, 51_192);
        assert_eq!(shifted(1_783_934, shift).unwrap(), 1_732_742);
        assert_eq!(shifted(1_848_488, shift).unwrap(), 1_797_296);
        assert_eq!(shifted(3_368_795, shift).unwrap(), 3_317_603);
    }

    /// The reference file is exactly the image plus the reserved region.
    #[test]
    fn total_size_matches_the_reference_conversion() {
        let plan = IsoPlan {
            base_blocks: 25_612,
            shift_sectors: 51_192,
            image_bytes: 1_660_559 * BLOCK,
            patches: Vec::new(),
            directories: 0,
            references: 0,
        };
        assert_eq!(plan.total_bytes(), 6_801_715_200);
    }

    /// A sector that would land in the reserved area means the base is wrong, and must stop
    /// the conversion rather than produce a subtly broken image.
    #[test]
    fn a_reference_falling_into_the_reserved_area_is_refused() {
        assert!(shifted(40, 32).is_err());
        assert!(shifted(10, 100).is_err());
        assert_eq!(shifted(64, 32).unwrap(), 32);
    }

    #[test]
    fn reserved_region_records_the_image_length_and_our_name() {
        let plan = IsoPlan {
            base_blocks: 1,
            shift_sectors: 0,
            image_bytes: 6_801_649_664,
            patches: Vec::new(),
            directories: 0,
            references: 0,
        };
        let r = reserved_region(&plan);
        assert_eq!(r.len(), RESERVED as usize);
        assert_eq!(&r[0..4], b"XSF\x1a");
        assert_eq!(
            u64::from_le_bytes(r[8..16].try_into().unwrap()),
            6_801_649_664
        );
        let sig = String::from_utf8_lossy(&r[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 13]);
        assert!(sig.starts_with("dump-o-matic"), "{sig}");
    }

    /// The skin has to be recognisable as ISO 9660 and describe the right volume size.
    #[test]
    fn iso9660_skin_describes_the_whole_file() {
        let plan = IsoPlan {
            base_blocks: 1,
            shift_sectors: 0,
            image_bytes: 6_801_649_664,
            patches: Vec::new(),
            directories: 0,
            references: 0,
        };
        let r = reserved_region(&plan);
        assert_eq!(r[PVD_OFFSET], 1);
        assert_eq!(&r[PVD_OFFSET + 1..PVD_OFFSET + 6], b"CD001");
        let le = u32::from_le_bytes(r[PVD_OFFSET + 80..PVD_OFFSET + 84].try_into().unwrap());
        let be = u32::from_be_bytes(r[PVD_OFFSET + 84..PVD_OFFSET + 88].try_into().unwrap());
        assert_eq!(le, be);
        assert_eq!(le, 3_321_150, "sector count of the reference conversion");
        assert_eq!(r[PVD_OFFSET + xdvdfs::SECTOR as usize], 0xff);
    }
}
