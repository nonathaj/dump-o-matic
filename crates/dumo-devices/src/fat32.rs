//! Read-only FAT32 reader.
//!
//! Console storage is read here without mounting it. That is a deliberate choice, not an
//! inconvenience: mounting a FAT volume read-write lets the kernel update its dirty bit
//! and free-space hints, which is a write to media we are supposed to be preserving. A
//! read-only mount avoids that, but still requires privilege the tool may not have, and
//! fails outright on a volume the kernel dislikes. Parsing the filesystem ourselves means
//! the device is only ever opened `O_RDONLY` and only ever read.
//!
//! Only what is needed to find and read content files is implemented: the BPB, the FAT
//! chain, long filenames, and positioned reads. There is no write path at all, by
//! construction — nothing in this module can modify a byte.

use crate::{DeviceError, Result};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Attribute bit marking a directory entry.
pub const ATTR_DIRECTORY: u8 = 0x10;
/// Attribute byte marking a long-filename fragment rather than a real entry.
const ATTR_LFN: u8 = 0x0f;
/// Directory entry size, fixed by the format.
const ENTRY_SIZE: usize = 32;

/// A cluster value at or above this marks the end of a chain.
const CHAIN_END: u32 = 0x0fff_fff8;

/// Refuse to follow a cluster chain longer than this.
///
/// A corrupt or hostile FAT can contain a cycle, which would otherwise spin forever. The
/// bound is generous — 2^26 clusters is a petabyte at 16 KiB clusters — so it can only be
/// hit by a chain that is wrong.
const MAX_CHAIN: usize = 1 << 26;

/// One entry in a directory.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub attributes: u8,
    pub first_cluster: u32,
    pub size: u64,
}

impl DirEntry {
    pub fn is_dir(&self) -> bool {
        self.attributes & ATTR_DIRECTORY != 0
    }
}

/// A mounted-by-us FAT32 volume, opened read-only.
pub struct Fat32 {
    file: File,
    source: String,
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    reserved_sectors: u32,
    fat_count: u32,
    sectors_per_fat: u32,
    root_cluster: u32,
    total_sectors: u64,
    /// OEM name from the BPB. The Xbox 360 writes `XBOX360` here when it formats a drive,
    /// which is a useful hint even though it is not proof of anything.
    pub oem_name: String,
    first_data_sector: u32,
    cluster_bytes: u32,
}

impl Fat32 {
    /// Open a block device or image file and parse its FAT32 geometry.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => DeviceError::PermissionDenied {
                path: path.display().to_string(),
            },
            std::io::ErrorKind::NotFound => DeviceError::NotFound {
                path: path.display().to_string(),
            },
            _ => DeviceError::Io {
                path: path.display().to_string(),
                source: e,
            },
        })?;
        Self::from_file(file, path.display().to_string())
    }

    fn from_file(file: File, source: String) -> Result<Self> {
        let mut boot = [0u8; 512];
        file.read_exact_at(&mut boot, 0).map_err(|e| DeviceError::Io {
            path: source.clone(),
            source: e,
        })?;

        let bytes_per_sector = u16le(&boot, 0x0b) as u32;
        let sectors_per_cluster = boot[0x0d] as u32;
        let reserved_sectors = u16le(&boot, 0x0e) as u32;
        let fat_count = boot[0x10] as u32;
        let root_entries = u16le(&boot, 0x11);
        let sectors_per_fat = u32le(&boot, 0x24);
        let root_cluster = u32le(&boot, 0x2c);
        let total_sectors = match u16le(&boot, 0x13) {
            0 => u32le(&boot, 0x20) as u64,
            n => n as u64,
        };

        // Validate before trusting any of it: a wrong geometry would turn every later
        // read into a read of unrelated bytes, which is a far more confusing failure than
        // refusing here.
        let bad = |detail: &str| DeviceError::NotFat32 {
            path: source.clone(),
            detail: detail.to_string(),
        };
        if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
            return Err(bad(&format!("bytes per sector is {bytes_per_sector}")));
        }
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
            return Err(bad(&format!(
                "sectors per cluster is {sectors_per_cluster}"
            )));
        }
        // FAT32 is identified by a zero 16-bit root entry count and a 32-bit FAT size.
        if root_entries != 0 || sectors_per_fat == 0 {
            return Err(bad("not a FAT32 volume (16-bit root directory)"));
        }
        if fat_count == 0 || fat_count > 4 {
            return Err(bad(&format!("{fat_count} file allocation tables")));
        }
        if root_cluster < 2 {
            return Err(bad(&format!("root cluster is {root_cluster}")));
        }
        if total_sectors == 0 {
            return Err(bad("volume reports zero sectors"));
        }

        let oem_name = String::from_utf8_lossy(&boot[3..11]).trim().to_string();
        let first_data_sector = reserved_sectors + fat_count * sectors_per_fat;
        let cluster_bytes = bytes_per_sector * sectors_per_cluster;

        Ok(Self {
            file,
            source,
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            fat_count,
            sectors_per_fat,
            root_cluster,
            total_sectors,
            oem_name,
            first_data_sector,
            cluster_bytes,
        })
    }

    pub fn cluster_bytes(&self) -> u32 {
        self.cluster_bytes
    }

    pub fn volume_bytes(&self) -> u64 {
        self.total_sectors * self.bytes_per_sector as u64
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Byte offset of a data cluster.
    fn cluster_offset(&self, cluster: u32) -> u64 {
        let sector =
            self.first_data_sector as u64 + (cluster as u64 - 2) * self.sectors_per_cluster as u64;
        sector * self.bytes_per_sector as u64
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        self.file
            .read_exact_at(buf, offset)
            .map_err(|e| DeviceError::Io {
                path: self.source.clone(),
                source: e,
            })
    }

    /// Follow the FAT to read the next cluster in a chain.
    fn next_cluster(&self, cluster: u32) -> Result<u32> {
        let offset = self.reserved_sectors as u64 * self.bytes_per_sector as u64 + cluster as u64 * 4;
        let mut buf = [0u8; 4];
        self.read_at(&mut buf, offset)?;
        // The top four bits are reserved and must be masked before comparing.
        Ok(u32::from_le_bytes(buf) & 0x0fff_ffff)
    }

    /// Collect the cluster chain starting at `cluster`.
    pub fn chain(&self, cluster: u32) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut c = cluster;
        while c >= 2 && c < CHAIN_END {
            out.push(c);
            if out.len() >= MAX_CHAIN {
                return Err(DeviceError::Corrupt {
                    path: self.source.clone(),
                    detail: format!("cluster chain from {cluster} does not terminate"),
                });
            }
            c = self.next_cluster(c)?;
        }
        Ok(out)
    }

    /// Read a whole chain into memory. Only used for directories, which are small.
    fn read_chain(&self, cluster: u32) -> Result<Vec<u8>> {
        let clusters = self.chain(cluster)?;
        let mut out = vec![0u8; clusters.len() * self.cluster_bytes as usize];
        for (i, c) in clusters.iter().enumerate() {
            let start = i * self.cluster_bytes as usize;
            let end = start + self.cluster_bytes as usize;
            self.read_at(&mut out[start..end], self.cluster_offset(*c))?;
        }
        Ok(out)
    }

    pub fn root_cluster(&self) -> u32 {
        self.root_cluster
    }

    /// List a directory by its first cluster.
    pub fn read_dir(&self, cluster: u32) -> Result<Vec<DirEntry>> {
        let data = self.read_chain(cluster)?;
        Ok(parse_directory(&data))
    }

    /// Resolve a `/`-separated path from the volume root.
    pub fn resolve(&self, path: &str) -> Result<DirEntry> {
        let mut current = DirEntry {
            name: String::new(),
            attributes: ATTR_DIRECTORY,
            first_cluster: self.root_cluster,
            size: 0,
        };
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !current.is_dir() {
                return Err(DeviceError::NotFound {
                    path: format!("{}:{path}", self.source),
                });
            }
            let entries = self.read_dir(current.first_cluster)?;
            current = entries
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(part))
                .ok_or_else(|| DeviceError::NotFound {
                    path: format!("{}:{path}", self.source),
                })?;
        }
        Ok(current)
    }

    /// Open a file for positioned reads.
    pub fn open_file(&self, path: &str) -> Result<Fat32File<'_>> {
        let entry = self.resolve(path)?;
        if entry.is_dir() {
            return Err(DeviceError::NotFound {
                path: format!("{}:{path} is a directory", self.source),
            });
        }
        self.open_entry(&entry)
    }

    /// Open a file from an entry already listed, avoiding a second directory walk.
    pub fn open_entry(&self, entry: &DirEntry) -> Result<Fat32File<'_>> {
        Ok(Fat32File {
            fs: self,
            clusters: self.chain(entry.first_cluster)?,
            size: entry.size,
        })
    }

    /// Read a whole small file. Refuses anything above `limit`, so a wrong path cannot
    /// turn into a multi-gigabyte allocation.
    pub fn read_file(&self, path: &str, limit: u64) -> Result<Vec<u8>> {
        let f = self.open_file(path)?;
        if f.size > limit {
            return Err(DeviceError::Corrupt {
                path: format!("{}:{path}", self.source),
                detail: format!("expected at most {limit} bytes, found {}", f.size),
            });
        }
        let mut buf = vec![0u8; f.size as usize];
        f.read_at(&mut buf, 0)?;
        Ok(buf)
    }
}

/// A file on the volume, supporting positioned reads through its cluster chain.
pub struct Fat32File<'a> {
    fs: &'a Fat32,
    clusters: Vec<u32>,
    size: u64,
}

impl Fat32File<'_> {
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Read exactly `buf.len()` bytes from `offset`.
    ///
    /// Short reads are an error rather than a silent truncation: every caller here is
    /// reconstructing an image, where a quietly zero-filled gap would corrupt the output
    /// without failing.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        if offset.saturating_add(buf.len() as u64) > self.size {
            return Err(DeviceError::Corrupt {
                path: self.fs.source.clone(),
                detail: format!(
                    "read of {} bytes at {offset} runs past the end of a {}-byte file",
                    buf.len(),
                    self.size
                ),
            });
        }
        let cluster_bytes = self.fs.cluster_bytes as u64;
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let index = (pos / cluster_bytes) as usize;
            let inner = pos % cluster_bytes;
            let cluster = *self.clusters.get(index).ok_or_else(|| DeviceError::Corrupt {
                path: self.fs.source.clone(),
                detail: format!("cluster {index} missing from chain"),
            })?;
            let take = ((cluster_bytes - inner) as usize).min(buf.len() - done);
            self.fs.read_at(
                &mut buf[done..done + take],
                self.fs.cluster_offset(cluster) + inner,
            )?;
            done += take;
        }
        Ok(())
    }
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Parse a directory's raw bytes into entries, assembling long filenames.
fn parse_directory(data: &[u8]) -> Vec<DirEntry> {
    let mut out = Vec::new();
    let mut lfn: Vec<(u8, String)> = Vec::new();

    for chunk in data.chunks_exact(ENTRY_SIZE) {
        match chunk[0] {
            // End of directory: nothing beyond this point is allocated.
            0x00 => break,
            // Deleted entry; any long-name fragments before it belong to it, not to the
            // next live entry.
            0xe5 => {
                lfn.clear();
                continue;
            }
            _ => {}
        }

        let attributes = chunk[0x0b];
        if attributes == ATTR_LFN {
            let sequence = chunk[0] & 0x3f;
            let mut units = Vec::new();
            for range in [0x01..0x0b, 0x0e..0x1a, 0x1c..0x20] {
                for pair in chunk[range].chunks_exact(2) {
                    units.push(u16::from_le_bytes([pair[0], pair[1]]));
                }
            }
            let text: String = String::from_utf16_lossy(&units);
            let text = text.split('\0').next().unwrap_or_default().to_string();
            lfn.push((sequence, text));
            continue;
        }

        // Volume label entries are not files and have no meaningful name here.
        if attributes & 0x08 != 0 && attributes & ATTR_DIRECTORY == 0 {
            lfn.clear();
            continue;
        }

        let name = if lfn.is_empty() {
            short_name(chunk)
        } else {
            // Fragments are stored last-first; ordering by sequence rebuilds the name.
            lfn.sort_by_key(|(seq, _)| *seq);
            lfn.iter().map(|(_, s)| s.as_str()).collect()
        };
        lfn.clear();

        if name == "." || name == ".." || name.is_empty() {
            continue;
        }

        out.push(DirEntry {
            name,
            attributes,
            first_cluster: (u16le(chunk, 0x14) as u32) << 16 | u16le(chunk, 0x1a) as u32,
            size: u32le(chunk, 0x1c) as u64,
        });
    }
    out
}

/// Rebuild an 8.3 short name.
fn short_name(chunk: &[u8]) -> String {
    let base = String::from_utf8_lossy(&chunk[0..8]).trim_end().to_string();
    let ext = String::from_utf8_lossy(&chunk[8..11]).trim_end().to_string();
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but structurally valid FAT32 volume in memory.
    ///
    /// Synthetic on purpose: the real drives this code reads are 250 GB and cannot live in
    /// a test suite, but every structure that matters — the BPB, a multi-cluster chain, a
    /// long filename — is small enough to construct exactly.
    fn synthetic_volume() -> (tempdir::Dir, std::path::PathBuf) {
        let dir = tempdir::Dir::new("fat32");
        let path = dir.path().join("volume.img");

        let bps: u32 = 512;
        let spc: u32 = 1;
        let reserved: u32 = 4;
        let fats: u32 = 1;
        let sectors_per_fat: u32 = 8;
        let total_sectors: u32 = 256;

        let mut img = vec![0u8; (total_sectors * bps) as usize];
        img[0..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
        img[3..11].copy_from_slice(b"XBOX360 ");
        img[0x0b..0x0d].copy_from_slice(&(bps as u16).to_le_bytes());
        img[0x0d] = spc as u8;
        img[0x0e..0x10].copy_from_slice(&(reserved as u16).to_le_bytes());
        img[0x10] = fats as u8;
        img[0x15] = 0xf8;
        img[0x20..0x24].copy_from_slice(&total_sectors.to_le_bytes());
        img[0x24..0x28].copy_from_slice(&sectors_per_fat.to_le_bytes());
        img[0x2c..0x30].copy_from_slice(&2u32.to_le_bytes());

        let fat_start = (reserved * bps) as usize;
        let data_start = ((reserved + fats * sectors_per_fat) * bps) as usize;
        let cluster_at = |n: u32| data_start + ((n - 2) * bps) as usize;
        let mut set_fat = |img: &mut Vec<u8>, cluster: u32, value: u32| {
            let o = fat_start + cluster as usize * 4;
            img[o..o + 4].copy_from_slice(&value.to_le_bytes());
        };

        // Cluster 2: root directory. Cluster 3-4: a two-cluster file, chained.
        set_fat(&mut img, 2, 0x0fff_ffff);
        set_fat(&mut img, 3, 4);
        set_fat(&mut img, 4, 0x0fff_ffff);

        // A long filename ("Data0000.bin") plus its 8.3 entry, in the root directory.
        let root = cluster_at(2);
        let long = "Data0000.bin";
        let mut units: Vec<u16> = long.encode_utf16().collect();
        units.push(0);
        let mut lfn = [0xffu8; ENTRY_SIZE];
        lfn[0] = 0x41; // sequence 1, last fragment
        lfn[0x0b] = ATTR_LFN;
        lfn[0x0c] = 0;
        lfn[0x1a] = 0;
        lfn[0x1b] = 0;
        for (i, u) in units.iter().enumerate().take(5) {
            let o = 0x01 + i * 2;
            lfn[o..o + 2].copy_from_slice(&u.to_le_bytes());
        }
        for (i, u) in units.iter().enumerate().skip(5).take(6) {
            let o = 0x0e + (i - 5) * 2;
            lfn[o..o + 2].copy_from_slice(&u.to_le_bytes());
        }
        for (i, u) in units.iter().enumerate().skip(11).take(2) {
            let o = 0x1c + (i - 11) * 2;
            lfn[o..o + 2].copy_from_slice(&u.to_le_bytes());
        }
        img[root..root + ENTRY_SIZE].copy_from_slice(&lfn);

        let mut entry = [0u8; ENTRY_SIZE];
        entry[0..11].copy_from_slice(b"DATA0000BIN");
        entry[0x0b] = 0x20;
        entry[0x14..0x16].copy_from_slice(&0u16.to_le_bytes());
        entry[0x1a..0x1c].copy_from_slice(&3u16.to_le_bytes());
        entry[0x1c..0x20].copy_from_slice(&600u32.to_le_bytes());
        img[root + ENTRY_SIZE..root + 2 * ENTRY_SIZE].copy_from_slice(&entry);

        // Distinguishable contents across the cluster boundary.
        for i in 0..512usize {
            img[cluster_at(3) + i] = (i % 251) as u8;
        }
        for i in 0..88usize {
            img[cluster_at(4) + i] = 0xa5;
        }

        std::fs::write(&path, &img).unwrap();
        (dir, path)
    }

    /// Minimal scoped temporary directory, to avoid a dev-dependency.
    mod tempdir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new(tag: &str) -> Self {
                let mut p = std::env::temp_dir();
                let n = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                p.push(format!("dumo-{tag}-{n}"));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn parses_geometry_and_oem_name() {
        let (_d, path) = synthetic_volume();
        let fs = Fat32::open(&path).unwrap();
        assert_eq!(fs.oem_name, "XBOX360");
        assert_eq!(fs.cluster_bytes(), 512);
        assert_eq!(fs.volume_bytes(), 256 * 512);
    }

    #[test]
    fn reads_a_long_filename() {
        let (_d, path) = synthetic_volume();
        let fs = Fat32::open(&path).unwrap();
        let entries = fs.read_dir(fs.root_cluster()).unwrap();
        assert_eq!(entries.len(), 1, "got {entries:?}");
        assert_eq!(entries[0].name, "Data0000.bin");
        assert_eq!(entries[0].size, 600);
    }

    /// The case that matters for image reconstruction: a read spanning two clusters must
    /// follow the chain, not run off the end of the first one.
    #[test]
    fn reads_across_a_cluster_boundary() {
        let (_d, path) = synthetic_volume();
        let fs = Fat32::open(&path).unwrap();
        let f = fs.open_file("Data0000.bin").unwrap();
        assert_eq!(f.size(), 600);
        let mut buf = vec![0u8; 600];
        f.read_at(&mut buf, 0).unwrap();
        for (i, b) in buf.iter().enumerate().take(512) {
            assert_eq!(*b, (i % 251) as u8, "byte {i}");
        }
        assert!(buf[512..].iter().all(|b| *b == 0xa5));

        // A positioned read straddling the boundary must agree with the whole-file read.
        let mut mid = vec![0u8; 16];
        f.read_at(&mut mid, 504).unwrap();
        assert_eq!(mid, buf[504..520]);
    }

    #[test]
    fn reading_past_the_end_is_an_error() {
        let (_d, path) = synthetic_volume();
        let fs = Fat32::open(&path).unwrap();
        let f = fs.open_file("Data0000.bin").unwrap();
        let mut buf = vec![0u8; 8];
        assert!(f.read_at(&mut buf, 596).is_err());
    }

    #[test]
    fn a_non_fat32_volume_is_refused() {
        let d = tempdir::Dir::new("notfat");
        let path = d.path().join("zero.img");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let err = Fat32::open(&path).unwrap_err();
        assert!(matches!(err, DeviceError::NotFat32 { .. }), "{err:?}");
    }

    #[test]
    fn resolve_is_case_insensitive() {
        let (_d, path) = synthetic_volume();
        let fs = Fat32::open(&path).unwrap();
        assert!(fs.resolve("DATA0000.BIN").is_ok());
        assert!(fs.resolve("data0000.bin").is_ok());
        assert!(fs.resolve("nope.bin").is_err());
    }
}
