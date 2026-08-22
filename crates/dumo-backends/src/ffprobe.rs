//! ffprobe adapter: read technical features out of a media file.
//!
//! This is the raw material for video identification. A ripped title carries no useful
//! filename, but it does carry duration, chapter layout, and track structure — and those
//! are enough to work out a disc's *shape* (feature film, episode set, extras) before any
//! online lookup happens.

use crate::{BackendError, Result};
use std::path::Path;
use std::process::Command;

const TOOL: &str = "ffprobe";

/// A chapter marker.
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub start_secs: f64,
    pub end_secs: f64,
    pub title: Option<String>,
}

impl Chapter {
    pub fn duration_secs(&self) -> f64 {
        (self.end_secs - self.start_secs).max(0.0)
    }
}

/// One stream within the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    pub kind: String,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Technical features of a media file.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaFeatures {
    pub duration_secs: f64,
    pub size_bytes: u64,
    /// Container-level title tag, if the ripper wrote one.
    pub title: Option<String>,
    pub chapters: Vec<Chapter>,
    pub streams: Vec<Stream>,
}

impl MediaFeatures {
    pub fn chapter_count(&self) -> usize {
        self.chapters.len()
    }

    pub fn video_streams(&self) -> usize {
        self.streams.iter().filter(|s| s.kind == "video").count()
    }

    pub fn audio_streams(&self) -> usize {
        self.streams.iter().filter(|s| s.kind == "audio").count()
    }

    pub fn subtitle_streams(&self) -> usize {
        self.streams.iter().filter(|s| s.kind == "subtitle").count()
    }

    pub fn resolution(&self) -> Option<(u32, u32)> {
        self.streams
            .iter()
            .find(|s| s.kind == "video")
            .and_then(|s| Some((s.width?, s.height?)))
    }

    pub fn duration_mins(&self) -> f64 {
        self.duration_secs / 60.0
    }
}

/// Run ffprobe over a file and parse its JSON output.
pub fn probe(path: &Path) -> Result<MediaFeatures> {
    let out = Command::new(TOOL)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-show_chapters",
        ])
        .arg(path)
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

    if !out.status.success() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }

    parse_json(&String::from_utf8_lossy(&out.stdout)).ok_or(BackendError::Failed {
        tool: TOOL,
        detail: "could not parse ffprobe output".into(),
    })
}

/// Parse ffprobe's JSON. Split out so it can be tested against captured output.
pub fn parse_json(text: &str) -> Option<MediaFeatures> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;

    let format = v.get("format");
    let duration_secs = format
        .and_then(|f| f.get("duration"))
        .and_then(|d| d.as_str())
        .and_then(|d| d.parse().ok())
        .unwrap_or(0.0);
    let size_bytes = format
        .and_then(|f| f.get("size"))
        .and_then(|d| d.as_str())
        .and_then(|d| d.parse().ok())
        .unwrap_or(0);
    let title = format
        .and_then(|f| f.get("tags"))
        .and_then(|t| t.get("title"))
        .and_then(|t| t.as_str())
        .map(str::to_string);

    let tag = |o: &serde_json::Value, k: &str| -> Option<String> {
        o.get("tags")?.get(k)?.as_str().map(str::to_string)
    };

    let chapters = v
        .get("chapters")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .map(|c| Chapter {
                    start_secs: c
                        .get("start_time")
                        .and_then(|s| s.as_str())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0),
                    end_secs: c
                        .get("end_time")
                        .and_then(|s| s.as_str())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0),
                    title: tag(c, "title"),
                })
                .collect()
        })
        .unwrap_or_default();

    let streams = v
        .get("streams")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .map(|s| Stream {
                    kind: s
                        .get("codec_type")
                        .and_then(|k| k.as_str())
                        .unwrap_or("")
                        .to_string(),
                    codec: s
                        .get("codec_name")
                        .and_then(|k| k.as_str())
                        .unwrap_or("")
                        .to_string(),
                    language: tag(s, "language"),
                    title: tag(s, "title"),
                    width: s.get("width").and_then(|w| w.as_u64()).map(|w| w as u32),
                    height: s.get("height").and_then(|h| h.as_u64()).map(|h| h as u32),
                })
                .collect()
        })
        .unwrap_or_default();

    Some(MediaFeatures {
        duration_secs,
        size_bytes,
        title,
        chapters,
        streams,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from real ffprobe output for a staged DVD rip.
    const SAMPLE: &str = r#"{
        "streams": [
            {"index":0,"codec_name":"mpeg2video","codec_type":"video","width":720,"height":480,
             "tags":{"language":"eng"}},
            {"index":1,"codec_name":"ac3","codec_type":"audio","tags":{"language":"eng","title":"Stereo"}},
            {"index":2,"codec_name":"subrip","codec_type":"subtitle","tags":{"language":"eng"}}
        ],
        "chapters": [
            {"start_time":"0.000000","end_time":"600.000000","tags":{"title":"Chapter 01"}},
            {"start_time":"600.000000","end_time":"1500.000000"},
            {"start_time":"1500.000000","end_time":"2400.000000"},
            {"start_time":"2400.000000","end_time":"3070.100000"}
        ],
        "format": {"duration":"3070.100000","size":"1993913930"}
    }"#;

    #[test]
    fn parses_real_ffprobe_output() {
        let f = parse_json(SAMPLE).expect("parsed");
        assert!((f.duration_secs - 3070.1).abs() < 0.01);
        assert_eq!(f.size_bytes, 1_993_913_930);
        assert_eq!(f.chapter_count(), 4);
        assert_eq!(f.video_streams(), 1);
        assert_eq!(f.audio_streams(), 1);
        assert_eq!(f.subtitle_streams(), 1);
        assert_eq!(f.resolution(), Some((720, 480)));
    }

    #[test]
    fn reads_chapter_times_and_titles() {
        let f = parse_json(SAMPLE).unwrap();
        assert_eq!(f.chapters[0].title.as_deref(), Some("Chapter 01"));
        assert_eq!(f.chapters[1].title, None);
        assert!((f.chapters[0].duration_secs() - 600.0).abs() < 0.01);
    }

    #[test]
    fn reads_stream_language_and_title() {
        let f = parse_json(SAMPLE).unwrap();
        let audio = f.streams.iter().find(|s| s.kind == "audio").unwrap();
        assert_eq!(audio.language.as_deref(), Some("eng"));
        assert_eq!(audio.title.as_deref(), Some("Stereo"));
    }

    #[test]
    fn missing_fields_do_not_panic() {
        let f = parse_json(r#"{"format":{}}"#).unwrap();
        assert_eq!(f.duration_secs, 0.0);
        assert_eq!(f.chapter_count(), 0);
        assert_eq!(f.resolution(), None);
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(parse_json("not json").is_none());
    }
}
