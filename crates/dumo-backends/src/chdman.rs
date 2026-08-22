//! chdman adapter: pack CD dumps into the format emulators actually read.
//!
//! A Redump CD dump is a `.cue` plus one `.bin` per track. That is the right *archival*
//! form, but it is a poor fit for an emulator library. Measured against this machine's
//! own ES-DE configuration: the `ps2` system does not list `.cue` as a scannable
//! extension at all (PCSX2 has no cue parser and reads the raw `.bin` directly), while
//! `psx` lists both `.cue` and `.bin`, so a single game appears twice.
//!
//! CHD solves both. It is one file, it is in every relevant system's extension list, and
//! it is read natively by PCSX2, DuckStation and the RetroArch disc cores. It is also
//! substantially smaller.
//!
//! The property that makes it acceptable *here*, though, is that it is losslessly
//! reversible: `chdman extractcd` reproduces the original track data byte for byte. That
//! is what lets a CHD replace the archival files rather than sit beside them — but only
//! if it is proven, per dump, rather than assumed. See [`verify_roundtrip`].

use crate::{BackendError, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const TOOL: &str = "chdman";

/// Report chdman's version, or that it is missing.
///
/// chdman prints its banner and then exits non-zero when given no usable command, so the
/// exit status is deliberately ignored here; the banner on stdout is the signal.
pub fn version() -> Result<String> {
    let out = Command::new(TOOL).arg("--version").output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            BackendError::NotInstalled { tool: TOOL }
        } else {
            BackendError::Io {
                tool: TOOL,
                source: e,
            }
        }
    })?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();
    if line.is_empty() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: "no version output".into(),
        });
    }
    Ok(line)
}

fn run(args: &[&std::ffi::OsStr]) -> Result<()> {
    let out = Command::new(TOOL).args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            BackendError::NotInstalled { tool: TOOL }
        } else {
            BackendError::Io {
                tool: TOOL,
                source: e,
            }
        }
    })?;
    if !out.status.success() {
        // chdman writes its diagnostics to stderr; keep the last few lines, since the
        // useful message is at the end after the banner.
        let err = String::from_utf8_lossy(&out.stderr);
        let detail: Vec<&str> = err
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(3)
            .collect();
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: detail.into_iter().rev().collect::<Vec<_>>().join("; "),
        });
    }
    Ok(())
}

/// Pack a `.cue`/`.bin` set into a single CHD.
///
/// `cue` must sit alongside the track files it names — chdman resolves them relative to
/// the cue, exactly as an emulator would.
pub fn create_cd(cue: &Path, out: &Path) -> Result<()> {
    if out.exists() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: format!("{} already exists; refusing to overwrite", out.display()),
        });
    }
    run(&[
        "createcd".as_ref(),
        "-i".as_ref(),
        cue.as_ref(),
        "-o".as_ref(),
        out.as_ref(),
    ])
}

/// Unpack a CHD back to a `.cue` and its track files.
///
/// `out_cue` names the cue to write; chdman derives the track file names from `out_bin`.
pub fn extract_cd(chd: &Path, out_cue: &Path, out_bin: &Path) -> Result<()> {
    run(&[
        "extractcd".as_ref(),
        "-i".as_ref(),
        chd.as_ref(),
        "-o".as_ref(),
        out_cue.as_ref(),
        "-ob".as_ref(),
        out_bin.as_ref(),
    ])
}

/// What a round-trip produced, so the caller can compare it against Redump.
#[derive(Debug, Clone)]
pub struct RoundTrip {
    /// Track files chdman wrote, in the order the cue names them.
    pub tracks: Vec<PathBuf>,
    /// The cue chdman wrote. Its *text* is chdman's own, not Redump's — only the track
    /// data is expected to round-trip byte for byte.
    pub cue: PathBuf,
}

/// Extract a CHD into `work_dir` so its contents can be checked against known hashes.
///
/// This exists because "CHD is lossless" is a claim about the format, not evidence about
/// this file on this disk. The pipeline's rule is that a transformation is only trusted
/// once its output has been read back and matched, and packing a dump into a CHD is no
/// different: the caller extracts, hashes, and compares against the Redump digests the
/// dump already verified against. Only then may the original tracks be released.
pub fn verify_roundtrip(chd: &Path, work_dir: &Path, stem: &str) -> Result<RoundTrip> {
    std::fs::create_dir_all(work_dir).map_err(|e| BackendError::Io {
        tool: TOOL,
        source: e,
    })?;
    let cue = work_dir.join(format!("{stem}.cue"));
    let bin = work_dir.join(format!("{stem}.bin"));
    extract_cd(chd, &cue, &bin)?;

    // A multi-track disc extracts to "<stem> (Track N).bin" rather than the single name
    // we asked for, so discover what actually landed instead of assuming.
    let mut tracks: Vec<PathBuf> = std::fs::read_dir(work_dir)
        .map_err(|e| BackendError::Io {
            tool: TOOL,
            source: e,
        })?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "bin").unwrap_or(false))
        .collect();
    tracks.sort();

    if tracks.is_empty() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: "extractcd produced no track files".into(),
        });
    }
    Ok(RoundTrip { cue, tracks })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Refusing to overwrite is checked before chdman is even consulted, so this holds
    /// whether or not the tool is installed.
    #[test]
    fn create_refuses_an_existing_destination() {
        let dir = std::env::temp_dir().join(format!("dumo-chd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("x.chd");
        std::fs::write(&out, b"existing").unwrap();

        let err = create_cd(&dir.join("x.cue"), &out).unwrap_err();
        assert!(matches!(err, BackendError::Failed { .. }));
        // The existing file is untouched.
        assert_eq!(std::fs::read(&out).unwrap(), b"existing");
        std::fs::remove_dir_all(&dir).ok();
    }
}
