//! Cutting a title at its chapter marks, with mkvmerge.
//!
//! Some extras are compilations: Frieren's season 1 part 2 disc 1 holds eleven mini-anime
//! shorts as one title, a chapter each, and each short is a special of its own. Cutting
//! at the chapters gives each its own file without re-encoding anything. A Blu-ray's
//! chapter marks sit on keyframes, so the cuts are exact.

use crate::{BackendError, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const TOOL: &str = "mkvmerge";

/// mkvmerge's version line, for the job's record of what produced its files.
pub fn version() -> Result<String> {
    let out = Command::new(TOOL).arg("--version").output().map_err(not_installed)?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or(TOOL)
        .trim()
        .to_string())
}

fn not_installed(e: std::io::Error) -> BackendError {
    if e.kind() == std::io::ErrorKind::NotFound {
        BackendError::NotInstalled { tool: TOOL }
    } else {
        BackendError::Io { tool: TOOL, source: e }
    }
}

/// Cut `src` at every chapter mark into `out_dir/<stem>-001.mkv`, `-002`, and so on,
/// and return the pieces in order.
///
/// `out_dir` must hold no pieces of that name already, so that what is returned is
/// exactly what this run wrote. The source is only read.
pub fn split_chapters(src: &Path, out_dir: &Path, stem: &str) -> Result<Vec<PathBuf>> {
    if !pieces(out_dir, stem).is_empty() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: format!("{} already holds pieces named {stem}-NNN.mkv", out_dir.display()),
        });
    }
    let out = Command::new(TOOL)
        .arg("--output")
        .arg(out_dir.join(format!("{stem}.mkv")))
        .args(["--split", "chapters:all"])
        .arg(src)
        .output()
        .map_err(not_installed)?;
    // 0 is success and 1 success with warnings; 2 is an error.
    if out.status.code().map(|c| c > 1).unwrap_or(true) {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: String::from_utf8_lossy(&out.stdout)
                .lines()
                .chain(String::from_utf8_lossy(&out.stderr).lines())
                .filter(|l| l.contains("Error"))
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    Ok(pieces(out_dir, stem))
}

/// What kind of stream a track carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitles,
    Other,
}

/// One track of a Matroska file, as mkvmerge numbers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: u32,
    pub kind: TrackKind,
    /// ISO 639-2 code, e.g. `eng`; `und` or empty when the file does not say.
    pub language: String,
    pub codec: String,
}

/// The tracks of a Matroska file.
pub fn tracks(path: &Path) -> Result<Vec<Track>> {
    let out = Command::new(TOOL)
        .arg("-J")
        .arg(path)
        .output()
        .map_err(not_installed)?;
    if !out.status.success() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: format!("could not read the tracks of {}", path.display()),
        });
    }
    parse_tracks(&String::from_utf8_lossy(&out.stdout))
}

fn parse_tracks(json: &str) -> Result<Vec<Track>> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| BackendError::Failed {
        tool: TOOL,
        detail: format!("unreadable track list: {e}"),
    })?;
    Ok(v["tracks"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|t| {
            Some(Track {
                id: t["id"].as_u64()? as u32,
                kind: match t["type"].as_str()? {
                    "video" => TrackKind::Video,
                    "audio" => TrackKind::Audio,
                    "subtitles" => TrackKind::Subtitles,
                    _ => TrackKind::Other,
                },
                language: t["properties"]["language"].as_str().unwrap_or("").to_string(),
                codec: t["codec"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect())
}

/// Copy `src` to `dest` keeping only the given audio and subtitle tracks, without
/// re-encoding. Video and everything else is kept. `dest` must not exist.
pub fn remux_keeping(src: &Path, dest: &Path, audio: &[u32], subtitles: &[u32]) -> Result<()> {
    if dest.exists() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: format!("{} already exists", dest.display()),
        });
    }
    let ids = |v: &[u32]| v.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let mut cmd = Command::new(TOOL);
    cmd.arg("--output").arg(dest);
    // An empty list must be said as "none": `--audio-tracks ""` is not accepted.
    if audio.is_empty() {
        cmd.arg("--no-audio");
    } else {
        cmd.args(["--audio-tracks", &ids(audio)]);
    }
    if subtitles.is_empty() {
        cmd.arg("--no-subtitles");
    } else {
        cmd.args(["--subtitle-tracks", &ids(subtitles)]);
    }
    let out = cmd.arg(src).output().map_err(not_installed)?;
    if out.status.code().map(|c| c > 1).unwrap_or(true) {
        let _ = std::fs::remove_file(dest);
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| l.contains("Error"))
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    Ok(())
}

/// The `<stem>-NNN.mkv` files in `dir`, in number order.
fn pieces(dir: &Path, stem: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(u32, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let n = name
                .strip_prefix(stem)?
                .strip_prefix('-')?
                .strip_suffix(".mkv")?;
            (n.len() == 3 && n.chars().all(|c| c.is_ascii_digit()))
                .then(|| (n.parse().ok().map(|n: u32| (n, e.path()))))
                .flatten()
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("dumo-mkvmerge-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn reads_track_kinds_and_languages() {
        // Trimmed from `mkvmerge -J` on a Frieren Blu-ray episode.
        let json = r#"{"tracks":[
            {"id":0,"type":"video","codec":"AVC/H.264/MPEG-4p10","properties":{"language":"eng"}},
            {"id":1,"type":"audio","codec":"TrueHD","properties":{"language":"eng"}},
            {"id":2,"type":"audio","codec":"TrueHD","properties":{"language":"jpn"}},
            {"id":3,"type":"subtitles","codec":"HDMV PGS","properties":{}}
        ]}"#;
        let t = parse_tracks(json).unwrap();
        assert_eq!(t.len(), 4);
        assert_eq!((t[2].kind, t[2].language.as_str()), (TrackKind::Audio, "jpn"));
        assert_eq!((t[3].kind, t[3].language.as_str()), (TrackKind::Subtitles, ""));
    }

    #[test]
    fn pieces_are_found_in_number_order_and_nothing_else() {
        let dir = scratch("pieces");
        for name in ["t07-010.mkv", "t07-002.mkv", "t07-001.mkv", "t07.mkv", "t07-01.mkv", "t08-001.mkv"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let names: Vec<String> = pieces(&dir, "t07")
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["t07-001.mkv", "t07-002.mkv", "t07-010.mkv"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_a_directory_already_holding_pieces() {
        let dir = scratch("refuse");
        std::fs::write(dir.join("t07-001.mkv"), b"").unwrap();
        assert!(split_chapters(&dir.join("src.mkv"), &dir, "t07").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Runs only where ffmpeg and mkvmerge are installed, like the chdman round trips.
    #[test]
    fn cuts_a_title_at_each_chapter() {
        let have = |t: &str| Command::new(t).arg("-version").output().is_ok()
            || Command::new(t).arg("--version").output().is_ok();
        if !have("ffmpeg") || !have(TOOL) {
            return;
        }
        let dir = scratch("cut");
        let meta = dir.join("chapters.txt");
        std::fs::write(
            &meta,
            ";FFMETADATA1\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=2000\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=2000\nEND=4000\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=4000\nEND=6000\n",
        )
        .unwrap();
        let src = dir.join("src.mkv");
        let made = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=duration=6:size=64x48:rate=10"])
            .arg("-i")
            .arg(&meta)
            .args(["-map_metadata", "1", "-g", "10", "-c:v", "mpeg4"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let got = split_chapters(&src, &out, "src").unwrap();
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got.iter().all(|p| p.metadata().unwrap().len() > 0));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
