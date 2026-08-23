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

/// Which chdman subcommand family a dump needs.
///
/// CD and DVD images are different enough that chdman has separate commands for them:
/// a CD is raw 2352-byte sectors with subchannel and a cue describing its tracks, a DVD
/// is a flat run of 2048-byte sectors. Using the wrong one either fails outright or
/// produces something an emulator will not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscFormat {
    /// `.cue` plus per-track `.bin`.
    Cd,
    /// A single `.iso`.
    Dvd,
}

impl DiscFormat {
    fn create(self) -> &'static str {
        match self {
            DiscFormat::Cd => "createcd",
            DiscFormat::Dvd => "createdvd",
        }
    }

    fn extract(self) -> &'static str {
        match self {
            DiscFormat::Cd => "extractcd",
            DiscFormat::Dvd => "extractdvd",
        }
    }
}

/// Pack a disc image into a single CHD.
///
/// For [`DiscFormat::Cd`], `input` is the `.cue`; chdman resolves its track files
/// relative to it, exactly as an emulator would. For [`DiscFormat::Dvd`] it is the
/// `.iso`.
pub fn create(input: &Path, out: &Path, format: DiscFormat) -> Result<()> {
    if out.exists() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: format!("{} already exists; refusing to overwrite", out.display()),
        });
    }
    run(&[
        format.create().as_ref(),
        "-i".as_ref(),
        input.as_ref(),
        "-o".as_ref(),
        out.as_ref(),
    ])
}

/// Unpack a CHD back to the image it was made from.
///
/// `out` is the `.cue` for a CD or the `.iso` for a DVD; `out_bin` names the track files
/// and is only meaningful for a CD.
pub fn extract(chd: &Path, out: &Path, out_bin: Option<&Path>, format: DiscFormat) -> Result<()> {
    let mut args: Vec<&std::ffi::OsStr> = vec![
        format.extract().as_ref(),
        "-i".as_ref(),
        chd.as_ref(),
        "-o".as_ref(),
        out.as_ref(),
    ];
    if let (DiscFormat::Cd, Some(b)) = (format, out_bin) {
        args.push("-ob".as_ref());
        args.push(b.as_ref());
    }
    run(&args)
}

/// Pack a `.cue`/`.bin` set into a single CHD.
///
/// `cue` must sit alongside the track files it names — chdman resolves them relative to
/// the cue, exactly as an emulator would.
pub fn create_cd(cue: &Path, out: &Path) -> Result<()> {
    create(cue, out, DiscFormat::Cd)
}

/// Unpack a CHD back to a `.cue` and its track files.
///
/// `out_cue` names the cue to write; chdman derives the track file names from `out_bin`.
pub fn extract_cd(chd: &Path, out_cue: &Path, out_bin: &Path) -> Result<()> {
    extract(chd, out_cue, Some(out_bin), DiscFormat::Cd)
}

/// What a round-trip produced, so the caller can compare it against Redump.
#[derive(Debug, Clone)]
pub struct RoundTrip {
    /// Track files chdman wrote, in the order the cue names them.
    pub tracks: Vec<PathBuf>,
    /// The index chdman wrote: a `.cue` for a CD, the `.iso` itself for a DVD. For a CD
    /// its *text* is chdman's own, not Redump's — only the track data is expected to
    /// round-trip byte for byte.
    pub cue: PathBuf,
}

/// Extract a CHD into `work_dir` so its contents can be checked against known hashes.
///
/// This exists because "CHD is lossless" is a claim about the format, not evidence about
/// this file on this disk. The pipeline's rule is that a transformation is only trusted
/// once its output has been read back and matched, and packing a dump into a CHD is no
/// different: the caller extracts, hashes, and compares against the Redump digests the
/// dump already verified against. Only then may the original tracks be released.
pub fn verify_roundtrip(
    chd: &Path,
    work_dir: &Path,
    stem: &str,
    format: DiscFormat,
) -> Result<RoundTrip> {
    std::fs::create_dir_all(work_dir).map_err(|e| BackendError::Io {
        tool: TOOL,
        source: e,
    })?;

    let (index, data_ext) = match format {
        DiscFormat::Cd => (work_dir.join(format!("{stem}.cue")), "bin"),
        DiscFormat::Dvd => (work_dir.join(format!("{stem}.iso")), "iso"),
    };
    match format {
        DiscFormat::Cd => extract(
            chd,
            &index,
            Some(&work_dir.join(format!("{stem}.bin"))),
            format,
        )?,
        DiscFormat::Dvd => extract(chd, &index, None, format)?,
    }

    // A multi-track disc extracts to "<stem> (Track N).bin" rather than the single name
    // we asked for, so discover what actually landed instead of assuming.
    let mut tracks: Vec<PathBuf> = std::fs::read_dir(work_dir)
        .map_err(|e| BackendError::Io {
            tool: TOOL,
            source: e,
        })?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == data_ext).unwrap_or(false))
        .collect();
    tracks.sort();

    if tracks.is_empty() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: "extractcd produced no track files".into(),
        });
    }
    Ok(RoundTrip {
        cue: index,
        tracks,
    })
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

    /// Is chdman available? Round-trip tests are skipped rather than failed without it,
    /// since it is an optional dependency.
    fn have_chdman() -> bool {
        version().is_ok()
    }

    fn scratch(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("dumo-chdman-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A DVD image round-trips byte for byte.
    ///
    /// This is the property the whole packaging step rests on — a CHD may replace the
    /// archival image only because the image can be recovered from it exactly. Verified
    /// here on a synthetic disc, and separately on a real 4.1 GB PS2 DVD whose extracted
    /// .iso reproduced Redump's sha1 f6a63934521febb2e0c83d78510dfe8e78bbf214.
    #[test]
    fn dvd_images_round_trip_byte_for_byte() {
        if !have_chdman() {
            return;
        }
        let dir = scratch("dvd");
        let iso = dir.join("game.iso");

        // A DVD image is a whole number of 2048-byte sectors. Mixed compressible and
        // incompressible content, so this exercises real compression rather than a run
        // of zeros that any container would handle.
        let mut data = Vec::new();
        for sector in 0..512u32 {
            let mut s = vec![0u8; 2048];
            for (i, b) in s.iter_mut().enumerate() {
                *b = ((sector as usize).wrapping_mul(31).wrapping_add(i * 7) % 251) as u8;
            }
            if sector % 4 == 0 {
                s.iter_mut().for_each(|b| *b = 0);
            }
            data.extend_from_slice(&s);
        }
        std::fs::write(&iso, &data).unwrap();

        let chd = dir.join("game.chd");
        create(&iso, &chd, DiscFormat::Dvd).expect("createdvd");
        assert!(chd.is_file());

        let out = dir.join("rt");
        let round = verify_roundtrip(&chd, &out, "game", DiscFormat::Dvd).expect("extractdvd");
        assert_eq!(round.tracks.len(), 1, "a DVD extracts to one image");
        assert_eq!(
            std::fs::read(&round.tracks[0]).unwrap(),
            data,
            "the extracted image must be byte-identical to the original"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The two formats must not be interchangeable by accident: pointing the DVD command
    /// at a cue sheet has to fail loudly rather than produce something unreadable.
    #[test]
    fn the_wrong_format_is_an_error_not_a_silent_mess() {
        if !have_chdman() {
            return;
        }
        let dir = scratch("mismatch");
        let cue = dir.join("game.cue");
        std::fs::write(&cue, b"FILE \"game.bin\" BINARY\r\n  TRACK 01 MODE1/2352\r\n").unwrap();

        let err = create(&cue, &dir.join("out.chd"), DiscFormat::Dvd).unwrap_err();
        assert!(matches!(err, BackendError::Failed { .. }), "got {err:?}");
        assert!(!dir.join("out.chd").is_file(), "no CHD should be left behind");

        std::fs::remove_dir_all(&dir).ok();
    }
}
