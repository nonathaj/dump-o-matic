//! The words on screen in a title's opening minutes, and a contact sheet of them.
//!
//! Some series put the episode's name on screen as a drawn title card and caption none
//! of it: Danny Phantom's DVDs carry no subtitle track at all, and each episode opens
//! with a poster reading "Danny Phantom starring in ... Parental Bonding". With no
//! dialogue to compare, the card is the only thing on the disc that names the episode,
//! and the discs do not hold the episodes in broadcast order.
//!
//! The card is read by OCR from one frame a second. Poster lettering defeats it as
//! often as not, so a contact sheet of the same minutes is also written, for reading
//! the cards by eye when the OCR could not.

use crate::{BackendError, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const TOOL: &str = "ffmpeg";

/// How far into a title its card is looked for, in seconds. Danny Phantom's come after
/// a cold open and the theme, between two and four minutes in.
pub const WINDOW_SECS: u32 = 8 * 60;

/// Seconds between the frames read. A card stays up for four seconds or more.
const EVERY_SECS: u32 = 1;

/// Seconds between the frames of a contact sheet: enough to catch a card, few enough
/// to read at a glance.
const SHEET_EVERY_SECS: u32 = 10;

/// The text read from each of a title's opening frames, one second apart, in order.
///
/// Each frame is read twice: in grey, and with only its brightest parts kept, as black
/// on white. Poster lettering is light paint over a busy drawing; the second reading
/// drops the drawing. Measured on ten of Danny Phantom's cards, each reading found
/// names the other missed, and together they found half again as many.
pub fn opening_text(path: &Path) -> Result<Vec<String>> {
    let dir = scratch(path)?;
    let frames = (|| {
        // Doubled: Tesseract reads small lettering better large.
        let filters = format!(
            "[0:v]fps=1/{EVERY_SECS},scale=iw*2:ih*2,format=gray,split[grey][b];\
             [b]lut=y='if(gt(val,{BRIGHT}),0,255)'[bright]"
        );
        run(Command::new(TOOL)
            .args(["-v", "error", "-nostdin", "-t", &WINDOW_SECS.to_string(), "-i"])
            .arg(path)
            .args(["-filter_complex", &filters, "-map", "[grey]"])
            .arg(dir.join("g%04d.png"))
            .args(["-map", "[bright]"])
            .arg(dir.join("b%04d.png")))?;
        let mut grey: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| BackendError::Io { tool: TOOL, source: e })?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with('g')).unwrap_or(false))
            .collect();
        grey.sort();
        let bright: Vec<PathBuf> = grey
            .iter()
            .map(|g| g.with_file_name(g.file_name().unwrap_or_default().to_string_lossy().replacen('g', "b", 1)))
            .collect();
        let both: Vec<PathBuf> = grey.iter().chain(&bright).cloned().collect();
        let mut read = crate::ocr::read_pictures(&both)?;
        let bright_read = read.split_off(grey.len());
        Ok(read
            .into_iter()
            .zip(bright_read)
            .map(|(g, b)| format!("{g}\n{b}"))
            .collect())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    frames
}

/// The grey level above which a frame's paint is kept as lettering in the second
/// reading.
const BRIGHT: u8 = 150;

/// A contact sheet of the title's opening minutes, a frame every ten seconds, each
/// stamped with its time, written to `out` as PNG.
pub fn contact_sheet(path: &Path, out: &Path) -> Result<()> {
    let frames = WINDOW_SECS / SHEET_EVERY_SECS;
    let columns = 8;
    let rows = frames.div_ceil(columns);
    let tile = format!("fps=1/{SHEET_EVERY_SECS},scale=240:-2");
    let grid = format!("tile={columns}x{rows}");
    let stamp = "drawtext=text='%{pts\\:hms}':x=4:y=4:fontsize=14:fontcolor=yellow:box=1:boxcolor=black";
    let sheet = |filters: &str| {
        run(Command::new(TOOL)
            .args(["-v", "error", "-nostdin", "-y", "-t", &WINDOW_SECS.to_string(), "-i"])
            .arg(path)
            .args(["-vf", filters, "-frames:v", "1"])
            .arg(out))
    };
    // The time stamps need ffmpeg built with a font library; without one, a sheet
    // without them still shows the cards.
    sheet(&format!("{tile},{stamp},{grid}")).or_else(|_| sheet(&format!("{tile},{grid}")))
}

/// A fresh, empty directory for one title's frames.
fn scratch(path: &Path) -> Result<PathBuf> {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().replace(|c: char| !c.is_ascii_alphanumeric(), "_"))
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("dumo-frames-{}-{stem}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| BackendError::Io { tool: TOOL, source: e })?;
    Ok(dir)
}

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            BackendError::NotInstalled { tool: TOOL }
        } else {
            BackendError::Io { tool: TOOL, source: e }
        }
    })?;
    if !out.status.success() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only where ffmpeg and Tesseract are installed, like the chdman round trips.
    #[test]
    fn reads_a_card_drawn_into_the_opening_and_makes_a_sheet() {
        let have = |t: &str, a: &str| Command::new(t).arg(a).output().is_ok();
        if !have(TOOL, "-version") || !crate::ocr::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("dumo-frames-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("title.mkv");
        // Three seconds of black, then three of a white card with black lettering.
        let made = Command::new(TOOL)
            .args(["-v", "error", "-f", "lavfi", "-i", "color=black:s=320x240:d=3", "-f", "lavfi"])
            .args(["-i", "color=white:s=320x240:d=3", "-filter_complex"])
            .arg(
                "[1]drawtext=text='PARENTAL BONDING':x=20:y=100:fontsize=28:fontcolor=black[c];\
                 [0][c]concat=n=2:v=1",
            )
            .args(["-c:v", "mpeg4"])
            .arg(&src)
            .status()
            .unwrap();
        if !made.success() {
            // An ffmpeg without a font library cannot draw the card.
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }
        let text = opening_text(&src).unwrap();
        assert_eq!(text.len(), 6, "{text:?}");
        assert!(text[..3].iter().all(|t| t.trim().is_empty()), "{text:?}");
        assert!(text[3..].iter().any(|t| t.contains("BONDING")), "{text:?}");

        let sheet = dir.join("sheet.png");
        contact_sheet(&src, &sheet).unwrap();
        assert!(sheet.metadata().unwrap().len() > 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
