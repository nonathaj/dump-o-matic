//! Reconstruct a Redump-canonical `.cue` sheet.
//!
//! A cue sheet is the one artifact of a CD dump that is *not* a copy of anything on the
//! disc: it is generated text that names the track files it points at. redumper names
//! them after the image (`SLUS-20247.bin`) while Redump names them after the game
//! (`Tetris Worlds (USA).bin`), and Redump writes CRLF line endings. So a
//! byte-perfect CD dump still has a cue that matches no datfile entry — which would
//! leave every CD title permanently "incomplete" and unfilable.
//!
//! The fix is to rewrite the `FILE` references and line endings when applying the
//! archival name. Nothing here is taken on trust: the caller hashes the result and
//! compares it against the datfile, so a cue this module gets wrong fails the set-
//! completeness check instead of being written out as if it were correct.

/// Line ending Redump's cue sheets use.
const CRLF: &[u8] = b"\r\n";

/// Rewrite `FILE "..."` references according to `renames`, and normalise to CRLF.
///
/// `renames` maps current file name to archival file name. References not listed are
/// left alone — a cue naming a file we did not rename is a cue we do not understand,
/// and quietly "fixing" it would be worse than leaving it to fail verification.
pub fn retarget(cue: &[u8], renames: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(cue.len() + cue.len() / 8);
    for line in split_lines(cue) {
        out.extend_from_slice(&rewrite_line(line, renames));
        out.extend_from_slice(CRLF);
    }
    out
}

/// Split on LF, dropping a trailing CR so CRLF and LF input behave identically.
///
/// A trailing newline does not produce a final empty line; cue sheets end with one and
/// re-emitting it would add a stray blank line on every pass.
fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = data
        .split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .collect();
    if lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

/// Replace the quoted name in a `FILE` line, preserving everything around it.
fn rewrite_line(line: &[u8], renames: &[(String, String)]) -> Vec<u8> {
    let trimmed = line
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map(|i| &line[i..])
        .unwrap_or(line);
    if !trimmed.starts_with(b"FILE ") {
        return line.to_vec();
    }
    // The name is between the first and last quote: file names may contain quotes only
    // in pathological cases, but they routinely contain spaces, so splitting on
    // whitespace would be wrong.
    let (Some(open), Some(close)) = (
        line.iter().position(|&b| b == b'"'),
        line.iter().rposition(|&b| b == b'"'),
    ) else {
        return line.to_vec();
    };
    if close <= open {
        return line.to_vec();
    }
    let current = &line[open + 1..close];
    let Some((_, new)) = renames
        .iter()
        .find(|(old, _)| old.as_bytes() == current)
    else {
        return line.to_vec();
    };

    let mut out = Vec::with_capacity(line.len() + new.len());
    out.extend_from_slice(&line[..=open]);
    out.extend_from_slice(new.as_bytes());
    out.extend_from_slice(&line[close..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renames(old: &str, new: &str) -> Vec<(String, String)> {
        vec![(old.to_string(), new.to_string())]
    }

    /// The real case, end to end.
    ///
    /// This cue came out of redumper for a PS2 CD, and the expected SHA-1 and size are
    /// Redump's own datfile entry for the same disc. The `.bin` already matched
    /// Redump's hashes exactly; only the cue differed, and only in these two respects.
    #[test]
    fn reproduces_the_redump_cue_byte_for_byte() {
        let src = b"FILE \"SLUS-20247.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n";
        let out = retarget(src, &renames("SLUS-20247.bin", "Tetris Worlds (USA).bin"));

        assert_eq!(out.len(), 85, "Redump lists this cue as 85 bytes");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "FILE \"Tetris Worlds (USA).bin\" BINARY\r\n  \
             TRACK 01 MODE2/2352\r\n    INDEX 01 00:00:00\r\n"
        );
    }

    #[test]
    fn already_crlf_input_is_unchanged_in_length() {
        let src = b"FILE \"a.bin\" BINARY\r\n  TRACK 01 MODE2/2352\r\n";
        let out = retarget(src, &renames("a.bin", "a.bin"));
        assert_eq!(out, src);
    }

    /// Rewriting must be idempotent: applying it twice must not double the endings or
    /// re-rewrite a name that is already archival.
    #[test]
    fn is_idempotent() {
        let src = b"FILE \"old.bin\" BINARY\n  TRACK 01 MODE2/2352\n";
        let r = renames("old.bin", "New Name (USA).bin");
        let once = retarget(src, &r);
        let twice = retarget(&once, &r);
        assert_eq!(once, twice);
    }

    #[test]
    fn multi_track_cues_rewrite_every_reference() {
        let src = b"FILE \"g (Track 1).bin\" BINARY\n  TRACK 01 MODE1/2352\n\
                    FILE \"g (Track 2).bin\" BINARY\n  TRACK 02 AUDIO\n";
        let r = vec![
            ("g (Track 1).bin".into(), "Game (USA) (Track 1).bin".into()),
            ("g (Track 2).bin".into(), "Game (USA) (Track 2).bin".into()),
        ];
        let out = String::from_utf8(retarget(src, &r)).unwrap();
        assert!(out.contains("\"Game (USA) (Track 1).bin\""));
        assert!(out.contains("\"Game (USA) (Track 2).bin\""));
        assert!(!out.contains("\"g ("));
    }

    /// Names containing spaces are the norm, so the parse must not split on whitespace.
    #[test]
    fn handles_spaces_and_punctuation_in_names() {
        let src = b"FILE \"Tomb Raider - The Last Revelation (USA).bin\" BINARY\n";
        let out = retarget(src, &renames("Tomb Raider - The Last Revelation (USA).bin", "X.bin"));
        assert_eq!(out, b"FILE \"X.bin\" BINARY\r\n");
    }

    /// An unrecognised reference is left alone rather than guessed at; the caller's hash
    /// check then refuses the set instead of filing something we invented.
    #[test]
    fn unknown_references_are_left_untouched() {
        let src = b"FILE \"mystery.bin\" BINARY\n";
        let out = retarget(src, &renames("other.bin", "new.bin"));
        assert_eq!(out, b"FILE \"mystery.bin\" BINARY\r\n");
    }

    #[test]
    fn non_file_lines_are_preserved_verbatim() {
        let src = b"REM COMMENT \"quoted\"\n    INDEX 01 00:00:00\n";
        let out = retarget(src, &renames("quoted", "changed"));
        assert_eq!(out, b"REM COMMENT \"quoted\"\r\n    INDEX 01 00:00:00\r\n");
    }

    #[test]
    fn empty_input_produces_empty_output() {
        assert!(retarget(b"", &[]).is_empty());
    }
}
