//! Minimal read-only UDF (ECMA-167) reader.
//!
//! Enough to answer one question: what is in the root directory. Blu-ray video discs
//! are UDF without an ISO 9660 bridge (unlike almost all DVDs, which carry both), so
//! `iso9660::read_dir` never sees their `BDMV` directory — this walks the UDF volume
//! and partition descriptors down to the root directory's File Identifier Descriptors
//! instead. Not a general-purpose UDF driver: no write support, no fragmented-file
//! reassembly beyond a directory's own extents, no Unicode beyond UTF-16BE/Latin-1.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

const SECTOR_SIZE: u64 = 2048;
/// Where the Anchor Volume Descriptor Pointer lives on optical media (ECMA-167 2/8.4.2.1).
const ANCHOR_LBA: u64 = 256;

const TAG_PARTITION_DESCRIPTOR: u16 = 5;
const TAG_LOGICAL_VOLUME_DESCRIPTOR: u16 = 6;
const TAG_TERMINATING_DESCRIPTOR: u16 = 8;
const TAG_FILE_SET_DESCRIPTOR: u16 = 256;
const TAG_FILE_IDENTIFIER_DESCRIPTOR: u16 = 257;
const TAG_FILE_ENTRY: u16 = 261;
const TAG_EXTENDED_FILE_ENTRY: u16 = 266;

/// A UDF root directory entry; mirrors `iso9660::DirEntry`'s useful fields.
#[derive(Debug, Clone)]
pub struct UdfEntry {
    pub name: String,
    pub is_dir: bool,
}

fn le_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn read_sectors<R: Read + Seek>(r: &mut R, lba: u64, count: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; (SECTOR_SIZE * count) as usize];
    r.seek(SeekFrom::Start(lba * SECTOR_SIZE))?;
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn tag_id(sector: &[u8]) -> Option<u16> {
    if sector.len() < 16 {
        return None;
    }
    Some(le_u16(sector, 0))
}

/// `long_ad` / `short_ad` resolved to a logical block + length, within a partition
/// *reference* — an index into the Logical Volume Descriptor's partition map table,
/// not the physical `PartitionNumber` from a Partition Descriptor. Most discs have one
/// map that aliases its one partition directly, but UDF 2.01+ discs (which is to say,
/// essentially every pressed BD-ROM) add a second, "Metadata Partition" map: a virtual
/// partition backed by a Metadata File whose own allocation descriptors point at the
/// real blocks. Resolving a reference through that indirection is what `effective_starts`
/// below is for.
struct Extent {
    partition_ref: u16,
    lbn: u32,
    len: u32,
}

/// An ICB's allocation descriptors, or the file's data embedded directly in the ICB.
enum Content {
    Extents(Vec<Extent>),
    Embedded { offset: usize, len: usize },
}

/// Resolve a partition reference to the physical LBA its logical block 0 starts at.
///
/// A Type 1 map aliases a real `PartitionNumber` directly. A Type 2 "Metadata Partition"
/// map names a Metadata File (itself an ICB in a real partition); this disc's whole
/// virtual metadata partition is read from wherever that file's *first* extent points,
/// which is exactly right for the unfragmented case every pressed, read-only disc uses
/// and gives up cleanly (`None`) rather than guess for anything stranger.
fn resolve_partition_ref<R: Read + Seek>(
    r: &mut R,
    partitions: &HashMap<u16, u32>,
    maps: &[PartitionMap],
    partition_ref: u16,
) -> Option<u32> {
    match maps.get(partition_ref as usize)? {
        PartitionMap::Direct(number) => partitions.get(number).copied(),
        PartitionMap::Metadata {
            partition_number,
            metadata_file_lbn,
        } => {
            let base = *partitions.get(partition_number)?;
            let sector = read_sectors(r, u64::from(base) + u64::from(*metadata_file_lbn), 1).ok()?;
            // The Metadata File is reached by direct physical address, not through the
            // map table, so its own short_ad extents are plain offsets into that same
            // physical partition — there is no ambient reference to inherit.
            match file_entry_content(&sector, 0)? {
                Content::Extents(extents) => {
                    let first = extents.first()?;
                    Some(base + first.lbn)
                }
                Content::Embedded { .. } => None,
            }
        }
    }
}

enum PartitionMap {
    Direct(u16),
    Metadata {
        partition_number: u16,
        metadata_file_lbn: u32,
    },
}

/// Parse the Logical Volume Descriptor's partition map table (ECMA-167 3/10.6.14 /
/// UDF 2.2.9): a sequence of variable-length entries, `NumberOfPartitionMaps` of them,
/// each stating its own length so the next one can be found.
fn parse_partition_maps(lvd: &[u8]) -> Vec<PartitionMap> {
    let num_maps = le_u32(lvd, 268) as usize;
    let mut maps = Vec::new();
    let mut off = 440usize;
    for _ in 0..num_maps {
        if off + 2 > lvd.len() {
            break;
        }
        let map_type = lvd[off];
        let map_len = lvd[off + 1] as usize;
        if map_len == 0 || off + map_len > lvd.len() {
            break;
        }
        match map_type {
            1 if map_len >= 6 => maps.push(PartitionMap::Direct(le_u16(lvd, off + 4))),
            2 if map_len >= 44 => maps.push(PartitionMap::Metadata {
                partition_number: le_u16(lvd, off + 38),
                metadata_file_lbn: le_u32(lvd, off + 40),
            }),
            // An unrecognised map type still occupies a reference slot.
            _ => maps.push(PartitionMap::Direct(u16::MAX)),
        }
        off += map_len;
    }
    maps
}

/// Read the root directory's entries, if this looks like a UDF volume we understand.
pub fn read_root_entries<R: Read + Seek>(r: &mut R) -> std::io::Result<Option<Vec<UdfEntry>>> {
    let Ok(anchor) = read_sectors(r, ANCHOR_LBA, 1) else {
        return Ok(None);
    };
    if tag_id(&anchor) != Some(2) {
        return Ok(None);
    }
    let main_vds_len = le_u32(&anchor, 16);
    let main_vds_lba = le_u32(&anchor, 20);
    if main_vds_len == 0 {
        return Ok(None);
    }

    let mut partitions: HashMap<u16, u32> = HashMap::new();
    let mut fsd_extent: Option<Extent> = None;
    let mut maps: Vec<PartitionMap> = Vec::new();

    let vds_sectors = u64::from(main_vds_len.div_ceil(SECTOR_SIZE as u32));
    for s in 0..vds_sectors {
        let Ok(sector) = read_sectors(r, u64::from(main_vds_lba) + s, 1) else {
            break;
        };
        match tag_id(&sector) {
            Some(TAG_TERMINATING_DESCRIPTOR) => break,
            Some(TAG_PARTITION_DESCRIPTOR) => {
                let number = le_u16(&sector, 22);
                let start = le_u32(&sector, 188);
                partitions.insert(number, start);
            }
            Some(TAG_LOGICAL_VOLUME_DESCRIPTOR) => {
                // LogicalVolumeContentsUse (16 bytes, a long_ad) at offset 248 holds the
                // File Set Descriptor's location.
                if sector.len() >= 440 {
                    fsd_extent = Some(Extent {
                        len: le_u32(&sector, 248),
                        lbn: le_u32(&sector, 252),
                        partition_ref: le_u16(&sector, 256),
                    });
                    maps = parse_partition_maps(&sector);
                }
            }
            _ => {}
        }
    }

    let Some(fsd) = fsd_extent else { return Ok(None) };
    let Some(fsd_start) = resolve_partition_ref(r, &partitions, &maps, fsd.partition_ref) else {
        return Ok(None);
    };
    let Ok(fsd_sector) = read_sectors(r, u64::from(fsd_start) + u64::from(fsd.lbn), 1) else {
        return Ok(None);
    };
    if tag_id(&fsd_sector) != Some(TAG_FILE_SET_DESCRIPTOR) || fsd_sector.len() < 416 {
        return Ok(None);
    }
    // RootDirectoryICB (long_ad, 16 bytes) at offset 400.
    let root_icb = Extent {
        len: le_u32(&fsd_sector, 400),
        lbn: le_u32(&fsd_sector, 404),
        partition_ref: le_u16(&fsd_sector, 408),
    };
    let Some(root_part_start) = resolve_partition_ref(r, &partitions, &maps, root_icb.partition_ref)
    else {
        return Ok(None);
    };
    let Ok(fe_sector) = read_sectors(r, u64::from(root_part_start) + u64::from(root_icb.lbn), 1)
    else {
        return Ok(None);
    };

    let Some(content) = file_entry_content(&fe_sector, root_icb.partition_ref) else {
        return Ok(None);
    };

    let data = match content {
        Content::Embedded { offset, len } => {
            if offset + len > fe_sector.len() {
                return Ok(None);
            }
            fe_sector[offset..offset + len].to_vec()
        }
        Content::Extents(extents) => {
            let mut data = Vec::new();
            for e in extents {
                let Some(start) = resolve_partition_ref(r, &partitions, &maps, e.partition_ref)
                else {
                    continue;
                };
                let sectors = u64::from(e.len.div_ceil(SECTOR_SIZE as u32));
                if sectors == 0 {
                    continue;
                }
                let Ok(bytes) = read_sectors(r, u64::from(start) + u64::from(e.lbn), sectors)
                else {
                    continue;
                };
                data.extend_from_slice(&bytes[..(e.len as usize).min(bytes.len())]);
            }
            data
        }
    };

    Ok(Some(parse_file_identifiers(&data)))
}

/// Pull a File (or Extended File) Entry's allocation descriptors / embedded data out,
/// using the ICB Tag's flags to say which it is (ECMA-167 4/14.6, low 3 bits of Flags).
///
/// `ambient_partition_ref` is the partition reference this ICB was itself reached
/// through — a `short_ad` extent carries no partition reference of its own, so by spec
/// it inherits its containing ICB's. Irrelevant when the content turns out to be
/// `long_ad` (self-describing) or embedded (no extents at all).
fn file_entry_content(sector: &[u8], ambient_partition_ref: u16) -> Option<Content> {
    let (lea_off, lad_off, data_off) = match tag_id(sector)? {
        TAG_FILE_ENTRY => (168usize, 172usize, 176usize),
        TAG_EXTENDED_FILE_ENTRY => (208usize, 212usize, 216usize),
        _ => return None,
    };
    if sector.len() < data_off {
        return None;
    }
    let icb_flags = le_u16(sector, 16 + 18);
    let lengh_ea = le_u32(sector, lea_off) as usize;
    let length_ad = le_u32(sector, lad_off) as usize;
    let ad_off = data_off + lengh_ea;
    if ad_off + length_ad > sector.len() {
        return None;
    }
    let ad_bytes = &sector[ad_off..ad_off + length_ad];

    match icb_flags & 0x7 {
        3 => Some(Content::Embedded {
            offset: ad_off,
            len: length_ad,
        }),
        0 => {
            // short_ad: 8 bytes each (ExtentLength u32, ExtentPosition u32).
            let mut out = Vec::new();
            let mut off = 0;
            while off + 8 <= ad_bytes.len() {
                let len = le_u32(ad_bytes, off);
                let pos = le_u32(ad_bytes, off + 4);
                if len == 0 {
                    break;
                }
                out.push(Extent {
                    partition_ref: ambient_partition_ref,
                    lbn: pos,
                    len,
                });
                off += 8;
            }
            Some(Content::Extents(out))
        }
        1 => {
            // long_ad: 16 bytes each.
            let mut out = Vec::new();
            let mut off = 0;
            while off + 16 <= ad_bytes.len() {
                let len = le_u32(ad_bytes, off);
                if len == 0 {
                    break;
                }
                out.push(Extent {
                    len,
                    lbn: le_u32(ad_bytes, off + 4),
                    partition_ref: le_u16(ad_bytes, off + 8),
                });
                off += 16;
            }
            Some(Content::Extents(out))
        }
        _ => None,
    }
}

/// Decode a sequence of File Identifier Descriptors (ECMA-167 4/14.4) into entries.
fn parse_file_identifiers(data: &[u8]) -> Vec<UdfEntry> {
    let mut entries = Vec::new();
    let mut off = 0usize;
    while off + 38 <= data.len() {
        if tag_id(&data[off..]) != Some(TAG_FILE_IDENTIFIER_DESCRIPTOR) {
            break;
        }
        let characteristics = data[off + 18];
        let l_fi = data[off + 19] as usize;
        let l_iu = le_u16(data, off + 36) as usize;
        let name_off = off + 38 + l_iu;
        if name_off + l_fi > data.len() {
            break;
        }
        let is_parent = characteristics & 0x08 != 0;
        if l_fi > 0 && !is_parent {
            let raw = &data[name_off..name_off + l_fi];
            if let Some(name) = decode_dstring(raw) {
                let is_dir = characteristics & 0x02 != 0;
                entries.push(UdfEntry { name, is_dir });
            }
        }
        let record_len = 38 + l_iu + l_fi;
        let padded = record_len.div_ceil(4) * 4;
        if padded == 0 {
            break;
        }
        off += padded;
    }
    entries
}

/// The OSTA CS0 charspec: byte 0 is a compression id, 8 = 8-bit, 16 = 16-bit big-endian.
fn decode_dstring(raw: &[u8]) -> Option<String> {
    let (compression, chars) = raw.split_first()?;
    match compression {
        8 => Some(String::from_utf8_lossy(chars).to_string()),
        16 => {
            let units: Vec<u16> = chars
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn put_tag(buf: &mut [u8], at: usize, id: u16) {
        buf[at..at + 2].copy_from_slice(&id.to_le_bytes());
    }

    fn fid(name: &[u8], is_dir: bool, is_parent: bool) -> Vec<u8> {
        let mut rec = vec![0u8; 38];
        put_tag(&mut rec, 0, TAG_FILE_IDENTIFIER_DESCRIPTOR);
        let mut characteristics = 0u8;
        if is_dir {
            characteristics |= 0x02;
        }
        if is_parent {
            characteristics |= 0x08;
        }
        rec[18] = characteristics;
        if is_parent || name.is_empty() {
            rec[19] = 0;
        } else {
            let mut named = vec![8u8]; // 8-bit compression
            named.extend_from_slice(name);
            rec[19] = named.len() as u8;
            rec.extend_from_slice(&named);
        }
        while rec.len() % 4 != 0 {
            rec.push(0);
        }
        rec
    }

    /// A synthetic disc image with one partition, a File Set Descriptor, a root File
    /// Entry whose directory data is embedded directly in the ICB (flags == 3), and a
    /// BDMV + CERTIFICATE entry — the shape of a real BD-ROM video disc's root.
    fn synthetic_udf_bdmv() -> Vec<u8> {
        const PARTITION_START: u32 = 300;
        let mut img = vec![0u8; (SECTOR_SIZE * 400) as usize];

        // Anchor Volume Descriptor Pointer at LBA 256: Main VDS = 2 sectors at LBA 32.
        let a = (SECTOR_SIZE * ANCHOR_LBA) as usize;
        put_tag(&mut img, a, 2);
        img[a + 16..a + 20].copy_from_slice(&(SECTOR_SIZE as u32 * 2).to_le_bytes());
        img[a + 20..a + 24].copy_from_slice(&32u32.to_le_bytes());

        // Partition Descriptor at LBA 32: partition 0 starts at PARTITION_START.
        let pd = (SECTOR_SIZE * 32) as usize;
        put_tag(&mut img, pd, TAG_PARTITION_DESCRIPTOR);
        img[pd + 22..pd + 24].copy_from_slice(&0u16.to_le_bytes());
        img[pd + 188..pd + 192].copy_from_slice(&PARTITION_START.to_le_bytes());

        // Logical Volume Descriptor at LBA 33: File Set Descriptor at partition-relative
        // LBN 0, partition ref 0 — one Type 1 map aliasing partition 0 directly.
        let lvd = (SECTOR_SIZE * 33) as usize;
        put_tag(&mut img, lvd, TAG_LOGICAL_VOLUME_DESCRIPTOR);
        img[lvd + 248..lvd + 252].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
        img[lvd + 252..lvd + 256].copy_from_slice(&0u32.to_le_bytes());
        img[lvd + 256..lvd + 258].copy_from_slice(&0u16.to_le_bytes());
        img[lvd + 268..lvd + 272].copy_from_slice(&1u32.to_le_bytes()); // NumberOfPartitionMaps
        img[lvd + 440] = 1; // Type 1 map
        img[lvd + 441] = 6; // map length
        img[lvd + 444..lvd + 446].copy_from_slice(&0u16.to_le_bytes()); // -> partition 0

        // File Set Descriptor at partition-relative LBN 0 -> absolute LBA 300. Root
        // Directory ICB at partition-relative LBN 1, partition ref 0.
        let fsd = (SECTOR_SIZE * u64::from(PARTITION_START)) as usize;
        put_tag(&mut img, fsd, TAG_FILE_SET_DESCRIPTOR);
        img[fsd + 400..fsd + 404].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
        img[fsd + 404..fsd + 408].copy_from_slice(&1u32.to_le_bytes());
        img[fsd + 408..fsd + 410].copy_from_slice(&0u16.to_le_bytes());

        // Root File Entry at partition-relative LBN 1 -> absolute LBA 301. ICBTag.Flags
        // (offset 16+18=34) = 3 (embedded). Directory data embedded starting at 176.
        let fe = (SECTOR_SIZE * u64::from(PARTITION_START + 1)) as usize;
        put_tag(&mut img, fe, TAG_FILE_ENTRY);
        img[fe + 34..fe + 36].copy_from_slice(&3u16.to_le_bytes());
        let mut dir_data = fid(&[], true, true); // parent/self entry, no name
        dir_data.extend(fid(b"BDMV", true, false));
        dir_data.extend(fid(b"CERTIFICATE", true, false));
        img[fe + 168..fe + 172].copy_from_slice(&0u32.to_le_bytes()); // no extended attrs
        img[fe + 172..fe + 176].copy_from_slice(&(dir_data.len() as u32).to_le_bytes());
        img[fe + 176..fe + 176 + dir_data.len()].copy_from_slice(&dir_data);

        img
    }

    #[test]
    fn reads_bdmv_from_an_embedded_root_directory() {
        let img = synthetic_udf_bdmv();
        let entries = read_root_entries(&mut Cursor::new(&img)).unwrap().unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["BDMV", "CERTIFICATE"]);
        assert!(entries.iter().all(|e| e.is_dir));
    }

    /// A synthetic disc shaped like a real pressed BD-ROM: everything (File Set
    /// Descriptor, root File Entry, root directory extent) sits behind a UDF 2.01+
    /// "Metadata Partition" (Type 2 map), reached only by first resolving a Metadata
    /// File's own short_ad extent in the real, physical partition. This is a regression
    /// test for exactly the gap a real Blu-ray video disc exposed: without Metadata
    /// Partition support, `BDMV` is unreachable and every BD-Video disc reads as "Data".
    fn synthetic_udf_bdmv_with_metadata_partition() -> Vec<u8> {
        const PARTITION_START: u32 = 300;
        const METADATA_BASE: u32 = 320; // where the metadata file's extent points
        let mut img = vec![0u8; (SECTOR_SIZE * 400) as usize];

        let a = (SECTOR_SIZE * ANCHOR_LBA) as usize;
        put_tag(&mut img, a, 2);
        img[a + 16..a + 20].copy_from_slice(&(SECTOR_SIZE as u32 * 2).to_le_bytes());
        img[a + 20..a + 24].copy_from_slice(&32u32.to_le_bytes());

        // Partition Descriptor: physical partition 0 starts at PARTITION_START.
        let pd = (SECTOR_SIZE * 32) as usize;
        put_tag(&mut img, pd, TAG_PARTITION_DESCRIPTOR);
        img[pd + 22..pd + 24].copy_from_slice(&0u16.to_le_bytes());
        img[pd + 188..pd + 192].copy_from_slice(&PARTITION_START.to_le_bytes());

        // Logical Volume Descriptor: FSD lives at virtual metadata-partition LBN 0
        // (partition ref 1 = the Type 2 map below). Map 0 is the Type 1 direct alias
        // (unused here but present, as on a real disc); map 1 is the metadata partition,
        // whose Metadata File sits at physical partition 0, LBN 0.
        let lvd = (SECTOR_SIZE * 33) as usize;
        put_tag(&mut img, lvd, TAG_LOGICAL_VOLUME_DESCRIPTOR);
        img[lvd + 248..lvd + 252].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
        img[lvd + 252..lvd + 256].copy_from_slice(&0u32.to_le_bytes()); // FSD at virtual LBN 0
        img[lvd + 256..lvd + 258].copy_from_slice(&1u16.to_le_bytes()); // partition ref 1
        img[lvd + 268..lvd + 272].copy_from_slice(&2u32.to_le_bytes()); // NumberOfPartitionMaps
        img[lvd + 440] = 1; // map 0: Type 1
        img[lvd + 441] = 6;
        img[lvd + 444..lvd + 446].copy_from_slice(&0u16.to_le_bytes()); // -> partition 0
        img[lvd + 446] = 2; // map 1: Type 2 (Metadata Partition)
        img[lvd + 447] = 64;
        img[lvd + 446 + 38..lvd + 446 + 40].copy_from_slice(&0u16.to_le_bytes()); // underlying partition 0
        img[lvd + 446 + 40..lvd + 446 + 44].copy_from_slice(&0u32.to_le_bytes()); // metadata file at LBN 0

        // Metadata File entry at physical LBA PARTITION_START + 0: a short_ad extent
        // pointing at METADATA_BASE's partition-relative LBN (relative to partition 0).
        let mf = (SECTOR_SIZE * u64::from(PARTITION_START)) as usize;
        put_tag(&mut img, mf, TAG_EXTENDED_FILE_ENTRY);
        img[mf + 34..mf + 36].copy_from_slice(&0u16.to_le_bytes()); // icb flags: short_ad
        img[mf + 208..mf + 212].copy_from_slice(&0u32.to_le_bytes()); // no extended attrs
        img[mf + 212..mf + 216].copy_from_slice(&8u32.to_le_bytes()); // one short_ad
        img[mf + 216..mf + 220].copy_from_slice(&(SECTOR_SIZE as u32 * 4).to_le_bytes());
        img[mf + 220..mf + 224].copy_from_slice(&(METADATA_BASE - PARTITION_START).to_le_bytes());

        // File Set Descriptor at virtual metadata LBN 0 -> physical METADATA_BASE + 0.
        // Root Directory ICB at virtual metadata LBN 1, partition ref 1.
        let fsd = (SECTOR_SIZE * u64::from(METADATA_BASE)) as usize;
        put_tag(&mut img, fsd, TAG_FILE_SET_DESCRIPTOR);
        img[fsd + 400..fsd + 404].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
        img[fsd + 404..fsd + 408].copy_from_slice(&1u32.to_le_bytes());
        img[fsd + 408..fsd + 410].copy_from_slice(&1u16.to_le_bytes());

        // Root File Entry at virtual metadata LBN 1 -> physical METADATA_BASE + 1. Its
        // own directory data is a short_ad extent (inheriting partition ref 1) at
        // virtual metadata LBN 2 -> physical METADATA_BASE + 2.
        let fe = (SECTOR_SIZE * u64::from(METADATA_BASE + 1)) as usize;
        put_tag(&mut img, fe, TAG_EXTENDED_FILE_ENTRY);
        img[fe + 34..fe + 36].copy_from_slice(&0u16.to_le_bytes()); // icb flags: short_ad
        let mut dir_data = fid(&[], true, true);
        dir_data.extend(fid(b"BDMV", true, false));
        dir_data.extend(fid(b"CERTIFICATE", true, false));
        img[fe + 208..fe + 212].copy_from_slice(&0u32.to_le_bytes());
        img[fe + 212..fe + 216].copy_from_slice(&8u32.to_le_bytes());
        img[fe + 216..fe + 220].copy_from_slice(&(dir_data.len() as u32).to_le_bytes());
        img[fe + 220..fe + 224].copy_from_slice(&2u32.to_le_bytes()); // virtual LBN 2

        let dir = (SECTOR_SIZE * u64::from(METADATA_BASE + 2)) as usize;
        img[dir..dir + dir_data.len()].copy_from_slice(&dir_data);

        img
    }

    #[test]
    fn reads_bdmv_through_a_metadata_partition_indirection() {
        let img = synthetic_udf_bdmv_with_metadata_partition();
        let entries = read_root_entries(&mut Cursor::new(&img)).unwrap().unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["BDMV", "CERTIFICATE"]);
        assert!(entries.iter().all(|e| e.is_dir));
    }

    #[test]
    fn no_anchor_descriptor_yields_no_entries() {
        let img = vec![0u8; (SECTOR_SIZE * 260) as usize];
        assert!(read_root_entries(&mut Cursor::new(&img)).unwrap().is_none());
    }

    #[test]
    fn truncated_image_does_not_panic() {
        let img = vec![0u8; 100];
        assert!(read_root_entries(&mut Cursor::new(&img)).unwrap().is_none());
    }

    #[test]
    fn decodes_16_bit_unicode_names() {
        let mut raw = vec![16u8];
        for c in "A".encode_utf16() {
            raw.extend_from_slice(&c.to_be_bytes());
        }
        assert_eq!(decode_dstring(&raw).as_deref(), Some("A"));
    }
}
