//! Which audio and subtitle tracks a filed video keeps.
//!
//! MakeMKV rips every track, so a show that wants its original audio can have it; filing
//! keeps the configured languages and drops the rest (see `VideoConfig`). Two rules keep
//! that from losing something wanted:
//!
//! - a track that does not say its language is kept: dropping it would be a guess;
//! - a title with no track of a kind in any wanted language keeps all of that kind.
//!   Frieren's mini-anime shorts have only Japanese audio; with English-only selection
//!   they came out of MakeMKV silent, which is worse than unwanted.

use dumo_backends::mkvmerge::{Track, TrackKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// mkvmerge track ids to keep.
    pub audio: Vec<u32>,
    pub subtitles: Vec<u32>,
    /// What is dropped, for the record: e.g. `audio jpn (TrueHD)`.
    pub dropped: Vec<String>,
    /// Kinds kept whole because none of their tracks is in a wanted language.
    pub kept_whole: Vec<String>,
}

impl Selection {
    /// Whether anything is dropped, so the title needs remuxing at all.
    pub fn trims(&self) -> bool {
        !self.dropped.is_empty()
    }
}

fn describe(t: &Track) -> String {
    let kind = match t.kind {
        TrackKind::Audio => "audio",
        TrackKind::Subtitles => "subtitles",
        TrackKind::Video => "video",
        TrackKind::Other => "track",
    };
    let lang = if t.language.is_empty() { "und" } else { &t.language };
    format!("{kind} {lang} ({})", t.codec)
}

/// Choose the audio and subtitle tracks to keep for `languages` (ISO 639-2 codes).
pub fn choose(tracks: &[Track], languages: &[String]) -> Selection {
    let wanted = |t: &Track| {
        t.language.is_empty()
            || t.language == "und"
            || languages.iter().any(|l| l.eq_ignore_ascii_case(&t.language))
    };
    let mut sel = Selection {
        audio: Vec::new(),
        subtitles: Vec::new(),
        dropped: Vec::new(),
        kept_whole: Vec::new(),
    };
    for (kind, label) in [(TrackKind::Audio, "audio"), (TrackKind::Subtitles, "subtitles")] {
        let all: Vec<&Track> = tracks.iter().filter(|t| t.kind == kind).collect();
        let mut keep: Vec<&Track> = all.iter().copied().filter(|t| wanted(t)).collect();
        if keep.is_empty() && !all.is_empty() {
            let mut langs: Vec<&str> = all.iter().map(|t| t.language.as_str()).collect();
            langs.dedup();
            sel.kept_whole.push(format!(
                "no {label} in {}, so all {} kept ({})",
                languages.join("/"),
                all.len(),
                langs.join(", ")
            ));
            keep = all.clone();
        }
        for t in &all {
            if keep.iter().any(|k| k.id == t.id) {
                match kind {
                    TrackKind::Audio => sel.audio.push(t.id),
                    _ => sel.subtitles.push(t.id),
                }
            } else {
                sel.dropped.push(describe(t));
            }
        }
    }
    sel
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: u32, kind: TrackKind, language: &str) -> Track {
        Track {
            id,
            kind,
            language: language.into(),
            codec: "X".into(),
        }
    }

    fn eng() -> Vec<String> {
        vec!["eng".into()]
    }

    #[test]
    fn keeps_the_wanted_language_and_drops_the_rest() {
        let tracks = vec![
            t(0, TrackKind::Video, "eng"),
            t(1, TrackKind::Audio, "eng"),
            t(2, TrackKind::Audio, "jpn"),
            t(3, TrackKind::Subtitles, "eng"),
            t(4, TrackKind::Subtitles, "jpn"),
        ];
        let s = choose(&tracks, &eng());
        assert_eq!((&s.audio[..], &s.subtitles[..]), (&[1][..], &[3][..]));
        assert_eq!(s.dropped, vec!["audio jpn (X)", "subtitles jpn (X)"]);
        assert!(s.kept_whole.is_empty());
    }

    #[test]
    fn a_title_with_no_wanted_audio_keeps_its_own_rather_than_going_silent() {
        // Frieren's mini-anime: Japanese audio, English subtitles.
        let tracks = vec![
            t(0, TrackKind::Video, "eng"),
            t(1, TrackKind::Audio, "jpn"),
            t(2, TrackKind::Subtitles, "eng"),
        ];
        let s = choose(&tracks, &eng());
        assert_eq!((&s.audio[..], &s.subtitles[..]), (&[1][..], &[2][..]));
        assert!(!s.trims());
        assert_eq!(s.kept_whole.len(), 1);
        assert!(s.kept_whole[0].contains("jpn"), "{:?}", s.kept_whole);
    }

    #[test]
    fn a_track_that_does_not_say_its_language_is_kept() {
        let tracks = vec![
            t(1, TrackKind::Audio, "eng"),
            t(2, TrackKind::Audio, ""),
            t(3, TrackKind::Audio, "und"),
            t(4, TrackKind::Audio, "fra"),
        ];
        let s = choose(&tracks, &eng());
        assert_eq!(s.audio, vec![1, 2, 3]);
        assert_eq!(s.dropped, vec!["audio fra (X)"]);
    }

    #[test]
    fn several_languages_can_be_kept() {
        let tracks = vec![t(1, TrackKind::Audio, "eng"), t(2, TrackKind::Audio, "jpn")];
        let s = choose(&tracks, &["eng".into(), "jpn".into()]);
        assert_eq!(s.audio, vec![1, 2]);
        assert!(!s.trims());
    }
}
