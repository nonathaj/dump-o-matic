//! Choosing among several titles that are the same episode.
//!
//! A Blu-ray often carries an episode more than once: the same playlist twice, a version
//! with commentary tracks, an extended cut in standard definition. Placing titles by
//! their dialogue finds every one of them at the same episode; this decides which to
//! keep, and whether two of them are not versions at all but the two halves of a
//! two-part episode that the listing counts as one.

use crate::transcript::{Phrases, MIN_SAME_SHARE};

/// What is known of one title that is a candidate for an episode.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub name: String,
    /// Height of the tallest video stream, in lines.
    pub height: u32,
    pub audio_tracks: usize,
    pub duration_secs: f64,
}

/// The one to keep, by position: the sharpest picture, then the fewest audio tracks — a
/// version carrying commentaries has more — then the running time closest to
/// `target_secs`, then the first on the disc.
pub fn keep(candidates: &[Candidate], target_secs: Option<f64>) -> Option<usize> {
    let off = |c: &Candidate| target_secs.map(|t| (c.duration_secs - t).abs()).unwrap_or(0.0);
    (0..candidates.len()).min_by(|&a, &b| {
        let (ca, cb) = (&candidates[a], &candidates[b]);
        cb.height
            .cmp(&ca.height)
            .then(ca.audio_tracks.cmp(&cb.audio_tracks))
            .then(off(ca).total_cmp(&off(cb)))
            .then(a.cmp(&b))
    })
}

/// Which part of an episode a title holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    Whole,
    /// One half of a two-part episode, 1 or 2.
    Part(u8),
}

/// Whether a title is the whole episode or one half of it, from its dialogue against the
/// two halves of the episode's reference subtitle.
///
/// A title holding the whole episode shares a great deal with both halves. One holding
/// half shares with that half and hardly anything with the other: what the two halves
/// have in common is stock phrases, the same floor any two episodes share.
pub fn piece(title: &Phrases, first_half: &Phrases, second_half: &Phrases) -> Piece {
    let (a, b) = (title.shared(first_half), title.shared(second_half));
    let one_sided = |hit: f64, miss: f64| hit >= MIN_SAME_SHARE && miss * 3.0 < hit;
    if one_sided(a, b) {
        Piece::Part(1)
    } else if one_sided(b, a) {
        Piece::Part(2)
    } else {
        Piece::Whole
    }
}

/// Split SRT text into the cues starting in the first half of its running time and the
/// rest, each still SRT.
///
/// The midpoint is half the start of the last cue, which is as close to half the episode
/// as the subtitle says.
pub fn split_srt(srt: &str) -> (String, String) {
    let blocks: Vec<&str> = srt
        .split("\n\n")
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .collect();
    let start = |b: &str| -> Option<f64> {
        let line = b.lines().find(|l| l.contains("-->"))?;
        let t = line.split("-->").next()?.trim().replace(',', ".");
        let mut parts = t.split(':');
        let h: f64 = parts.next()?.parse().ok()?;
        let m: f64 = parts.next()?.parse().ok()?;
        let s: f64 = parts.next()?.parse().ok()?;
        Some(h * 3600.0 + m * 60.0 + s)
    };
    let last = blocks.iter().filter_map(|b| start(b)).fold(0.0, f64::max);
    let mid = last / 2.0;
    let (mut first, mut second) = (String::new(), String::new());
    for b in blocks {
        let half = if start(b).map(|s| s < mid).unwrap_or(false) {
            &mut first
        } else {
            &mut second
        };
        half.push_str(b);
        half.push_str("\n\n");
    }
    (first, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, height: u32, audio: usize, secs: f64) -> Candidate {
        Candidate {
            name: name.into(),
            height,
            audio_tracks: audio,
            duration_secs: secs,
        }
    }

    #[test]
    fn keeps_the_hd_broadcast_cut_over_extended_and_commentary_versions() {
        // Friends S07E13 on Blu-ray: broadcast HD twice, an SD extended cut with and
        // without commentaries.
        let titles = [
            c("t12", 1080, 1, 1319.0),
            c("t16", 480, 2, 1677.0),
            c("t20", 480, 4, 1677.0),
            c("t27", 1080, 1, 1319.0),
        ];
        assert_eq!(keep(&titles, Some(1320.0)), Some(0));
        // Commentary versions in HD lose to the one without.
        let titles = [c("t14", 1080, 2, 1387.0), c("t07", 1080, 1, 1387.0)];
        assert_eq!(keep(&titles, None), Some(1));
        assert_eq!(keep(&[], None), None);
    }

    fn words(seed: u64, n: usize) -> String {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                format!("w{}", (x >> 40) % 400)
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn tells_the_halves_of_a_two_parter_from_the_whole() {
        let (one, two) = (words(1, 2000), words(2, 2000));
        let (h1, h2) = (Phrases::from_text(&one), Phrases::from_text(&two));
        assert_eq!(piece(&Phrases::from_text(&one), &h1, &h2), Piece::Part(1));
        assert_eq!(piece(&Phrases::from_text(&two), &h1, &h2), Piece::Part(2));
        let whole = Phrases::from_text(&format!("{one} {two}"));
        assert_eq!(piece(&whole, &h1, &h2), Piece::Whole);
    }

    #[test]
    fn splits_a_subtitle_at_half_its_running_time() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nfirst\n\n\
                   2\n00:10:00,000 --> 00:10:02,000\nmiddle\n\n\
                   3\n00:20:00,000 --> 00:20:02,000\nlast\n";
        let (a, b) = split_srt(srt);
        assert!(a.contains("first") && !a.contains("middle"));
        assert!(b.contains("middle") && b.contains("last"));
    }
}
