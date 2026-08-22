//! Match a disc's titles against an episode list.
//!
//! A DVD from a series holds a contiguous run of episodes in broadcast order. So the
//! problem is: given K title runtimes in disc order, find the window of K consecutive
//! episodes whose runtimes line up best.
//!
//! This is a **fuzzy** match. It can be very convincing — an anthology whose episodes
//! run 51, 102 and 69 minutes is nearly unambiguous — or nearly worthless, as with a
//! sitcom where every episode is 22 minutes and runtime carries no information. The
//! matcher therefore reports not just its best answer but how much better that answer
//! is than the runner-up, because that margin is what actually distinguishes the two
//! situations. Nothing here is ever auto-accepted.

use crate::tmdb::{Episode, Season};
use dumo_core::Confidence;

/// One title paired with the episode it appears to be.
#[derive(Debug, Clone)]
pub struct TitleMatch {
    pub title_name: String,
    pub title_mins: f64,
    pub episode: Episode,
    /// Absolute runtime difference, in minutes.
    pub delta_mins: f64,
}

/// A proposed alignment of a disc's titles onto a run of episodes.
#[derive(Debug, Clone)]
pub struct SeasonMatch {
    pub season: u32,
    pub first_episode: u32,
    pub matches: Vec<TitleMatch>,
    /// Mean absolute runtime difference across the window.
    pub mean_delta: f64,
    /// Worst single difference in the window.
    pub max_delta: f64,
    /// Mean delta of the next-best window, if there was one. The gap between this and
    /// `mean_delta` is what tells you whether the match is distinctive or a coin flip.
    pub runner_up_delta: Option<f64>,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
    /// Starting episode numbers of other alignments that score within a hair of this
    /// one. A non-empty list means runtime alone did not decide the answer.
    pub tied_alternatives: Vec<u32>,
    /// Whether the disc number was needed to break a tie.
    pub disc_hint_used: bool,
    /// Every scored window: (mean, max, counted, start index).
    pub all_windows: Vec<(f64, f64, usize, usize)>,
}

impl SeasonMatch {
    /// How much better the best window is than the next best.
    pub fn margin(&self) -> Option<f64> {
        self.runner_up_delta.map(|r| r - self.mean_delta)
    }
}

/// A disc title to be matched.
#[derive(Debug, Clone)]
pub struct DiscTitle {
    pub name: String,
    pub duration_secs: f64,
    /// Tokenised dialogue from the title's subtitle track, when it has one.
    ///
    /// Far more discriminating than a runtime: two episodes of a series routinely run to
    /// the same minute, but they do not say the same words. `None` means the title had
    /// no text subtitle track, and scoring falls back to runtime alone.
    pub dialogue: Option<std::collections::HashSet<String>>,
}

impl DiscTitle {
    fn mins(&self) -> f64 {
        self.duration_secs / 60.0
    }
}

/// Order titles the way they sit on the disc.
///
/// MakeMKV names output `<segment>_tNN.mkv`, where `NN` is the title index — the disc's
/// own authored order, which for a series is broadcast order. Sorting on the whole
/// filename would instead sort by the segment prefix, which is arbitrary.
pub fn sort_by_title_index(titles: &mut [DiscTitle]) {
    fn index_of(name: &str) -> u32 {
        name.rsplit_once("_t")
            .and_then(|(_, tail)| {
                let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().ok()
            })
            .unwrap_or(u32::MAX)
    }
    titles.sort_by_key(|t| (index_of(&t.name), t.name.clone()));
}

/// Score one alignment: the mean absolute runtime difference.
///
/// Episodes without runtime data are skipped rather than counted as a perfect or
/// terrible match; TMDB omits runtime often enough that treating a missing value as
/// zero would invent confidence that isn't there.
fn score(titles: &[DiscTitle], episodes: &[Episode]) -> Option<(f64, f64, usize)> {
    let mut total = 0.0;
    let mut worst: f64 = 0.0;
    let mut counted = 0usize;

    for (t, e) in titles.iter().zip(episodes.iter()) {
        let Some(rt) = e.runtime_mins else { continue };
        let d = (t.mins() - f64::from(rt)).abs();
        total += d;
        worst = worst.max(d);
        counted += 1;
    }
    if counted == 0 {
        return None;
    }
    Some((total / counted as f64, worst, counted))
}

/// Two alignments this close in score are treated as indistinguishable on runtime alone.
const TIE_EPSILON_MINS: f64 = 0.25;

/// Extract the disc number from a volume label, e.g. `ESPN_30_FOR_30_DISC_2` → 2.
///
/// Worth doing because runtimes frequently tie. A three-episode disc labelled DISC 2 in
/// a set almost certainly holds the episodes after DISC 1's, and that ordering resolves
/// ties that runtime cannot.
pub fn disc_number_from_label(label: &str) -> Option<u32> {
    let spaced = label.replace(['_', '.', '-'], " ").to_ascii_uppercase();
    let words: Vec<&str> = spaced.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if matches!(*w, "DISC" | "DISK" | "D") {
            if let Some(n) = words.get(i + 1).and_then(|n| n.parse::<u32>().ok()) {
                return Some(n);
            }
        }
        // Also handles a suffixed form like "D2".
        if let Some(rest) = w.strip_prefix('D') {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(n) = rest.parse() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Find the best contiguous run of episodes matching these titles.
///
/// `titles` must already be in disc order. `disc_hint` is the disc's number within its
/// set, used only to break ties that runtime cannot.
pub fn match_season_with_hint(
    titles: &[DiscTitle],
    season: &Season,
    disc_hint: Option<u32>,
) -> Option<SeasonMatch> {
    let mut m = match_season(titles, season)?;

    // Only intervene where runtime genuinely cannot decide.
    let k = titles.len();
    let tied: Vec<&(f64, f64, usize, usize)> = m
        .all_windows
        .iter()
        .filter(|w| (w.0 - m.mean_delta).abs() <= TIE_EPSILON_MINS)
        .collect();

    if tied.len() > 1 {
        m.tied_alternatives = tied
            .iter()
            .map(|w| season.episodes[w.3].number)
            .filter(|n| *n != m.first_episode)
            .collect();

        if let Some(disc) = disc_hint {
            // A disc holding k episodes, numbered from 1, would start here if the set is
            // packed evenly. Used only to choose between alignments already tied on
            // runtime, never to override a better-scoring one.
            let expected_start = (disc.saturating_sub(1)) * k as u32 + 1;
            if let Some(w) = tied
                .iter()
                .find(|w| season.episodes[w.3].number == expected_start)
            {
                let start = w.3;
                let window = &season.episodes[start..start + k];
                m.first_episode = window.first().map(|e| e.number).unwrap_or(0);
                m.mean_delta = w.0;
                m.max_delta = w.1;
                m.matches = titles
                    .iter()
                    .zip(window.iter())
                    .map(|(t, e)| TitleMatch {
                        title_name: t.name.clone(),
                        title_mins: t.mins(),
                        episode: e.clone(),
                        delta_mins: e
                            .runtime_mins
                            .map(|rt| (t.mins() - f64::from(rt)).abs())
                            .unwrap_or(f64::NAN),
                    })
                    .collect();
                m.tied_alternatives = tied
                    .iter()
                    .map(|w| season.episodes[w.3].number)
                    .filter(|n| *n != m.first_episode)
                    .collect();
                m.disc_hint_used = true;
                m.evidence.push(format!(
                    "runtimes tie between alignments starting at episode {}; the label says \
                     disc {disc}, and {k} episodes per disc puts this one at episode \
                     {expected_start}",
                    tied.iter()
                        .map(|w| season.episodes[w.3].number.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }
    Some(m)
}

/// Find the best contiguous run of episodes matching these titles.
///
/// `titles` must already be in disc order.
pub fn match_season(titles: &[DiscTitle], season: &Season) -> Option<SeasonMatch> {
    let k = titles.len();
    if k == 0 || season.episodes.len() < k {
        return None;
    }

    let mut scored: Vec<(f64, f64, usize, usize)> = Vec::new(); // (mean, max, counted, start)
    for start in 0..=(season.episodes.len() - k) {
        let window = &season.episodes[start..start + k];
        if let Some((mean, worst, counted)) = score(titles, window) {
            scored.push((mean, worst, counted, start));
        }
    }
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let (mean_delta, max_delta, counted, start) = scored[0];
    let runner_up_delta = scored.get(1).map(|s| s.0);
    let window = &season.episodes[start..start + k];

    let matches: Vec<TitleMatch> = titles
        .iter()
        .zip(window.iter())
        .map(|(t, e)| TitleMatch {
            title_name: t.name.clone(),
            title_mins: t.mins(),
            episode: e.clone(),
            delta_mins: e
                .runtime_mins
                .map(|rt| (t.mins() - f64::from(rt)).abs())
                .unwrap_or(f64::NAN),
        })
        .collect();

    let margin = runner_up_delta.map(|r| r - mean_delta);

    // Confidence reflects both how close the fit is and how distinctive it is. A tight
    // fit that a dozen other windows also achieve is not evidence of anything.
    let confidence = if mean_delta <= 2.0 && margin.map(|m| m >= 2.0).unwrap_or(true) {
        Confidence::Strong
    } else {
        Confidence::Weak
    };

    let mut evidence = vec![
        format!(
            "matched {k} title(s) against season {} episodes {}–{}",
            season.number,
            window.first().map(|e| e.number).unwrap_or(0),
            window.last().map(|e| e.number).unwrap_or(0)
        ),
        format!(
            "runtimes differ by {mean_delta:.1} min on average, {max_delta:.1} min at worst"
        ),
    ];
    if counted < k {
        evidence.push(format!(
            "{} of {k} episodes had no runtime in TMDB and were not scored",
            k - counted
        ));
    }
    match margin {
        Some(m) if m >= 2.0 => evidence.push(format!(
            "next-best alignment is {m:.1} min/episode worse — this one stands out"
        )),
        Some(m) => evidence.push(format!(
            "next-best alignment is only {m:.1} min/episode worse — runtimes alone do not \
             clearly distinguish these; check the episode titles"
        )),
        None => evidence.push("only one possible alignment for this many titles".into()),
    }
    evidence.push("runtime matching is inference, not proof — confirm before filing".into());

    let tied_alternatives: Vec<u32> = scored
        .iter()
        .skip(1)
        .filter(|w| (w.0 - mean_delta).abs() <= TIE_EPSILON_MINS)
        .map(|w| season.episodes[w.3].number)
        .collect();

    if !tied_alternatives.is_empty() {
        evidence.push(format!(
            "runtime cannot separate this from {} other alignment(s), starting at \
             episode(s) {}",
            tied_alternatives.len(),
            tied_alternatives
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    Some(SeasonMatch {
        season: season.number,
        first_episode: window.first().map(|e| e.number).unwrap_or(0),
        matches,
        mean_delta,
        max_delta,
        runner_up_delta,
        confidence,
        evidence,
        tied_alternatives,
        disc_hint_used: false,
        all_windows: scored.clone(),
    })
}

/// Match against several seasons and return the best fit from each, best first.
///
/// Confidence is then recomputed against the best *alternative across all seasons*, not
/// merely the runner-up within one. A disc that fits season 1 to within half a minute is
/// not well identified if season 3 fits just as well — and per-season scoring alone
/// cannot see that.
pub fn match_seasons(
    titles: &[DiscTitle],
    seasons: &[Season],
    disc_hint: Option<u32>,
) -> Vec<SeasonMatch> {
    let mut out: Vec<SeasonMatch> = seasons
        .iter()
        .filter_map(|s| match_season_with_hint(titles, s, disc_hint))
        .collect();
    out.sort_by(|a, b| {
        a.mean_delta
            .partial_cmp(&b.mean_delta)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if out.len() > 1 {
        let best = out[0].mean_delta;
        // The strongest competitor is whichever is closer: another window in the same
        // season, or the best window in a different season.
        let cross_season = out[1].mean_delta;
        let within = out[0].runner_up_delta.unwrap_or(f64::INFINITY);
        let strongest_alternative = within.min(cross_season);
        let margin = strongest_alternative - best;

        out[0].runner_up_delta = Some(strongest_alternative);
        // A tie broken by the disc number is still not proof, but it is a real
        // independent signal, so it is worth distinguishing from an unresolved tie.
        out[0].confidence = if best <= 2.0 && margin >= 2.0 {
            Confidence::Strong
        } else {
            Confidence::Weak
        };
        out[0].evidence.retain(|e| !e.starts_with("next-best"));
        if margin >= 2.0 {
            out[0].evidence.push(format!(
                "closest alternative anywhere in the series is {margin:.1} min/episode \
                 worse — this alignment stands out"
            ));
        } else {
            let (alt_season, alt_first) = (out[1].season, out[1].first_episode);
            out[0].evidence.push(format!(
                "season {alt_season} from episode {alt_first} fits almost as well \
                 ({margin:.1} min/episode difference); runtimes cannot separate them, \
                 so check the episode titles"
            ));
        }
    }
    out
}

/// Turn a disc volume label into a search query.
///
/// Labels are shouty and carry disc numbering: `ESPN_30_FOR_30_DISC_1` really means
/// "30 for 30". Underscores become spaces and trailing disc/season markers are dropped,
/// since they are about the physical disc rather than the work.
pub fn query_from_label(label: &str) -> String {
    let spaced = label.replace(['_', '.'], " ");
    let words: Vec<&str> = spaced.split_whitespace().collect();

    let mut end = words.len();
    // Strip a trailing "DISC 1", "D2", "SEASON 3", "VOL 2" and similar.
    while end >= 1 {
        let last = words[end - 1].to_ascii_uppercase();
        let prev = if end >= 2 {
            words[end - 2].to_ascii_uppercase()
        } else {
            String::new()
        };
        let is_number = last.chars().all(|c| c.is_ascii_digit());
        let marker = ["DISC", "DISK", "SEASON", "VOL", "VOLUME", "SET", "S"];
        if is_number && marker.contains(&prev.as_str()) {
            end -= 2;
        } else if marker.contains(&last.as_str()) {
            end -= 1;
        } else {
            break;
        }
    }
    let kept: Vec<String> = words[..end]
        .iter()
        .map(|w| {
            // Labels are usually uppercase; title-case them so searches look natural.
            let lower = w.to_ascii_lowercase();
            if w.chars().all(|c| c.is_ascii_uppercase() || !c.is_alphabetic()) && w.len() > 1 {
                lower
            } else {
                w.to_string()
            }
        })
        .collect();
    kept.join(" ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(n: u32, rt: Option<u32>, name: &str) -> Episode {
        Episode {
            season: 1,
            number: n,
            name: name.into(),
            runtime_mins: rt,
            air_date: None,
            overview: None,
        }
    }

    fn title(name: &str, mins: f64) -> DiscTitle {
        DiscTitle {
            name: name.into(),
            duration_secs: mins * 60.0,
            dialogue: None,
        }
    }

    /// Real episode runtimes from TMDB for 30 for 30 season 1.
    fn season_one() -> Season {
        Season {
            number: 1,
            episodes: vec![
                ep(1, Some(51), "King's Ransom"),
                ep(2, Some(53), "The Band That Wouldn't Die"),
                ep(3, Some(51), "Small Potatoes: Who Killed the USFL?"),
                ep(4, Some(52), "Muhammad and Larry"),
                ep(5, Some(51), "Without Bias"),
                ep(6, Some(51), "The Legend of Jimmy the Greek"),
                ep(7, Some(102), "The U"),
                ep(8, Some(69), "Winning Time: Reggie Miller vs. the New York Knicks"),
                ep(9, Some(52), "Guru of Go"),
                ep(10, Some(81), "No Crossover: The Trial of Allen Iverson"),
            ],
        }
    }

    /// Disc 3's real durations: 102 and 69 minutes. Distinctive runtimes make this
    /// alignment unmistakable, which is exactly when the matcher should be confident.
    #[test]
    fn distinctive_runtimes_match_unambiguously() {
        let titles = vec![title("B1_t00.mkv", 102.5), title("D1_t01.mkv", 69.3)];
        let m = match_season(&titles, &season_one()).expect("matched");
        assert_eq!(m.first_episode, 7);
        assert_eq!(m.matches[0].episode.name, "The U");
        assert_eq!(m.matches[1].episode.number, 8);
        assert!(m.mean_delta < 1.0, "mean delta {}", m.mean_delta);
        assert_eq!(m.confidence, Confidence::Strong);
    }

    /// Disc 1's real durations: 51, 54, 52 minutes.
    #[test]
    fn matches_the_opening_run_of_a_season() {
        let titles = vec![
            title("B1_t01.mkv", 51.2),
            title("D1_t02.mkv", 54.0),
            title("E1_t04.mkv", 51.9),
        ];
        let m = match_season(&titles, &season_one()).expect("matched");
        assert_eq!(m.first_episode, 1);
        assert_eq!(m.matches[0].episode.name, "King's Ransom");
    }

    /// A sitcom: every episode the same length, so runtime says nothing. The matcher
    /// must report low confidence rather than pretending otherwise.
    #[test]
    fn uniform_runtimes_produce_low_confidence() {
        let season = Season {
            number: 1,
            episodes: (1..=10).map(|n| ep(n, Some(22), "Episode")).collect(),
        };
        let titles = vec![title("a_t00.mkv", 22.0), title("b_t01.mkv", 22.0)];
        let m = match_season(&titles, &season).expect("matched");
        // The fit is perfect but every window fits equally, so it proves nothing.
        assert!(m.mean_delta < 0.5);
        assert_eq!(m.margin(), Some(0.0));
        assert_eq!(
            m.confidence,
            Confidence::Weak,
            "a fit that every window achieves is not evidence"
        );
    }

    /// Disc 2's real durations. They tie exactly between episodes 2-4 and 4-6, so
    /// runtime alone picks the wrong one; the disc number resolves it.
    #[test]
    fn disc_number_breaks_an_exact_runtime_tie() {
        let titles = vec![
            title("B1_t00.mkv", 52.2),
            title("D1_t01.mkv", 51.2),
            title("E1_t02.mkv", 51.8),
        ];
        let season = season_one();

        // Without the hint the tie is resolved arbitrarily, and reported as a tie.
        let plain = match_season(&titles, &season).unwrap();
        assert!(
            !plain.tied_alternatives.is_empty(),
            "an exact tie must be reported, not hidden"
        );

        // With it, disc 2 of a 3-episode-per-disc set starts at episode 4.
        let hinted = match_season_with_hint(&titles, &season, Some(2)).unwrap();
        assert_eq!(hinted.first_episode, 4);
        assert!(hinted.disc_hint_used);
        assert_eq!(hinted.matches[0].episode.name, "Muhammad and Larry");
        assert_eq!(hinted.matches[2].episode.name, "The Legend of Jimmy the Greek");
    }

    /// The hint must never override a genuinely better runtime fit.
    #[test]
    fn disc_number_does_not_override_a_better_scoring_window() {
        // Disc 3's titles fit episodes 7-8 uniquely and far better than anything else.
        let titles = vec![title("a_t00.mkv", 102.5), title("b_t01.mkv", 69.3)];
        // A hint of disc 1 would suggest starting at episode 1, which fits terribly.
        let m = match_season_with_hint(&titles, &season_one(), Some(1)).unwrap();
        assert_eq!(m.first_episode, 7, "hint must not beat a clear runtime win");
        assert!(!m.disc_hint_used);
    }

    #[test]
    fn extracts_disc_numbers_from_labels() {
        assert_eq!(disc_number_from_label("ESPN_30_FOR_30_DISC_2"), Some(2));
        assert_eq!(disc_number_from_label("THE_WIRE_S01_D3"), Some(3));
        assert_eq!(disc_number_from_label("SHOW_DISK_11"), Some(11));
        assert_eq!(disc_number_from_label("MOVIE_TITLE"), None);
    }

    #[test]
    fn never_returns_an_auto_acceptable_confidence() {
        let titles = vec![title("a_t00.mkv", 102.0), title("b_t01.mkv", 69.0)];
        let m = match_season(&titles, &season_one()).unwrap();
        assert!(!m.confidence.is_auto_acceptable());
    }

    #[test]
    fn episodes_without_runtime_are_skipped_not_scored_as_zero() {
        let season = Season {
            number: 1,
            episodes: vec![ep(1, None, "A"), ep(2, Some(50), "B"), ep(3, None, "C")],
        };
        let titles = vec![title("a_t00.mkv", 50.0), title("b_t01.mkv", 50.0)];
        let m = match_season(&titles, &season).expect("matched");
        assert!(m.evidence.iter().any(|e| e.contains("no runtime")));
    }

    #[test]
    fn no_match_when_the_season_is_shorter_than_the_disc() {
        let season = Season {
            number: 1,
            episodes: vec![ep(1, Some(50), "A")],
        };
        let titles = vec![title("a_t00.mkv", 50.0), title("b_t01.mkv", 50.0)];
        assert!(match_season(&titles, &season).is_none());
    }

    #[test]
    fn sorts_by_makemkv_title_index_not_filename() {
        // Alphabetically B1 < D1 < E1 happens to be right here, but the index is what
        // actually encodes disc order, so a case where they disagree must follow the index.
        let mut titles = vec![
            title("Z9_t02.mkv", 1.0),
            title("A1_t00.mkv", 2.0),
            title("M5_t01.mkv", 3.0),
        ];
        sort_by_title_index(&mut titles);
        let names: Vec<&str> = titles.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["A1_t00.mkv", "M5_t01.mkv", "Z9_t02.mkv"]);
    }

    #[test]
    fn derives_a_search_query_from_a_volume_label() {
        assert_eq!(query_from_label("ESPN_30_FOR_30_DISC_1"), "espn 30 for 30");
        assert_eq!(query_from_label("THE_WIRE_SEASON_2"), "the wire");
        assert_eq!(query_from_label("FRIENDS_S05_DISC_3"), "friends s05");
        assert_eq!(query_from_label("MAD_MEN"), "mad men");
    }

    #[test]
    fn query_from_label_handles_odd_input() {
        assert_eq!(query_from_label(""), "");
        assert_eq!(query_from_label("DISC_1"), "");
    }
}
