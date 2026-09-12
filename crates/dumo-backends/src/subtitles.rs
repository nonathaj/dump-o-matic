//! Extract subtitle text from a ripped title.
//!
//! Ripped DVDs carry more identifying information than their runtime. MakeMKV converts
//! the CEA-608 closed captions embedded in the MPEG-2 stream into a text subtitle
//! track, so the actual dialogue is available without OCR — and dialogue is far more
//! discriminating than a duration. Measured across a real box set, every main title had
//! a text track and every extra had none, which incidentally makes subtitle presence a
//! useful content-versus-extra signal in its own right.

use crate::{BackendError, Result};
use std::path::Path;
use std::process::Command;

const TOOL: &str = "ffmpeg";
const PROBE_TOOL: &str = "ffprobe";

/// Subtitle codecs that hold actual text.
///
/// The rest are bitmap formats — `dvd_subtitle` (VOBSUB), Blu-ray PGS, DVB — which
/// carry pictures of words and cannot become SRT without OCR.
const TEXT_SUBTITLE_CODECS: &[&str] = &["subrip", "srt", "ass", "ssa", "mov_text", "webvtt", "text"];

/// Index, among the subtitle streams, of the first one holding text.
///
/// Selecting `0:s:0` is wrong on a ripped DVD: MakeMKV emits the original VOBSUB track
/// first and the closed-caption-derived text track second, so mapping the first
/// subtitle stream hands ffmpeg a bitmap format, the conversion fails, and the title
/// silently reports no dialogue at all. Ask what the streams actually are instead.
fn first_text_subtitle_index(path: &Path) -> Option<usize> {
    let out = Command::new(PROBE_TOOL)
        .args([
            "-v", "error",
            "-select_streams", "s",
            "-show_entries", "stream=codec_name",
            "-of", "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().trim_end_matches(',').to_ascii_lowercase())
        .filter(|l| !l.is_empty())
        .position(|c| TEXT_SUBTITLE_CODECS.contains(&c.as_str()))
}

/// Extract the first text subtitle track as SRT.
///
/// `limit_secs` bounds how much is read: the opening minutes are plenty to identify an
/// episode, and stopping early keeps this fast over a whole library.
pub fn extract_text(path: &Path, limit_secs: u32) -> Result<String> {
    // No text track at all is ordinary — an extra, or a disc with only bitmap subs.
    let Some(stream) = first_text_subtitle_index(path) else {
        return Ok(String::new());
    };
    let out = Command::new(TOOL)
        .args(["-v", "error", "-to", &limit_secs.to_string()])
        .arg("-i")
        .arg(path)
        .args(["-map", &format!("0:s:{stream}"), "-f", "srt", "-"])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                BackendError::NotInstalled { tool: TOOL }
            } else {
                BackendError::Io {
                    tool: TOOL,
                    source: e,
                }
            }
        })?;

    // A title with no subtitle track fails here; that is ordinary, not an error worth
    // aborting a run over.
    if !out.status.success() {
        return Ok(String::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Strip SRT markup down to spoken words.
///
/// Removes sequence numbers, timestamps, speaker markers (`>>`), sound descriptions in
/// brackets, and music glyphs — none of which say anything about *which* episode this is.
pub fn dialogue_text(srt: &str) -> String {
    let mut out = String::new();
    for line in srt.lines() {
        let l = line.trim();
        if l.is_empty() || l.chars().all(|c| c.is_ascii_digit()) || l.contains("-->") {
            continue;
        }
        let mut cleaned = String::new();
        let mut depth = 0i32;
        for c in l.chars() {
            match c {
                // Angle brackets are markup (`<i>`); the others are sound descriptions.
                // Closing a pair that was never opened is harmless, which keeps the
                // speaker marker `>>` being dropped rather than kept as a token.
                '[' | '(' | '<' => depth += 1,
                ']' | ')' | '>' => depth = (depth - 1).max(0),
                '♪' => {}
                _ if depth == 0 => cleaned.push(c),
                _ => {}
            }
        }
        let cleaned = cleaned.trim();
        if !cleaned.is_empty() {
            out.push_str(cleaned);
            out.push(' ');
        }
    }
    out
}

/// Split text into lowercase word tokens.
///
/// Re-exported from [`dumo_core::text`] so ripped dialogue and reference synopses are
/// tokenised by the same code; see there for why there is no stopword list.
pub use dumo_core::text::tokenize;

#[cfg(test)]
mod tests {
    use super::*;

    /// Real SRT output from a staged rip.
    const SAMPLE: &str = r#"1
00:00:21,687 --> 00:00:29,861
[lively percussive music]
♪ ♪

2
00:00:31,297 --> 00:00:35,800
>> JORDAN DELIVERS A TWO-RBI...
[people clamoring]

3
00:01:51,377 --> 00:01:55,247
>> WHEN YOU'RE 27, YOU THINK
YOU KNOW EVERYTHING.
"#;

    #[test]
    fn strips_srt_markup_to_dialogue() {
        let d = dialogue_text(SAMPLE);
        assert!(d.contains("JORDAN DELIVERS"));
        // Sound descriptions and music glyphs say nothing about which episode this is.
        assert!(!d.contains("percussive"));
        assert!(!d.contains("clamoring"));
        assert!(!d.contains('♪'));
        // Timestamps and sequence numbers are gone.
        assert!(!d.contains("00:00"));
        assert!(!d.contains("-->"));
    }

    #[test]
    fn tokenize_keeps_words_and_drops_fragments() {
        let w = tokenize("The Gretzky trade shocked Edmonton -- a 1988 deal!");
        assert!(w.contains("gretzky"));
        assert!(w.contains("edmonton"));
        assert!(w.contains("1988"));
        // Common words are kept here on purpose: weighting them down is the corpus's
        // job, not a hardcoded list's.
        assert!(w.contains("the"));
        // Punctuation does not survive as a token.
        assert!(!w.contains("--"));
        assert!(!w.iter().any(|t| t.is_empty()));
    }

    #[test]
    fn empty_input_is_handled() {
        assert_eq!(dialogue_text(""), "");
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn italic_markup_does_not_become_dialogue() {
        // Real SRT from a ripped DVD wraps narration in <i> tags.
        let d = dialogue_text("<i> Good afternoon,</i>\n>> AND WELCOME.");
        assert!(d.contains("Good afternoon"));
        assert!(d.contains("AND WELCOME"));
        // The tag must not survive as a stray "i" token.
        assert!(!tokenize(&d).contains("i"), "{:?}", tokenize(&d));
    }

    #[test]
    fn nested_brackets_do_not_leak() {
        let d = dialogue_text("HELLO [a (nested) thing] WORLD");
        assert!(d.contains("HELLO"));
        assert!(d.contains("WORLD"));
        assert!(!d.contains("nested"));
    }
}
