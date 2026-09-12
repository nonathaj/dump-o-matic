//! Detecting a "play all" title.
//!
//! TV DVDs commonly offer, alongside the individual episodes, one title that plays them
//! back to back. It holds no unique content: the same streams, concatenated. Ripping it
//! costs as much disk as the whole rest of the disc and gives identification a title
//! that matches no episode.
//!
//! Detection is by arithmetic, not by name: a play-all title runs for the sum of the
//! titles it contains. On a real disc that came to 8519 s against a sum of 8521 s — two
//! seconds apart over 142 minutes.
//!
//! The bar is set deliberately high because the cost of a false positive is skipping
//! real content, which this project treats as data loss. A title is only ever excluded
//! when it is the longest, there are at least two others to account for it, and its
//! duration matches their total closely.

use dumo_core::TitleInfo;

/// How far a candidate's duration may sit from the sum of the others.
///
/// Expressed as a fraction so it scales with length. Chapter boundaries and the
/// occasional dropped frame put real play-all titles a second or two off an exact sum;
/// 0.5% of a two-hour title is ~36 s, comfortably past that and still far too tight for
/// an unrelated title to satisfy by chance.
const DURATION_TOLERANCE: f64 = 0.005;

/// Fewest constituent titles a play-all must account for.
///
/// With only one other title, "its duration equals the sum of the others" degenerates
/// into "the two are the same length", which is true of a disc holding an episode and
/// its commentary track and means nothing.
const MIN_CONSTITUENTS: usize = 2;

/// The play-all title among `titles`, if there is one.
///
/// Returns its index. Never returns a title that is not strictly the longest: a
/// concatenation is always longer than its parts.
pub fn detect(titles: &[TitleInfo]) -> Option<PlayAll> {
    if titles.len() < MIN_CONSTITUENTS + 1 {
        return None;
    }

    let longest = titles.iter().max_by_key(|t| t.duration_secs)?;
    let others: Vec<&TitleInfo> = titles
        .iter()
        .filter(|t| t.index != longest.index)
        .collect();
    if others.len() < MIN_CONSTITUENTS {
        return None;
    }

    // A concatenation is strictly longer than any of its parts.
    if others.iter().any(|t| t.duration_secs >= longest.duration_secs) {
        return None;
    }

    let sum: u64 = others.iter().map(|t| t.duration_secs).sum();
    if sum == 0 {
        return None;
    }
    let delta = longest.duration_secs.abs_diff(sum);
    if (delta as f64) / (sum as f64) > DURATION_TOLERANCE {
        return None;
    }

    Some(PlayAll {
        index: longest.index,
        duration_secs: longest.duration_secs,
        constituents_total_secs: sum,
        constituents: others.len(),
        estimated_bytes: longest.estimated_bytes,
    })
}

/// A title identified as playing the others back to back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayAll {
    pub index: u32,
    pub duration_secs: u64,
    pub constituents_total_secs: u64,
    pub constituents: usize,
    pub estimated_bytes: u64,
}

impl PlayAll {
    /// Why this title was taken to be a play-all, for the operator to judge.
    pub fn evidence(&self) -> String {
        format!(
            "title {} runs {}s, the same as titles 1-{} combined ({}s, {}s apart) — \
             a play-all holding no unique content",
            self.index,
            self.duration_secs,
            self.constituents,
            self.constituents_total_secs,
            self.duration_secs.abs_diff(self.constituents_total_secs),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(index: u32, duration_secs: u64, chapters: u32) -> TitleInfo {
        TitleInfo {
            index,
            duration_secs,
            estimated_bytes: duration_secs * 1_000_000,
            chapters,
            output_name: format!("t{index:02}.mkv"),
        }
    }

    #[test]
    fn finds_the_play_all_on_a_real_disc() {
        // Wonder Woman Season 3 Disc 1, as scanned.
        let titles = vec![
            t(0, 8519, 22),
            t(1, 2846, 8),
            t(2, 2840, 8),
            t(3, 2835, 8),
        ];
        let found = detect(&titles).expect("play-all not detected");
        assert_eq!(found.index, 0);
        assert_eq!(found.constituents, 3);
        assert_eq!(found.constituents_total_secs, 8521);
    }

    #[test]
    fn a_film_with_extras_is_not_a_play_all() {
        // The feature is longest but nowhere near the sum of the extras.
        let titles = vec![t(0, 7200, 30), t(1, 600, 2), t(2, 480, 1), t(3, 300, 1)];
        assert_eq!(detect(&titles), None);
    }

    #[test]
    fn two_titles_of_equal_length_are_not_a_play_all() {
        // An episode and its commentary track: the "sum of the others" test degenerates.
        let titles = vec![t(0, 2846, 8), t(1, 2846, 8)];
        assert_eq!(detect(&titles), None);
    }

    #[test]
    fn a_disc_of_plain_episodes_has_no_play_all() {
        let titles = vec![t(0, 2846, 8), t(1, 2840, 8), t(2, 2835, 8)];
        assert_eq!(detect(&titles), None);
    }

    #[test]
    fn a_coincidental_sum_outside_tolerance_is_left_alone() {
        // 5000 vs 4800: 4% out, far past the 0.5% bar.
        let titles = vec![t(0, 5000, 10), t(1, 1600, 5), t(2, 1600, 5), t(3, 1600, 5)];
        assert_eq!(detect(&titles), None);
    }

    #[test]
    fn single_title_disc_is_safe() {
        assert_eq!(detect(&[t(0, 7200, 30)]), None);
        assert_eq!(detect(&[]), None);
    }
}
