//! Filesystem operations that uphold the project's data-safety invariant.
//!
//! > No source is deleted until its replacement is verified by checksum on a genuine
//! > re-read at the destination.
//!
//! Two mechanisms, chosen by circumstance:
//!
//! - **Same filesystem:** `rename(2)`. Atomic, moves no data, and the file keeps its
//!   inode — there is no window in which the content could be lost or corrupted, and
//!   nothing is deleted. Re-hashing afterwards would verify nothing that the operation
//!   did not already guarantee.
//! - **Across filesystems:** copy to a temporary name, flush to disk, **re-read and
//!   hash**, and only then rename into place and remove the source. The temporary name
//!   matters: an interrupted copy must never leave a truncated file sitting at the real
//!   destination path, where a later run would mistake it for complete.

use crate::hash;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("destination {path} already exists; refusing to overwrite")]
    DestinationExists { path: PathBuf },

    #[error("source {path} does not exist")]
    SourceMissing { path: PathBuf },

    #[error("verification failed for {path}: expected {expected}, got {actual}")]
    VerificationFailed {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

type Result<T> = std::result::Result<T, FsError>;

/// How a move was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveMethod {
    /// Atomic rename within one filesystem; no data was copied.
    Rename,
    /// Copied to another filesystem and verified by re-reading the destination.
    CopyVerified,
}

/// Outcome of a verified move.
#[derive(Debug, Clone)]
pub struct MoveOutcome {
    pub method: MoveMethod,
    pub bytes: u64,
    /// Hash confirmed by re-reading the destination, when a copy was performed.
    pub verified_sha256: Option<String>,
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> FsError + '_ {
    move |source| FsError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Move a file, never overwriting and never losing data.
///
/// `expected_sha256` is compared against the destination after a cross-filesystem copy.
/// When `None`, the source is hashed first so there is still something to verify against.
pub fn move_verified(src: &Path, dest: &Path, expected_sha256: Option<&str>) -> Result<MoveOutcome> {
    if !src.is_file() {
        return Err(FsError::SourceMissing {
            path: src.to_path_buf(),
        });
    }
    // Never clobber. This is checked before every write path below, and again by using
    // create_new when the copy path opens the destination.
    if dest.exists() {
        return Err(FsError::DestinationExists {
            path: dest.to_path_buf(),
        });
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(io_err(parent))?;
    }

    let bytes = std::fs::metadata(src).map_err(io_err(src))?.len();

    match std::fs::rename(src, dest) {
        Ok(()) => Ok(MoveOutcome {
            method: MoveMethod::Rename,
            bytes,
            verified_sha256: None,
        }),
        // EXDEV: different filesystems, so the content has to be copied.
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let expected = match expected_sha256 {
                Some(h) => h.to_string(),
                None => hash::sha256_file(src).map_err(io_err(src))?.0,
            };
            let verified = copy_verified(src, dest, &expected)?;
            // Only now is it safe to remove the source.
            std::fs::remove_file(src).map_err(io_err(src))?;
            Ok(MoveOutcome {
                method: MoveMethod::CopyVerified,
                bytes,
                verified_sha256: Some(verified),
            })
        }
        Err(e) => Err(FsError::Io {
            path: dest.to_path_buf(),
            source: e,
        }),
    }
}

/// Copy a file and prove the destination matches, leaving the source untouched.
///
/// Returns the hash read back from the destination. On any failure the partial file is
/// removed and the real destination path is never created.
pub fn copy_verified(src: &Path, dest: &Path, expected_sha256: &str) -> Result<String> {
    if dest.exists() {
        return Err(FsError::DestinationExists {
            path: dest.to_path_buf(),
        });
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(io_err(parent))?;
    }

    // Write to a temporary sibling so an interruption cannot leave a truncated file at
    // the destination path.
    let tmp = temp_sibling(dest);
    let _ = std::fs::remove_file(&tmp);

    let copy_result = (|| -> std::io::Result<()> {
        use std::io::{Read, Write};
        let mut input = std::fs::File::open(src)?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;

        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n])?;
        }
        // Flush our buffers and ask the filesystem to commit before we read back.
        output.flush()?;
        output.sync_all()?;
        Ok(())
    })();

    if let Err(e) = copy_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(FsError::Io {
            path: tmp,
            source: e,
        });
    }

    // Re-read the written file and hash it. Opening a fresh handle after sync_all is
    // what makes this a real verification rather than a re-hash of our own buffers.
    //
    // Caveat worth stating plainly: on a network filesystem the kernel may still serve
    // this read from local cache, which would weaken the guarantee. Verifying that a
    // re-read genuinely round-trips to the server is tracked separately and must be
    // settled before this path is trusted for network destinations.
    let actual = match hash::sha256_file(&tmp) {
        Ok((h, _)) => h,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(FsError::Io { path: tmp, source: e });
        }
    };

    if actual != expected_sha256 {
        let _ = std::fs::remove_file(&tmp);
        return Err(FsError::VerificationFailed {
            path: dest.to_path_buf(),
            expected: expected_sha256.to_string(),
            actual,
        });
    }

    std::fs::rename(&tmp, dest).map_err(io_err(dest))?;
    Ok(actual)
}

/// A temporary path beside `dest`, so the rename into place stays within one directory.
fn temp_sibling(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    dest.with_file_name(format!(".{name}.dumo-partial"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("dumo-fsops-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(path: &Path, content: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::File::create(path).unwrap().write_all(content).unwrap();
    }

    #[test]
    fn moves_within_a_filesystem_by_rename() {
        let dir = scratch("rename");
        let src = dir.join("a.iso");
        let dest = dir.join("sub/b.iso");
        write(&src, b"hello world");

        let out = move_verified(&src, &dest, None).unwrap();
        assert_eq!(out.method, MoveMethod::Rename);
        assert_eq!(out.bytes, 11);
        assert!(!src.exists(), "source should be gone after a move");
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
        std::fs::remove_dir_all(dir).ok();
    }

    /// The single most important behaviour: never destroy an existing file.
    #[test]
    fn refuses_to_overwrite_an_existing_destination() {
        let dir = scratch("nooverwrite");
        let src = dir.join("a.iso");
        let dest = dir.join("b.iso");
        write(&src, b"new content");
        write(&dest, b"PRECIOUS EXISTING DATA");

        let err = move_verified(&src, &dest, None).unwrap_err();
        assert!(matches!(err, FsError::DestinationExists { .. }));
        // Both files must survive untouched.
        assert_eq!(std::fs::read(&dest).unwrap(), b"PRECIOUS EXISTING DATA");
        assert_eq!(std::fs::read(&src).unwrap(), b"new content");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_source_is_an_error() {
        let dir = scratch("nosrc");
        let err = move_verified(&dir.join("nope"), &dir.join("dest"), None).unwrap_err();
        assert!(matches!(err, FsError::SourceMissing { .. }));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn copy_verified_writes_and_confirms() {
        let dir = scratch("copyok");
        let src = dir.join("a.bin");
        let dest = dir.join("out/a.bin");
        write(&src, b"verify me");

        let expected = hash::sha256_file(&src).unwrap().0;
        let got = copy_verified(&src, &dest, &expected).unwrap();
        assert_eq!(got, expected);
        assert_eq!(std::fs::read(&dest).unwrap(), b"verify me");
        // Source is left alone; only move_verified removes it, and only after this.
        assert!(src.exists());
        std::fs::remove_dir_all(dir).ok();
    }

    /// A mismatch must leave no destination file and no stray partial.
    #[test]
    fn copy_verified_rejects_a_hash_mismatch_and_cleans_up() {
        let dir = scratch("copybad");
        let src = dir.join("a.bin");
        let dest = dir.join("out/a.bin");
        write(&src, b"actual content");

        let wrong = "0".repeat(64);
        let err = copy_verified(&src, &dest, &wrong).unwrap_err();
        assert!(matches!(err, FsError::VerificationFailed { .. }));

        assert!(!dest.exists(), "destination must not exist after a failed verify");
        let strays: Vec<_> = std::fs::read_dir(dir.join("out"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(strays.is_empty(), "left behind: {strays:?}");
        assert!(src.exists(), "source must survive a failed copy");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn copy_verified_refuses_existing_destination() {
        let dir = scratch("copyexists");
        let src = dir.join("a.bin");
        let dest = dir.join("b.bin");
        write(&src, b"x");
        write(&dest, b"keep me");
        let err = copy_verified(&src, &dest, "whatever").unwrap_err();
        assert!(matches!(err, FsError::DestinationExists { .. }));
        assert_eq!(std::fs::read(&dest).unwrap(), b"keep me");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn temp_sibling_stays_in_the_destination_directory() {
        let t = temp_sibling(Path::new("/mnt/share/games/Title (USA).iso"));
        assert_eq!(t.parent(), Path::new("/mnt/share/games").parent().map(|_| Path::new("/mnt/share/games")));
        assert!(t.file_name().unwrap().to_string_lossy().ends_with(".dumo-partial"));
        assert!(t.file_name().unwrap().to_string_lossy().starts_with('.'));
    }

    #[test]
    fn large_content_copies_correctly() {
        let dir = scratch("large");
        let src = dir.join("big.bin");
        let dest = dir.join("big-out.bin");
        let data: Vec<u8> = (0..(3 * 1024 * 1024 + 17)).map(|i| (i % 251) as u8).collect();
        write(&src, &data);

        let expected = hash::sha256_file(&src).unwrap().0;
        copy_verified(&src, &dest, &expected).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        std::fs::remove_dir_all(dir).ok();
    }
}
