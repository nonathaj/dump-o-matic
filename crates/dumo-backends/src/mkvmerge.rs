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
