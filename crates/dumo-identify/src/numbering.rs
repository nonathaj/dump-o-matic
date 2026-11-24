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
    subtitle_numbers_through(entries, usize::MAX)
}

/// How many entries before the last are double length: the most shifts the site's
/// numbering can have made to any episode after them.
pub fn double_count(entries: &[(u32, Option<u32>)]) -> usize {
    let last = entries.len().saturating_sub(1);
    doubles(entries).into_iter().filter(|&i| i < last).count()
}

/// Indexes of the double-length entries, a double being one at least [`DOUBLE_RATIO`]
/// times the season's median running time.
fn doubles(entries: &[(u32, Option<u32>)]) -> Vec<usize> {
    let mut known: Vec<u32> = entries.iter().filter_map(|e| e.1).filter(|&m| m > 0).collect();
    if known.is_empty() {
        return Vec::new();
    }
    known.sort_unstable();
    let median = f64::from(known[known.len() / 2]);
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.1.map(|m| f64::from(m) >= DOUBLE_RATIO * median).unwrap_or(false))
        .map(|(i, _)| i)
        .collect()
}

/// As [`subtitle_numbers`], but only the first `taken` double-length entries are
/// numbered as two: the site does not always split every one. Danny Phantom season 2
/// lists "Reign Storm" and "The Ultimate Enemy" at 44 minutes each; OpenSubtitles splits
/// the first and numbers the second as one, so the episodes after them sit one number
/// higher, not two.
pub fn subtitle_numbers_through(entries: &[(u32, Option<u32>)], taken: usize) -> Option<Vec<Vec<u32>>> {
    // Past the last double nothing is shifted, so with none before the last entry the
    // numbers are TMDB's own and the caller needs no other reading.
    if double_count(entries) == 0 {
        return None;
    }
    let split: Vec<usize> = doubles(entries).into_iter().take(taken).collect();
    let mut shift = 0;
    Some(
        entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let first = e.0 + shift;
                if split.contains(&i) {
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

    /// Danny Phantom season 2: "Reign Storm" (E04) and "The Ultimate Enemy" (E05) are both
    /// 44 minutes in TMDB. The site splits the first only.
    #[test]
    fn only_the_first_doubles_can_be_taken_as_split() {
        let s = season(&[22, 22, 22, 44, 44, 22, 22, 22]);
        assert_eq!(double_count(&s), 2);
        let all = subtitle_numbers(&s).unwrap();
        assert_eq!(all[5], vec![8], "both split: two higher");
        let one = subtitle_numbers_through(&s, 1).unwrap();
        assert_eq!(one[3], vec![4, 5]);
        assert_eq!(one[4], vec![6], "the second double is one episode there");
        assert_eq!(one[5], vec![7], "so everything after is one higher");
        let none = subtitle_numbers_through(&s, 0).unwrap();
        assert_eq!(none[5], vec![6], "TMDB's own numbers");
    }

    /// A season of hour-long episodes has no doubles among them: the median is the
    /// running time that ordinary episodes have.
    #[test]
    fn long_episodes_are_judged_against_the_seasons_own_length() {
        assert_eq!(subtitle_numbers(&season(&[52, 54, 51, 58, 53])), None);
    }
}
