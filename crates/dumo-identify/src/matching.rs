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

    Some(SeasonMatch {
        season: season.number,
        first_episode: window.first().map(|e| e.number).unwrap_or(0),
        matches,
        mean_delta,
        max_delta,
        runner_up_delta,
        confidence,
        evidence,
    })
}

/// Match against several seasons and return the best fit from each, best first.
///
/// Confidence is then recomputed against the best *alternative across all seasons*, not
/// merely the runner-up within one. A disc that fits season 1 to within half a minute is
/// not well identified if season 3 fits just as well — and per-season scoring alone
/// cannot see that.
pub fn match_seasons(titles: &[DiscTitle], seasons: &[Season]) -> Vec<SeasonMatch> {
    let mut out: Vec<SeasonMatch> = seasons
        .iter()
        .filter_map(|s| match_season(titles, s))
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
        }
    }

    fn title(name: &str, mins: f64) -> DiscTitle {
        DiscTitle {
            name: name.into(),
            duration_secs: mins * 60.0,
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
