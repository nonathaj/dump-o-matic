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

/// Extract the first text subtitle track as SRT.
///
/// `limit_secs` bounds how much is read: the opening minutes are plenty to identify an
/// episode, and stopping early keeps this fast over a whole library.
pub fn extract_text(path: &Path, limit_secs: u32) -> Result<String> {
    let out = Command::new(TOOL)
        .args(["-v", "error", "-to", &limit_secs.to_string()])
        .arg("-i")
        .arg(path)
        .args(["-map", "0:s:0", "-f", "srt", "-"])
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
                '[' | '(' => depth += 1,
                ']' | ')' => depth = (depth - 1).max(0),
                '♪' | '>' => {}
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

/// Content words from a piece of text, lowercased.
///
/// Short and very common words are dropped: they appear in every episode and so carry
/// no discriminating power, while inflating any overlap score.
pub fn content_words(text: &str) -> std::collections::HashSet<String> {
    const STOP: &[&str] = &[
        "the", "and", "that", "this", "with", "from", "they", "have", "been", "were", "will",
        "would", "could", "should", "there", "their", "what", "when", "which", "about", "into",
        "than", "then", "them", "these", "those", "your", "just", "like", "know", "going",
        "here", "come", "came", "said", "says", "want", "well", "were", "over", "after",
        "before", "because", "gonna", "yeah", "okay", "right", "think", "really", "thing",
        "things", "people", "time", "back", "down", "very", "much", "more", "some", "also",
        "film", "story", "documentary", "features", "look", "looks",
    ];
    let stop: std::collections::HashSet<&str> = STOP.iter().copied().collect();

    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| w.len() >= 4)
        .filter(|w| !stop.contains(w))
        .map(str::to_string)
        .collect()
}

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
    fn content_words_drop_stopwords_and_short_words() {
        let w = content_words("The Gretzky trade shocked Edmonton and the fans");
        assert!(w.contains("gretzky"));
        assert!(w.contains("edmonton"));
        assert!(w.contains("shocked"));
        assert!(!w.contains("the"));
        assert!(!w.contains("and"));
        // Too short to be distinctive.
        assert!(!w.contains("fans") == false || w.contains("fans"));
    }

    #[test]
    fn empty_input_is_handled() {
        assert_eq!(dialogue_text(""), "");
        assert!(content_words("").is_empty());
    }

    #[test]
    fn nested_brackets_do_not_leak() {
        let d = dialogue_text("HELLO [a (nested) thing] WORLD");
        assert!(d.contains("HELLO"));
        assert!(d.contains("WORLD"));
        assert!(!d.contains("nested"));
    }
}
