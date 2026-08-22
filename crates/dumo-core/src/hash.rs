//! Content hashing.
//!
//! Hashes are the backbone of the project's data-safety invariant: nothing is ever
//! deleted or overwritten until a hash proves its replacement is intact.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// 1 MiB read buffer — large enough to keep the disk busy, small enough to stay off the
/// stack and out of the way.
const CHUNK: usize = 1024 * 1024;

/// Hash a file with SHA-256, returning the lowercase hex digest and the byte count.
///
/// The byte count is returned alongside deliberately: comparing sizes before hashes
/// gives a cheap early mismatch signal, and recording both makes a later verification
/// failure easier to diagnose.
pub fn sha256_file(path: &Path) -> std::io::Result<(String, u64)> {
    let f = File::open(path)?;
    let mut reader = BufReader::with_capacity(CHUNK, f);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total: u64 = 0;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }

    Ok((hex(&hasher.finalize()), total))
}

/// Hash a file, reporting progress as bytes are consumed.
pub fn sha256_file_with_progress(
    path: &Path,
    mut on_progress: impl FnMut(u64),
) -> std::io::Result<(String, u64)> {
    let f = File::open(path)?;
    let mut reader = BufReader::with_capacity(CHUNK, f);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total: u64 = 0;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        on_progress(total);
    }

    Ok((hex(&hasher.finalize()), total))
}

/// The digest set Redump datfiles are keyed on.
///
/// Computed in a single pass over the file, since these images are gigabytes and reading
/// them three times would triple the cost for no benefit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RedumpDigests {
    pub size: u64,
    pub crc32: String,
    pub md5: String,
    pub sha1: String,
}

/// CRC-32 (IEEE 802.3), the variant Redump uses.
///
/// Implemented directly rather than pulled in as a dependency: it is a dozen lines, and
/// the known-answer tests below pin it to the standard.
struct Crc32 {
    value: u32,
}

impl Crc32 {
    fn new() -> Self {
        Self { value: 0xFFFF_FFFF }
    }

    fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.value ^= u32::from(b);
            for _ in 0..8 {
                let mask = (self.value & 1).wrapping_neg();
                // 0xEDB88320 is the reversed polynomial for CRC-32/IEEE.
                self.value = (self.value >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
    }

    fn finish(self) -> u32 {
        !self.value
    }
}

/// Compute CRC-32, MD5, and SHA-1 over bytes already in memory.
///
/// For content that is generated rather than read — a reconstructed cue sheet — where
/// the digests are needed to check the result against a datfile before it is written
/// anywhere.
pub fn redump_digests_of(data: &[u8]) -> RedumpDigests {
    use md5::Md5;
    use sha1::Sha1;

    let mut crc = Crc32::new();
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();
    crc.update(data);
    md5.update(data);
    sha1.update(data);

    RedumpDigests {
        size: data.len() as u64,
        crc32: format!("{:08x}", crc.finish()),
        md5: hex(&md5.finalize()),
        sha1: hex(&sha1.finalize()),
    }
}

/// SHA-256 of bytes already in memory.
pub fn sha256_of(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finalize())
}

/// Compute CRC-32, MD5, and SHA-1 for a file in one pass.
pub fn redump_digests(path: &Path) -> std::io::Result<RedumpDigests> {
    redump_digests_with_progress(path, |_| {})
}

/// As [`redump_digests`], reporting bytes consumed as it goes.
pub fn redump_digests_with_progress(
    path: &Path,
    mut on_progress: impl FnMut(u64),
) -> std::io::Result<RedumpDigests> {
    use md5::Md5;
    use sha1::Sha1;

    let f = File::open(path)?;
    let mut reader = BufReader::with_capacity(CHUNK, f);
    let mut buf = vec![0u8; CHUNK];

    let mut crc = Crc32::new();
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();
    let mut total: u64 = 0;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        crc.update(chunk);
        md5.update(chunk);
        sha1.update(chunk);
        total += n as u64;
        on_progress(total);
    }

    Ok(RedumpDigests {
        size: total,
        crc32: format!("{:08x}", crc.finish()),
        md5: hex(&md5.finalize()),
        sha1: hex(&sha1.finalize()),
    })
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("dumo-hash-test-{name}-{}", std::process::id()));
        let mut f = File::create(&p).unwrap();
        f.write_all(contents).unwrap();
        p
    }

    /// Known-answer test: the SHA-256 of "abc" is a published constant.
    #[test]
    fn matches_known_sha256_vector() {
        let p = temp_file("abc", b"abc");
        let (digest, len) = sha256_file(&p).unwrap();
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(len, 3);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn empty_file_hashes_to_empty_digest() {
        let p = temp_file("empty", b"");
        let (digest, len) = sha256_file(&p).unwrap();
        assert_eq!(
            digest,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(len, 0);
        std::fs::remove_file(p).ok();
    }

    /// Content larger than the read buffer must hash identically to a single-shot hash,
    /// proving the chunked loop is correct.
    #[test]
    fn multi_chunk_file_hashes_correctly() {
        let data: Vec<u8> = (0..(CHUNK * 2 + 12345)).map(|i| (i % 251) as u8).collect();
        let p = temp_file("large", &data);
        let (digest, len) = sha256_file(&p).unwrap();

        let expected = {
            let mut h = Sha256::new();
            h.update(&data);
            hex(&h.finalize())
        };
        assert_eq!(digest, expected);
        assert_eq!(len as usize, data.len());
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn progress_reports_reach_total() {
        let data = vec![7u8; CHUNK + 500];
        let p = temp_file("progress", &data);
        let mut last = 0;
        let (_, total) = sha256_file_with_progress(&p, |n| last = n).unwrap();
        assert_eq!(last, total);
        assert_eq!(total as usize, data.len());
        std::fs::remove_file(p).ok();
    }

    /// Known-answer vectors for all three Redump digests over "abc".
    #[test]
    fn redump_digests_match_known_vectors() {
        let p = temp_file("digests", b"abc");
        let d = redump_digests(&p).unwrap();
        assert_eq!(d.size, 3);
        assert_eq!(d.crc32, "352441c2");
        assert_eq!(d.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(d.sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        std::fs::remove_file(p).ok();
    }

    /// The canonical CRC-32 check value: "123456789" hashes to 0xCBF43926.
    #[test]
    fn crc32_matches_standard_check_value() {
        let p = temp_file("crccheck", b"123456789");
        assert_eq!(redump_digests(&p).unwrap().crc32, "cbf43926");
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn empty_file_digests() {
        let p = temp_file("emptydig", b"");
        let d = redump_digests(&p).unwrap();
        assert_eq!(d.crc32, "00000000");
        assert_eq!(d.md5, "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(d.sha1, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        std::fs::remove_file(p).ok();
    }

    /// Chunked reading must not change the result for content larger than the buffer.
    #[test]
    fn multi_chunk_digests_are_stable() {
        let data: Vec<u8> = (0..(CHUNK + 7777)).map(|i| (i % 253) as u8).collect();
        let p = temp_file("digbig", &data);
        let a = redump_digests(&p).unwrap();

        let expected_crc = {
            let mut c = Crc32::new();
            c.update(&data);
            format!("{:08x}", c.finish())
        };
        assert_eq!(a.crc32, expected_crc);
        assert_eq!(a.size as usize, data.len());
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn different_content_differs() {
        let a = temp_file("a", b"hello");
        let b = temp_file("b", b"hellp");
        assert_ne!(sha256_file(&a).unwrap().0, sha256_file(&b).unwrap().0);
        std::fs::remove_file(a).ok();
        std::fs::remove_file(b).ok();
    }
}
