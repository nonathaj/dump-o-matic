//! Where a season's episodes sit in a subtitle site's numbering.
//!
//! TMDB lists a two-part episode as one entry of double length, where OpenSubtitles
//! numbers its halves as two episodes. Every episode after it is then one number higher
//! there than in TMDB's listing: Friends season 6 has "The One That Could Have Been" at
//! E15 in TMDB, and Unagi, TMDB's E16, is E17 on OpenSubtitles. Asked for E16, the site
//! hands back the second half of the two-parter, and every title after it is placed one
//! episode early.
//!
//! The pattern can only be guessed at from the listing, by the double-length entries, so
//! this gives the guess and leaves it to the caller to check it against the titles.

/// An entry at least this many times the season's median running time is a two-parter
/// listed as one.
const DOUBLE_RATIO: f64 = 1.8;

/// For each of a season's `(number, running time in minutes)` entries, the episode
/// numbers a subtitle site is expected to give it: one, or two for a double-length entry
/// whose halves the site numbers apart.
///
/// `None` when no entry before the last is double length. Past that point nothing is
/// shifted, so the numbers are TMDB's own and the caller needs no other reading.
pub fn subtitle_numbers(entries: &[(u32, Option<u32>)]) -> Option<Vec<Vec<u32>>> {
    let mut known: Vec<u32> = entries.iter().filter_map(|e| e.1).filter(|&m| m > 0).collect();
    if known.is_empty() {
        return None;
    }
    known.sort_unstable();
    let median = f64::from(known[known.len() / 2]);
    let is_double = |e: &(u32, Option<u32>)| e.1.map(|m| f64::from(m) >= DOUBLE_RATIO * median).unwrap_or(false);
    let last = entries.len().checked_sub(1)?;
    if !entries.iter().take(last).any(is_double) {
        return None;
    }
    let mut shift = 0;
    Some(
        entries
            .iter()
            .map(|e| {
                let first = e.0 + shift;
                if is_double(e) {
                    shift += 1;
                    vec![first, first + 1]
                } else {
                    vec![first]
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn season(runtimes: &[u32]) -> Vec<(u32, Option<u32>)> {
        runtimes.iter().enumerate().map(|(i, &m)| (i as u32 + 1, Some(m))).collect()
    }

    /// Friends season 6, as TMDB lists it: 23 entries, with "The One That Could Have Been"
    /// (E15, 45 min) and "The One with the Proposal" (E23, 44 min) as one entry each.
    #[test]
    fn a_two_parter_listed_as_one_shifts_every_episode_after_it() {
        let s = season(&[
            23, 22, 22, 22, 22, 22, 22, 22, 23, 22, 22, 22, 22, 23, 45, 22, 22, 22, 22, 23, 23, 23, 44,
        ]);
        let n = subtitle_numbers(&s).unwrap();
        assert_eq!(n[13], vec![14]);
        assert_eq!(n[14], vec![15, 16], "the two-parter takes two numbers");
        assert_eq!(n[15], vec![17], "Unagi, TMDB's E16");
        assert_eq!(n[21], vec![23]);
        assert_eq!(n[22], vec![24, 25], "the final two-parter, too");
    }

    /// A two-parter at the end of a season shifts nothing after it, so the numbers are
    /// TMDB's own: how seasons 5 and 7 to 10 were already placed.
    #[test]
    fn a_two_parter_at_the_end_needs_no_other_numbering() {
        assert_eq!(subtitle_numbers(&season(&[22, 22, 22, 22, 44])), None);
        assert_eq!(subtitle_numbers(&season(&[22, 22, 22, 22])), None);
        assert_eq!(subtitle_numbers(&[(1, None), (2, None)]), None);
        assert_eq!(subtitle_numbers(&[]), None);
    }

    /// A season of hour-long episodes has no doubles among them: the median is the
    /// running time that ordinary episodes have.
    #[test]
    fn long_episodes_are_judged_against_the_seasons_own_length() {
        assert_eq!(subtitle_numbers(&season(&[52, 54, 51, 58, 53])), None);
    }
}
