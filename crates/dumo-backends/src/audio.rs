//! Audio CD tracks: cutting PCM out of a redumper dump, and encoding it with LAME.
//!
//! redumper writes one `.bin` per track, each starting at that track's pregap (Redump's
//! convention). Laid end to end they are the whole program area, sample for sample, so
//! they are treated as one stream and cut at the TOC's track starts instead. That puts
//! each pregap at the end of the previous track — what most music rippers do by default,
//! and the boundaries AccurateRip's checksums are computed over.

use crate::{BackendError, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Bytes in one Red Book sector of audio: 588 stereo 16-bit samples.
pub const SECTOR_BYTES: u64 = 2352;
pub const SAMPLE_RATE: u32 = 44_100;
const LAME: &str = "lame";

/// The concatenated track files of a dump, read as one continuous byte stream.
pub struct Program {
    parts: Vec<(PathBuf, u64)>,
}

impl Program {
    /// `bins` must already be in track order.
    pub fn open(bins: &[PathBuf]) -> std::io::Result<Self> {
        let parts = bins
            .iter()
            .map(|p| Ok((p.clone(), std::fs::metadata(p)?.len())))
            .collect::<std::io::Result<Vec<_>>>()?;
        Ok(Program { parts })
    }

    pub fn len(&self) -> u64 {
        self.parts.iter().map(|(_, l)| l).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read `len` bytes starting at `start`, across file boundaries as needed.
    pub fn read_range(&self, start: u64, len: u64) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len as usize);
        let mut base = 0u64;
        for (path, size) in &self.parts {
            let end = base + size;
            let want_start = start.max(base);
            let want_end = (start + len).min(end);
            if want_start < want_end {
                let mut f = std::fs::File::open(path)?;
                f.seek(SeekFrom::Start(want_start - base))?;
                let mut chunk = vec![0u8; (want_end - want_start) as usize];
                f.read_exact(&mut chunk)?;
                out.extend(chunk);
            }
            base = end;
        }
        if out.len() as u64 != len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("asked for {len} bytes at {start}, the dump holds {}", self.len()),
            ));
        }
        Ok(out)
    }
}

/// Byte ranges of each track within the program, from the TOC's track starts (0-based
/// LBAs, in order) and the lead-out.
pub fn track_ranges(starts: &[u32], leadout: u32) -> Vec<(u64, u64)> {
    starts
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let end = starts.get(i + 1).copied().unwrap_or(leadout);
            let start = u64::from(s) * SECTOR_BYTES;
            (start, u64::from(end.saturating_sub(s)) * SECTOR_BYTES)
        })
        .collect()
}

/// A canonical 44-byte WAV header for 16-bit stereo CD audio.
pub fn wav_header(data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    let byte_rate = SAMPLE_RATE * 4;
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&2u16.to_le_bytes()); // stereo
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&4u16.to_le_bytes()); // block align
    h[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits per sample
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

pub fn write_wav(path: &Path, pcm: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(&wav_header(pcm.len() as u32))?;
    f.write_all(pcm)?;
    f.sync_all()
}

/// The LAME version string, which is also what goes in the `TSSE` (encoder) frame.
pub fn lame_version() -> Result<String> {
    let out = Command::new(LAME).arg("--version").output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            BackendError::NotInstalled { tool: LAME }
        } else {
            BackendError::Io { tool: LAME, source: e }
        }
    })?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .ok_or(BackendError::Failed {
            tool: LAME,
            detail: "no version output".into(),
        })
}

/// Encode a WAV to an untagged VBR MP3 at `-V 2` (~190 kb/s), LAME's standard
/// transparent preset.
pub fn encode_mp3_v2(wav: &Path, mp3: &Path) -> Result<()> {
    let out = Command::new(LAME)
        .args(["--quiet", "-V", "2", "--nohist"])
        .arg(wav)
        .arg(mp3)
        .output()
        .map_err(|e| BackendError::Io { tool: LAME, source: e })?;
    if !out.status.success() {
        return Err(BackendError::Failed {
            tool: LAME,
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_run_from_their_start_to_the_next_track_or_the_leadout() {
        let r = track_ranges(&[0, 20776, 34101], 52000);
        assert_eq!(r[0], (0, 20776 * SECTOR_BYTES));
        assert_eq!(r[1], (20776 * SECTOR_BYTES, (34101 - 20776) * SECTOR_BYTES));
        assert_eq!(r[2], (34101 * SECTOR_BYTES, (52000 - 34101) * SECTOR_BYTES));
    }

    #[test]
    fn reads_across_track_file_boundaries() {
        let dir = std::env::temp_dir().join(format!("dumo-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        std::fs::write(&a, [1u8, 2, 3, 4]).unwrap();
        std::fs::write(&b, [5u8, 6, 7]).unwrap();
        let p = Program::open(&[a, b]).unwrap();
        assert_eq!(p.len(), 7);
        assert_eq!(p.read_range(2, 4).unwrap(), vec![3, 4, 5, 6]);
        assert!(p.read_range(5, 4).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wav_header_describes_cd_audio() {
        let h = wav_header(2352);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 36 + 2352);
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 44_100);
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 2352);
    }
}
