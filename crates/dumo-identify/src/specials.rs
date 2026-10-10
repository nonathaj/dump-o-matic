//! Match a bonus disc against a series' specials.
//!
//! A box set's bonus discs hold the material TMDB files as season 0: retrospectives,
//! holiday specials, clip shows. Unlike a season disc, they are in no useful order —
//! one measured bonus disc held specials 13, 14, 4, 2 and 6 — and some of what they
//! carry is not listed at all. So the consecutive-run model the season matcher rests
//! on does not apply. Each title is instead matched to at most one special, jointly,
//! on what its dialogue says, and a title that fits nothing well is left unmatched
//! rather than forced onto the least-bad candidate.

use crate::matching::{DiscTitle, TitleMatch};
use crate::signals::{self, ScoringParams};
use crate::tmdb::Season;
use dumo_core::Confidence;
use std::collections::HashSet;

/// How much better a title's dialogue must fit its special than its next-best one, in
/// standard deviations of that title's scores, before the match is proposed.
///
/// A margin rather than an absolute level: the largest z-score a single standout can
/// reach grows with the number of specials, so a fixed level would refuse every match
/// for a show with few of them.
pub const MIN_SPECIAL_MARGIN: f64 = 1.5;

/// Weight of the runtime fit, in the same units as the dialogue z-score.
///
/// Small: published runtimes for specials are often a broadcast slot rather than the
/// running time (a 23 minute special listed at 30), so runtime only breaks ties and
/// vetoes the absurd.
const RUNTIME_WEIGHT: f64 = 0.5;

#[derive(Debug, Clone)]
pub struct SpecialsMatch {
    /// Proposed matches, in disc order, each with its margin over the title's next-best
    /// special.
    pub matched: Vec<(TitleMatch, f64)>,
    /// Titles that fit no special well enough: the special that came closest and the
    /// margin it had, when there was one.
    pub unmatched: Vec<(String, Option<(TitleMatch, f64)>)>,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
}

/// Match `titles` one-to-one against the episodes of `specials`.
pub fn match_specials(titles: &[DiscTitle], specials: &Season) -> SpecialsMatch {
    let params = ScoringParams::default();
    // Every special with a real name is a candidate; TMDB's placeholders ("Episode
    // 41") carry nothing to match and are left out. A special without a synopsis is
    // scored on its name alone — see below.
    let episodes: Vec<&crate::tmdb::Episode> = specials
        .episodes
        .iter()
        .filter(|e| !is_placeholder_name(&e.name))
        .collect();
    let unlisted = specials.episodes.len() - episodes.len();
    let (n, m) = (titles.len(), episodes.len());

    let with_dialogue: Vec<usize> = (0..n).filter(|&t| titles[t].dialogue.is_some()).collect();
    // Two scores, added. The synopsis, among only the specials that have one: scoring
    // by the share of a reference matched, a bare name of three words is matched in
    // full by one chance hit and outranks every real synopsis — "The Monster Wrangler"
    // beat "Lord Zedd's Monster Heads" on "monster" alone. And the name, among all of
    // them: specials come in near-identical siblings — "The Green Ranger Kata" and "The
    // White Ranger Kata" — whose synopses differ in what they happen to describe, while
    // the word that tells them apart is in the name. Measured: the White Ranger video
    // says "white" 30 times and "green" never, yet matched the Green one by 2.4 standard
    // deviations on synopsis alone, because only that synopsis listed the moves.
    let with_synopsis: Vec<usize> = (0..m)
        .filter(|&e| {
            let o = episodes[e].overview.as_deref().unwrap_or("");
            !o.trim().is_empty()
        })
        .collect();
    let synopses: Vec<HashSet<String>> = with_synopsis
        .iter()
        // The synopsis alone: the name is scored separately below, and counting it in
        // both would give a special with a synopsis two chances at its name's words.
        .map(|&e| {
            let synopsis = episodes[e].overview.as_deref().unwrap_or("");
            signals::stem_all(&dumo_core::text::tokenize(synopsis))
        })
        .collect();
    let names: Vec<HashSet<String>> = episodes
        .iter()
        .map(|e| signals::stem_all(&dumo_core::text::tokenize(&e.name)))
        .collect();
    let dialogue: Vec<_> = with_dialogue
        .iter()
        .map(|&t| signals::stem_counts(titles[t].dialogue.as_ref().expect("filtered")))
        .collect();
    let by_synopsis = signals::distinctive_fit(&dialogue, &synopses);
    let by_name = signals::distinctive_fit(&dialogue, &names);
    let fit_rows: Vec<Vec<f64>> = (0..dialogue.len())
        .map(|row| {
            (0..m)
                .map(|e| {
                    // No synopsis scores as an average one: no evidence either way.
                    let synopsis = with_synopsis
                        .iter()
                        .position(|&s| s == e)
                        .map(|i| by_synopsis[row][i])
                        .unwrap_or(0.0);
                    synopsis + by_name[row][e]
                })
                .collect()
        })
        .collect();
    let fit = |t: usize, e: usize| -> Option<f64> {
        with_dialogue
            .iter()
            .position(|&i| i == t)
            .map(|row| fit_rows[row][e])
    };

    // Dialogue fit plus a little runtime fit; `None` for a pairing the runtime rules out.
    let score = |t: usize, e: usize| -> Option<f64> {
        let mins = titles[t].duration_secs / 60.0;
        if signals::runtime_is_implausible(mins, episodes[e], &params) {
            return None;
        }
        let runtime = signals::runtime_score_with(mins, episodes[e], &params);
        Some(fit(t, e)? + RUNTIME_WEIGHT * runtime)
    };

    // Square the matrix with a free "no special" column for every title, and free rows
    // to fill the rest. Without that column a title that fits nothing — a 14 minute
    // featurette among specials of 25 minutes and up — is forced onto whichever special
    // it can reach, and pushes off the title that actually belongs there.
    let size = n + m;
    let cost: Vec<Vec<f64>> = (0..size)
        .map(|t| {
            (0..size)
                .map(|e| {
                    if t >= n || e >= m {
                        return 0.0;
                    }
                    let mins = titles[t].duration_secs / 60.0;
                    if signals::runtime_is_implausible(mins, episodes[e], &params) {
                        return f64::INFINITY;
                    }
                    -score(t, e).unwrap_or(0.0)
                })
                .collect()
        })
        .collect();
    let assignment = signals::assign_min_cost(&cost);

    let mut matched = Vec::new();
    let mut unmatched = Vec::new();
    for (t, title) in titles.iter().enumerate() {
        let e = assignment[t];
        let mins = title.duration_secs / 60.0;
        // The margin over this title's best *other* plausible special, whatever the
        // assignment did with it: a title that fits two specials alike is not
        // identified, but one the runtime rules out is no competitor.
        let margin = (e < m)
            .then(|| {
                let here = score(t, e)?;
                let next = (0..m)
                    .filter(|&o| o != e)
                    .filter_map(|o| score(t, o))
                    .fold(f64::NEG_INFINITY, f64::max);
                Some(if next.is_finite() { here - next } else { here })
            })
            .flatten();
        let accepted = margin.map(|x| x >= MIN_SPECIAL_MARGIN).unwrap_or(false);
        let proposal = (e < m).then(|| {
            let episode = episodes[e].clone();
            let delta_mins = episode
                .runtime_mins
                .map(|rt| (mins - f64::from(rt)).abs())
                .unwrap_or(f64::NAN);
            TitleMatch {
                title_name: title.name.clone(),
                title_mins: mins,
                episode,
                delta_mins,
            }
        });
        match (proposal, margin) {
            (Some(p), Some(x)) if accepted => matched.push((p, x)),
            (Some(p), Some(x)) => unmatched.push((title.name.clone(), Some((p, x)))),
            _ => unmatched.push((title.name.clone(), None)),
        }
    }

    let mut evidence = vec![
        format!(
            "matched each title to at most one of {m} specials on its dialogue, in any order — \
             a bonus disc follows no episode order"
        ),
        format!(
            "{} of {n} title(s) fit one special at least {MIN_SPECIAL_MARGIN:.1} standard \
             deviations better than any other; the rest are left where they are",
            matched.len()
        ),
    ];
    if unlisted > 0 {
        evidence.push(format!(
            "{unlisted} special(s) are unnamed placeholders on TMDB and could not be considered"
        ));
    }
    if with_dialogue.len() < n {
        evidence.push(format!(
            "{} title(s) had no subtitle track and cannot be matched this way",
            n - with_dialogue.len()
        ));
    }
    evidence.push("this is inference — confirm before filing".to_string());

    SpecialsMatch {
        matched,
        unmatched,
        confidence: Confidence::Weak,
        evidence,
    }
}

/// Whether an episode name is TMDB's placeholder for an unnamed one: "Episode 41".
fn is_placeholder_name(name: &str) -> bool {
    name.strip_prefix("Episode ")
        .map(|n| !n.is_empty() && n.trim().chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false)
}

/// Whether a volume label marks a disc of extras rather than episodes.
pub fn is_bonus_label(label: &str) -> bool {
    label
        .to_ascii_uppercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| {
            matches!(
                w,
                "BONUS" | "EXTRAS" | "EXTRA" | "SPECIAL" | "SPECIALS" | "FEATURES" | "SUPPLEMENTS"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmdb::Episode;

    fn special(n: u32, runtime: u32, name: &str, overview: &str) -> Episode {
        Episode {
            season: 0,
            number: n,
            name: name.into(),
            runtime_mins: Some(runtime),
            air_date: None,
            overview: Some(overview.into()),
        }
    }

    fn specials() -> Season {
        Season {
            number: 0,
            episodes: vec![
                special(1, 30, "Fan Club Video", ""),
                special(
                    2,
                    30,
                    "Alpha's Magical Christmas",
                    "Alpha throws a Christmas party while the Rangers help Santa with his sleigh",
                ),
                special(3, 42, "Karate Club", "Learn karate katas"),
                special(
                    4,
                    25,
                    "Lord Zedd's Monster Heads",
                    "Lord Zedd crashes the Halloween party",
                ),
                special(5, 52, "Bulk and Skull", "The slapstick misadventures of Bulk and Skull"),
                special(6, 32, "A Look Back", "Cast and crew look back at the series"),
            ],
        }
    }

    fn title(name: &str, mins: f64, words: &str) -> DiscTitle {
        let mut text = String::from("power rangers zordon alpha rangers ");
        for _ in 0..8 {
            text.push_str(words);
            text.push(' ');
        }
        DiscTitle {
            name: name.into(),
            duration_secs: mins * 60.0,
            dialogue: Some(dumo_core::text::count_words(&text)),
        }
    }

    #[test]
    fn matches_out_of_order_specials_and_leaves_the_unknown_alone() {
        let titles = vec![
            title("t00", 33.0, "look back cast crew series memories"),
            title("t01", 25.0, "zedd halloween party monster"),
            title("t02", 23.0, "christmas santa sleigh party"),
            title("t03", 52.0, "bulk skull slapstick misadventures"),
            title("t04", 14.0, "fans convention cosplay"),
        ];
        let r = match_specials(&titles, &specials());
        let got: Vec<(&str, u32)> = r
            .matched
            .iter()
            .map(|(m, _)| (m.title_name.as_str(), m.episode.number))
            .collect();
        assert_eq!(got, vec![("t00", 6), ("t01", 4), ("t02", 2), ("t03", 5)]);
        assert_eq!(r.unmatched.len(), 1);
        assert_eq!(r.unmatched[0].0, "t04");
        assert_eq!(r.confidence, Confidence::Weak);
    }

    #[test]
    fn a_title_without_dialogue_is_never_matched() {
        let mut t = title("t00", 25.0, "zedd halloween");
        t.dialogue = None;
        let r = match_specials(&[t], &specials());
        assert!(r.matched.is_empty());
    }

    #[test]
    fn placeholder_names_are_recognised() {
        assert!(is_placeholder_name("Episode 41"));
        assert!(!is_placeholder_name("Episode 41: The Return"));
        assert!(!is_placeholder_name("Fan Club Video"));
    }

    #[test]
    fn a_special_without_a_synopsis_is_matched_on_its_name() {
        let titles = vec![
            title("t00", 30.0, "fan club video members welcome"),
            title("t01", 25.0, "zedd halloween party monster"),
        ];
        let r = match_specials(&titles, &specials());
        let got: Vec<(&str, u32)> = r
            .matched
            .iter()
            .map(|(m, _)| (m.title_name.as_str(), m.episode.number))
            .collect();
        assert_eq!(got, vec![("t00", 1), ("t01", 4)]);
    }

    #[test]
    fn bonus_labels() {
        assert!(is_bonus_label("MMPR_BONUS_D1"));
        assert!(is_bonus_label("SHOW S1 EXTRAS"));
        assert!(!is_bonus_label("MMPR_S3D1"));
        assert!(!is_bonus_label("BONUSES_GALORE"));
    }
}
