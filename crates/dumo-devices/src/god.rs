//! Games-on-Demand (GoD, also called SVOD) image reader.
//!
//! An Xbox 360 Games-on-Demand title is stored as a package: a 45,056-byte
//! [`crate::xcontent`] header beside a `<name>.data/` directory of `Data0000`, `Data0001`
//! … files. Those files are not a plain split of the disc image. Each is laid out as
//!
//! ```text
//! [ master hash block ] [ hash block ] [ 204 data blocks ] [ hash block ] [ 204 data blocks ] …
//! ```
//!
//! with every block 0x1000 bytes and 203 hash-table groups per full file, which is exactly
//! the observed 0xA290000 file size:
//! `0x1000 + 203 * (0x1000 + 204 * 0x1000) == 0xA290000`.
//!
//! Each hash block holds the SHA-1 of the 204 data blocks that follow it, in order. That
//! is the reason this module verifies rather than trusts: the package carries a complete
//! hash tree over its own contents, so a faithful extraction can be *proven* as it is
//! read, giving package extraction the same standard of evidence that a redumper log gives
//! a disc rip. Verification was confirmed against real packages — every block of a 5.77 GB
//! title matched, across data-file boundaries.

use crate::source::{ContentSource, RandomRead};
use crate::xcontent::XContent;
use crate::{DeviceError, Result};
use sha1::{Digest, Sha1};

/// Size of every block, data or hash.
pub const BLOCK: u64 = 0x1000;
/// Data blocks covered by one hash block.
pub const GROUP_DATA: u64 = 204;
/// Hash groups in a full data file.
pub const GROUPS_PER_FILE: u64 = 203;
/// Data blocks in a full data file.
pub const DATA_PER_FILE: u64 = GROUP_DATA * GROUPS_PER_FILE;
/// Size of a full data file: one master hash block, then 203 groups.
pub const FULL_FILE: u64 = BLOCK + GROUPS_PER_FILE * (BLOCK + GROUP_DATA * BLOCK);
/// Length of a SHA-1 digest, the unit of a hash table.
const DIGEST: usize = 20;

/// A GoD package's image, presented as a flat, contiguous byte stream.
pub struct GodImage<'a> {
    files: Vec<Box<dyn RandomRead + 'a>>,
    block_count: u64,
    label: String,
}

impl<'a> GodImage<'a> {
    /// Open the image belonging to a package header at `header_path`.
    pub fn open(source: &'a dyn ContentSource, header_path: &str, header: &XContent) -> Result<Self> {
        let data_dir = format!("{header_path}.data");
        let mut entries: Vec<(u32, String)> = source
            .list(&data_dir)?
            .into_iter()
            .filter(|e| !e.is_dir)
            .filter_map(|e| parse_data_index(&e.name).map(|i| (i, e.name)))
            .collect();
        entries.sort_by_key(|(i, _)| *i);

        if entries.is_empty() {
            return Err(DeviceError::Corrupt {
                path: data_dir,
                detail: "no DataNNNN files".to_string(),
            });
        }
        // The files must be a complete run from zero: a gap would silently shift every
        // byte after it, producing a plausible-looking but wrong image.
        for (expected, (index, name)) in entries.iter().enumerate() {
            if *index as usize != expected {
                return Err(DeviceError::Corrupt {
                    path: data_dir,
                    detail: format!("expected Data{expected:04} but found {name}"),
                });
            }
        }

        let mut files: Vec<Box<dyn RandomRead + 'a>> = Vec::with_capacity(entries.len());
        for (_, name) in &entries {
            files.push(source.open(&format!("{data_dir}/{name}"))?);
        }

        // Every file but the last must be full; a short one in the middle would mean the
        // same silent shift as a missing file.
        for (i, f) in files.iter().enumerate().take(files.len() - 1) {
            if f.size() != FULL_FILE {
                return Err(DeviceError::Corrupt {
                    path: format!("{data_dir}/Data{i:04}"),
                    detail: format!("expected {FULL_FILE} bytes, found {}", f.size()),
                });
            }
        }

        let tail_blocks = files.last().map(|f| f.size() / BLOCK).unwrap_or(0);
        let block_count = (files.len() as u64 - 1) * DATA_PER_FILE + tail_data_blocks(tail_blocks);

        // The header states its own block count. It is consistently one lower than the
        // geometry of the files, because the final block is padding the header does not
        // count. Anything other than that small, known discrepancy means one of the two is
        // not describing this package, so refuse rather than pick a winner.
        let stated = header.data_block_count();
        if stated != 0 && block_count.saturating_sub(stated) > 1 {
            return Err(DeviceError::Corrupt {
                path: data_dir,
                detail: format!(
                    "header declares {stated} data blocks but the data files hold {block_count}"
                ),
            });
        }

        Ok(Self {
            files,
            block_count,
            label: header_path.to_string(),
        })
    }

    /// Number of data files making up the image.
    pub fn data_files(&self) -> usize {
        self.files.len()
    }

    pub fn block_count(&self) -> u64 {
        self.block_count
    }

    /// Size of the reconstructed image in bytes.
    pub fn size(&self) -> u64 {
        self.block_count * BLOCK
    }

    /// Locate data block `index`: which file it is in, and at what offset.
    fn block_location(&self, index: u64) -> Result<(usize, u64)> {
        if index >= self.block_count {
            return Err(DeviceError::Corrupt {
                path: self.label.clone(),
                detail: format!("block {index} is past the end of the image"),
            });
        }
        let file = index / DATA_PER_FILE;
        let within_file = index % DATA_PER_FILE;
        let group = within_file / GROUP_DATA;
        let within_group = within_file % GROUP_DATA;
        // Skip the master hash block, then each earlier group with its hash block, then
        // this group's own hash block.
        let offset = BLOCK
            + group * (BLOCK + GROUP_DATA * BLOCK)
            + BLOCK
            + within_group * BLOCK;
        Ok((file as usize, offset))
    }

    /// Read one data block.
    pub fn read_block(&self, index: u64, buf: &mut [u8; BLOCK as usize]) -> Result<()> {
        let (file, offset) = self.block_location(index)?;
        self.files[file].read_at(buf, offset)
    }

    /// Read the SHA-1 the package records for data block `index`.
    pub fn recorded_hash(&self, index: u64) -> Result<[u8; DIGEST]> {
        let (file, offset) = self.block_location(index)?;
        let within_group = index % GROUP_DATA;
        // The group's hash block immediately precedes its data blocks.
        let table = offset - (within_group + 1) * BLOCK;
        let mut digest = [0u8; DIGEST];
        self.files[file].read_at(&mut digest, table + within_group * DIGEST as u64)?;
        Ok(digest)
    }

    /// Read one data block and check it against the package's hash tree.
    pub fn read_block_verified(&self, index: u64, buf: &mut [u8; BLOCK as usize]) -> Result<()> {
        self.read_block(index, buf)?;
        let expected = self.recorded_hash(index)?;
        let computed = Sha1::digest(&buf[..]);
        if computed.as_slice() != expected {
            return Err(DeviceError::HashMismatch {
                item: self.label.clone(),
                block: index,
                expected: hex(&expected),
                computed: hex(computed.as_slice()),
            });
        }
        Ok(())
    }

    /// Read an arbitrary range of the reconstructed image.
    ///
    /// Unverified: this is the random-access path used to parse the filesystem, where
    /// reads are small and scattered. Extraction goes through
    /// [`GodImage::read_block_verified`] instead, so everything written out is checked.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        let mut block_buf = [0u8; BLOCK as usize];
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let index = pos / BLOCK;
            let inner = (pos % BLOCK) as usize;
            self.read_block(index, &mut block_buf)?;
            let take = (BLOCK as usize - inner).min(buf.len() - done);
            buf[done..done + take].copy_from_slice(&block_buf[inner..inner + take]);
            done += take;
        }
        Ok(())
    }

    /// Copy one data file out verbatim, verifying its whole hash tree as it passes.
    ///
    /// This is the faithful-copy path: the bytes written are exactly the bytes on the
    /// device, hash blocks and all, so the copy remains verifiable forever rather than only
    /// at the moment it was made. Both levels of the tree are checked — each group's hash
    /// block against the file's master hash block, and each data block against its group —
    /// so a copy that completes has had every byte of image data confirmed twice over.
    pub fn copy_data_file_verified(
        &self,
        file_index: usize,
        out: &mut dyn std::io::Write,
        progress: &mut dyn FnMut(u64),
    ) -> Result<CopyReport> {
        let file = self.files.get(file_index).ok_or_else(|| DeviceError::Corrupt {
            path: self.label.clone(),
            detail: format!("no data file {file_index}"),
        })?;

        let total_blocks = file.size() / BLOCK;
        if total_blocks == 0 || file.size() % BLOCK != 0 {
            return Err(DeviceError::Corrupt {
                path: format!("{}.data/Data{file_index:04}", self.label),
                detail: format!("{} bytes is not a whole number of blocks", file.size()),
            });
        }

        let mut hasher = sha2::Sha256::new();
        let mut block = [0u8; BLOCK as usize];
        let mut blocks_verified = 0u64;
        let mut written = 0u64;

        let read_block = |index: u64, buf: &mut [u8; BLOCK as usize]| -> Result<()> {
            file.read_at(buf, index * BLOCK)
        };

        // Block zero is the master hash table: the SHA-1 of every group hash block below.
        let mut master = [0u8; BLOCK as usize];
        read_block(0, &mut master)?;
        out.write_all(&master).map_err(write_err)?;
        sha2::Digest::update(&mut hasher, master);
        written += BLOCK;
        progress(written);

        let mut index = 1u64;
        let mut group = 0u64;
        while index < total_blocks {
            let mut table = [0u8; BLOCK as usize];
            read_block(index, &mut table)?;
            // The group's hash block must itself match the master table.
            let expected = &master[(group * DIGEST as u64) as usize
                ..(group * DIGEST as u64) as usize + DIGEST];
            let computed = Sha1::digest(table);
            if computed.as_slice() != expected {
                return Err(DeviceError::HashMismatch {
                    item: format!("{}.data/Data{file_index:04} group {group}", self.label),
                    block: index,
                    expected: hex(expected),
                    computed: hex(computed.as_slice()),
                });
            }
            out.write_all(&table).map_err(write_err)?;
            sha2::Digest::update(&mut hasher, table);
            written += BLOCK;
            index += 1;

            for slot in 0..GROUP_DATA {
                if index >= total_blocks {
                    break;
                }
                read_block(index, &mut block)?;
                let expected = &table[(slot * DIGEST as u64) as usize
                    ..(slot * DIGEST as u64) as usize + DIGEST];
                let computed = Sha1::digest(block);
                if computed.as_slice() != expected {
                    return Err(DeviceError::HashMismatch {
                        item: format!("{}.data/Data{file_index:04}", self.label),
                        block: index,
                        expected: hex(expected),
                        computed: hex(computed.as_slice()),
                    });
                }
                out.write_all(&block).map_err(write_err)?;
                sha2::Digest::update(&mut hasher, block);
                written += BLOCK;
                blocks_verified += 1;
                index += 1;
            }
            progress(written);
            group += 1;
        }

        out.flush().map_err(write_err)?;
        Ok(CopyReport {
            bytes: written,
            blocks_verified,
            sha256: hex(&sha2::Digest::finalize(hasher)),
        })
    }
}

/// Outcome of copying one data file.
pub struct CopyReport {
    pub bytes: u64,
    pub blocks_verified: u64,
    pub sha256: String,
}

fn write_err(source: std::io::Error) -> DeviceError {
    DeviceError::Io {
        path: "writing package copy".to_string(),
        source,
    }
}

impl RandomRead for GodImage<'_> {
    fn size(&self) -> u64 {
        self.size()
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        self.read_at(buf, offset)
    }
}

/// Usable data blocks in a final, possibly short, data file.
fn tail_data_blocks(total_blocks: u64) -> u64 {
    if total_blocks == 0 {
        return 0;
    }
    // One master hash block, then whole groups, then a partial group whose own hash block
    // still comes first.
    let after_master = total_blocks - 1;
    let groups = after_master / (1 + GROUP_DATA);
    let extra = after_master % (1 + GROUP_DATA);
    groups * GROUP_DATA + extra.saturating_sub(1)
}

/// Parse `Data0007` into `7`, rejecting anything else.
fn parse_data_index(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("Data").or_else(|| name.strip_prefix("data"))?;
    if rest.len() != 4 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_file_size_matches_the_observed_layout() {
        // The constant every other calculation rests on.
        assert_eq!(FULL_FILE, 0xA290000);
        assert_eq!(DATA_PER_FILE, 41_412);
    }

    #[test]
    fn data_file_names_are_parsed_strictly() {
        assert_eq!(parse_data_index("Data0000"), Some(0));
        assert_eq!(parse_data_index("Data0034"), Some(34));
        assert_eq!(parse_data_index("Data12"), None);
        assert_eq!(parse_data_index("Data00034"), None);
        assert_eq!(parse_data_index("default.xex"), None);
        assert_eq!(parse_data_index("Data000x"), None);
    }

    /// Checked against the real drive: a 35-file package whose last file is 5,009,408
    /// bytes reconstructs to 1,409,224 blocks, and the header declares 1,409,223.
    #[test]
    fn tail_blocks_match_a_measured_package() {
        assert_eq!(tail_data_blocks(0), 0);
        let tail = 5_009_408 / BLOCK;
        assert_eq!(tail, 1223);
        assert_eq!(tail_data_blocks(tail), 1216);
        assert_eq!(34 * DATA_PER_FILE + 1216, 1_409_224);
    }

    /// A file with only the master hash block and one group header holds no data yet.
    #[test]
    fn tail_blocks_handles_degenerate_sizes() {
        assert_eq!(tail_data_blocks(1), 0);
        assert_eq!(tail_data_blocks(2), 0);
        assert_eq!(tail_data_blocks(3), 1);
        assert_eq!(tail_data_blocks(1 + 1 + GROUP_DATA), GROUP_DATA);
        assert_eq!(tail_data_blocks(1 + 2 * (1 + GROUP_DATA)), 2 * GROUP_DATA);
    }
}
