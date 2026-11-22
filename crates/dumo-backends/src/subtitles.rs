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

/// Bitmap subtitle codecs that can be read by OCR: Blu-ray PGS, and DVD VobSub for the
/// DVDs whose rips carry no closed-caption text track.
const OCR_SUBTITLE_CODECS: &[&str] = &["hdmv_pgs_subtitle", "dvd_subtitle"];

/// Which subtitle stream to read a title's dialogue from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// Text, read directly. Index among the subtitle streams.
    Text(usize),
    /// Blu-ray PGS pictures, read by OCR. Index among the subtitle streams.
    Pictures(usize),
    /// DVD VobSub pictures, read by OCR. Index among the subtitle streams.
    DvdPictures(usize),
}

/// One subtitle stream, as ffprobe lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SubStream {
    codec: String,
    forced: bool,
    /// How many captions the track holds, from the statistics tags MakeMKV and mkvmerge
    /// write; `None` when the file does not say.
    captions: Option<u64>,
}

/// Each subtitle stream's codec, whether it is flagged as forced, and its caption count.
fn subtitle_streams(path: &Path) -> Vec<SubStream> {
    let Ok(out) = Command::new(PROBE_TOOL)
        .args([
            "-v", "error",
            "-select_streams", "s",
            "-show_entries",
            "stream=codec_name:stream_disposition=forced:stream_tags=NUMBER_OF_FRAMES",
            "-of", "compact",
        ])
        .arg(path)
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_streams(&String::from_utf8_lossy(&out.stdout))
}

/// Parse ffprobe's compact listing: `stream|codec_name=…|disposition:forced=0|tag:…=…`.
fn parse_streams(listing: &str) -> Vec<SubStream> {
    listing
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("stream|"))
        .map(|l| {
            let mut stream = SubStream { codec: String::new(), forced: false, captions: None };
            for field in l.split('|').skip(1) {
                let Some((key, value)) = field.split_once('=') else { continue };
                match key {
                    "codec_name" => stream.codec = value.trim().to_ascii_lowercase(),
                    "disposition:forced" => stream.forced = value.trim() == "1",
                    "tag:NUMBER_OF_FRAMES" => stream.captions = value.trim().parse().ok(),
                    _ => {}
                }
            }
            stream
        })
        .collect()
}

/// A picture track with under this share of the busiest one's captions holds only signs
/// and songs, even when nothing flags it as forced.
const SIGNS_ONLY_SHARE: u64 = 4;

/// Choose the stream to read: the first text stream, and only failing that the first
/// picture stream that is neither a forced track nor a signs-only one.
///
/// Selecting `0:s:0` is wrong on a ripped DVD: MakeMKV emits the original VOBSUB track
/// first and the closed-caption-derived text track second, so mapping the first
/// subtitle stream hands ffmpeg a bitmap format, the conversion fails, and the title
/// silently reports no dialogue at all. Text always wins because it is exact and free;
/// OCR is the costly fallback. A forced track holds only the lines in another language
/// or on a sign, too few to identify anything by. Not every disc flags it: the Neon
/// Genesis Evangelion Blu-rays put an unflagged signs track of 50 captions ahead of
/// dialogue tracks of 600 to 900, so a track far quieter than the busiest is passed
/// over too, when the file records the counts.
fn choose(streams: &[SubStream]) -> Option<Source> {
    if let Some(i) = streams
        .iter()
        .position(|s| TEXT_SUBTITLE_CODECS.contains(&s.codec.as_str()))
    {
        return Some(Source::Text(i));
    }
    let readable = |s: &SubStream| !s.forced && OCR_SUBTITLE_CODECS.contains(&s.codec.as_str());
    let busiest = streams.iter().filter(|s| readable(s)).filter_map(|s| s.captions).max();
    let i = streams.iter().position(|s| {
        readable(s)
            && match (s.captions, busiest) {
                (Some(n), Some(max)) => n * SIGNS_ONLY_SHARE >= max,
                _ => true,
            }
    })?;
    Some(if streams[i].codec == "dvd_subtitle" {
        Source::DvdPictures(i)
    } else {
        Source::Pictures(i)
    })
}

fn run_ffmpeg(path: &Path, limit_secs: u32, stream: usize, output: &[&str]) -> Result<Option<Vec<u8>>> {
    let out = Command::new(TOOL)
        .args(["-nostdin", "-v", "error", "-to", &limit_secs.to_string()])
        .arg("-i")
        .arg(path)
        .args(["-map", &format!("0:s:{stream}")])
        .args(output)
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
    // A stream that will not convert is ordinary, not an error worth aborting a run
    // over: the title simply has no readable dialogue.
    Ok(out.status.success().then_some(out.stdout))
}

/// Extract a title's subtitle text.
///
/// From a text track this is SRT. From a picture track, Blu-ray or DVD, it is the recognised
/// lines, without timings — the words are what matching uses. Picture subtitles need
/// Tesseract; without it this returns [`BackendError::NotInstalled`] so the caller can
/// say why a title has no dialogue, instead of the title quietly having none.
///
/// `limit_secs` bounds how much is read.
pub fn extract_text(path: &Path, limit_secs: u32) -> Result<String> {
    // No subtitle stream at all is ordinary — an extra, usually.
    let Some(source) = choose(&subtitle_streams(path)) else {
        return Ok(String::new());
    };
    match source {
        Source::Text(stream) => Ok(run_ffmpeg(path, limit_secs, stream, &["-f", "srt", "-"])?
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default()),
        Source::Pictures(stream) => {
            if !crate::ocr::available() {
                return Err(BackendError::NotInstalled { tool: "tesseract" });
            }
            let Some(sup) = run_ffmpeg(path, limit_secs, stream, &["-c", "copy", "-f", "sup", "-"])?
            else {
                return Ok(String::new());
            };
            crate::ocr::read_captions(&crate::pgs::decode(&sup))
        }
        // The whole track is read: a DVD's captions are a couple of megabytes, and the
        // windows asked for are longer than any title anyway.
        Source::DvdPictures(stream) => {
            if !crate::ocr::available() {
                return Err(BackendError::NotInstalled { tool: "tesseract" });
            }
            crate::ocr::read_captions(&crate::vobsub::read(path, stream)?)
        }
    }
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
pub use dumo_core::text::{count_words, tokenize};

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

    fn streams(s: &[(&str, bool)]) -> Vec<SubStream> {
        s.iter()
            .map(|(c, f)| SubStream { codec: c.to_string(), forced: *f, captions: None })
            .collect()
    }

    fn counted(s: &[(&str, u64)]) -> Vec<SubStream> {
        s.iter()
            .map(|(c, n)| SubStream { codec: c.to_string(), forced: false, captions: Some(*n) })
            .collect()
    }

    #[test]
    fn text_is_read_before_pictures_whatever_the_order() {
        // A ripped DVD: VOBSUB first, closed captions second.
        let dvd = streams(&[("dvd_subtitle", false), ("subrip", false)]);
        assert_eq!(choose(&dvd), Some(Source::Text(1)));
        let bd = streams(&[("hdmv_pgs_subtitle", false), ("hdmv_pgs_subtitle", false)]);
        assert_eq!(choose(&bd), Some(Source::Pictures(0)));
    }

    #[test]
    fn a_forced_picture_track_is_passed_over() {
        let bd = streams(&[("hdmv_pgs_subtitle", true), ("hdmv_pgs_subtitle", false)]);
        assert_eq!(choose(&bd), Some(Source::Pictures(1)));
        assert_eq!(choose(&streams(&[("hdmv_pgs_subtitle", true)])), None);
    }

    #[test]
    fn an_unflagged_signs_track_is_passed_over_for_the_dialogue() {
        // Neon Genesis Evangelion on Blu-ray: signs first, three dialogue tracks after.
        let pgs = "hdmv_pgs_subtitle";
        let bd = counted(&[(pgs, 50), (pgs, 651), (pgs, 610), (pgs, 945)]);
        assert_eq!(choose(&bd), Some(Source::Pictures(1)));
        // Tracks of like size keep the first, and a lone quiet track is still read.
        assert_eq!(choose(&counted(&[(pgs, 400), (pgs, 650)])), Some(Source::Pictures(0)));
        assert_eq!(choose(&counted(&[(pgs, 50)])), Some(Source::Pictures(0)));
    }

    #[test]
    fn a_dvd_with_only_pictures_is_read_by_ocr() {
        // Marvel's The Punisher on DVD: VobSub and no closed captions.
        assert_eq!(choose(&streams(&[("dvd_subtitle", false)])), Some(Source::DvdPictures(0)));
    }

    #[test]
    fn unreadable_picture_formats_are_not_chosen() {
        assert_eq!(choose(&streams(&[("dvb_subtitle", false)])), None);
        assert_eq!(choose(&[]), None);
    }

    #[test]
    fn parses_ffprobe_stream_listing() {
        let s = parse_streams(
            "stream|codec_name=hdmv_pgs_subtitle|disposition:forced=0|tag:NUMBER_OF_FRAMES=50\n\
             stream|codec_name=hdmv_pgs_subtitle|disposition:forced=1\n\
             stream|codec_name=subrip|disposition:forced=0\n\n",
        );
        let mut want =
            streams(&[("hdmv_pgs_subtitle", false), ("hdmv_pgs_subtitle", true), ("subrip", false)]);
        want[0].captions = Some(50);
        assert_eq!(s, want);
    }

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
