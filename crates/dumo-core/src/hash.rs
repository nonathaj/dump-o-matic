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

    #[test]
    fn different_content_differs() {
        let a = temp_file("a", b"hello");
        let b = temp_file("b", b"hellp");
        assert_ne!(sha256_file(&a).unwrap().0, sha256_file(&b).unwrap().0);
        std::fs::remove_file(a).ok();
        std::fs::remove_file(b).ok();
    }
}
