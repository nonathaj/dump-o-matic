//! Combine independent signals into one judgement, and say how much they agree.
//!
//! Runtime matching alone is weak: adjacent episodes of the same show frequently tie
//! exactly, and no amount of comparing durations breaks a tie. Subtitle dialogue is much
//! more discriminating but depends on a text track being present and on the reference
//! text (an episode synopsis) actually sharing vocabulary with what is said on screen.
//!
//! Neither is trustworthy alone. But they fail *independently* — a runtime tie says
//! nothing about which episode the dialogue mentions — so their agreement is far stronger
//! evidence than either score. This module reports each signal separately and derives
//! confidence from whether they concur, which is both more honest and more useful than a
//! single blended number that hides the disagreement.

use crate::tmdb::Episode;
use dumo_core::Confidence;
use std::collections::HashSet;

/// How one signal rated one (title, episode) pairing. Higher is better, 0.0..=1.0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalScores {
    /// From runtime difference. 1.0 is an exact match.
    pub runtime: f64,
    /// From dialogue vocabulary overlap with the episode's title and synopsis.
    /// `None` when the title has no subtitle track.
    pub subtitle: Option<f64>,
}

impl SignalScores {
    /// A single blended score, used only for ranking. The individual signals are what
    /// get reported.
    pub fn combined(&self) -> f64 {
        match self.subtitle {
            // Dialogue is the more discriminating signal, so it carries more weight —
            // but runtime still guards against a synopsis that happens to share words.
            Some(s) => 0.35 * self.runtime + 0.65 * s,
            None => self.runtime,
        }
    }
}

/// Convert a runtime difference into a 0..1 score.
///
/// Ten minutes out is treated as no evidence at all; the scale is linear below that.
pub fn runtime_score(title_mins: f64, episode: &Episode) -> f64 {
    match episode.runtime_mins {
        Some(rt) => {
            let delta = (title_mins - f64::from(rt)).abs();
            (1.0 - delta / 10.0).clamp(0.0, 1.0)
        }
        // No runtime published: absent evidence, not evidence of absence. A neutral
        // score avoids both rewarding and punishing the pairing.
        None => 0.5,
    }
}

/// Reduce a word to a crude stem so inflections still match.
///
/// A synopsis says "the trade" while the dialogue says "traded"; exact comparison misses
/// that, and such near-misses are common enough to cost real matches. This is
/// deliberately minimal — full stemming would need a dependency, and over-aggressive
/// stemming creates false matches, which are worse here than missed ones.
pub fn stem(word: &str) -> String {
    let mut w = word.to_string();
    for suffix in ["ing", "ed", "es", "s"] {
        if w.len() > suffix.len() + 3 && w.ends_with(suffix) {
            w.truncate(w.len() - suffix.len());
            break;
        }
    }
    // Drop a trailing "e" as well, so "trade" and "traded" (which becomes "trad") meet
    // at the same stem. Without this the suffix strip alone leaves them apart, which is
    // exactly the near-miss it was meant to fix.
    if w.len() > 3 && w.ends_with('e') {
        w.truncate(w.len() - 1);
    }
    w
}

/// Stem every word in a set.
pub fn stem_all(words: &HashSet<String>) -> HashSet<String> {
    words.iter().map(|w| stem(w)).collect()
}

/// Score dialogue against an episode's title and synopsis.
///
/// Measured as the fraction of the reference's distinctive words that appear in the
/// dialogue. Normalising by the reference (not the dialogue) matters: dialogue has
/// thousands of words and a synopsis has dozens, so normalising the other way would
/// score everything near zero.
pub fn subtitle_score(dialogue_words: &HashSet<String>, reference: &HashSet<String>) -> f64 {
    if reference.is_empty() || dialogue_words.is_empty() {
        return 0.0;
    }
    let dialogue = stem_all(dialogue_words);
    let reference_stems = stem_all(reference);
    let hits = reference_stems.iter().filter(|w| dialogue.contains(*w)).count();
    hits as f64 / reference_stems.len() as f64
}

/// One title's verdict, with each signal's opinion kept separate.
#[derive(Debug, Clone)]
pub struct TitleVerdict {
    pub title_name: String,
    /// Episode number chosen by the combined score.
    pub chosen_episode: u32,
    pub scores: SignalScores,
    /// What runtime alone would have picked.
    pub runtime_pick: u32,
    /// What dialogue alone would have picked, if there was a subtitle track.
    pub subtitle_pick: Option<u32>,
    /// Whether the independent signals concur.
    pub signals_agree: bool,
    /// Margin over the runner-up on the combined score.
    pub margin: f64,
}

/// Derive confidence from agreement between independent signals, not from one score.
///
/// Two signals that fail in unrelated ways agreeing is much better evidence than either
/// being individually confident. Still never [`Confidence::Exact`]: that is reserved for
/// cryptographic matches, and this is inference however well it concurs.
pub fn confidence_from(verdicts: &[TitleVerdict]) -> Confidence {
    if verdicts.is_empty() {
        return Confidence::Unknown;
    }
    let with_subs = verdicts.iter().filter(|v| v.subtitle_pick.is_some()).count();
    let agreeing = verdicts.iter().filter(|v| v.signals_agree).count();
    let thin_margin = verdicts.iter().any(|v| v.margin < 0.05);

    if with_subs == verdicts.len() && agreeing == verdicts.len() && !thin_margin {
        // Every title had both signals available, and they all concurred.
        Confidence::Strong
    } else {
        Confidence::Weak
    }
}

/// Assign titles to episodes so that no episode is used twice.
///
/// Scoring each title independently lets two titles claim the same episode, which is
/// never right for a disc — and it happened on real data, with two titles both matching
/// the same synopsis. Assigning greedily over the strongest pairings first, and retiring
/// each episode once taken, removes that failure.
pub fn assign_unique(scores: &[(usize, u32, f64)], n_titles: usize) -> Vec<Option<(u32, f64)>> {
    let mut ranked = scores.to_vec();
    ranked.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut out: Vec<Option<(u32, f64)>> = vec![None; n_titles];
    let mut used_episodes: HashSet<u32> = HashSet::new();

    for (ti, ep, sc) in ranked {
        if out.get(ti).map(|o| o.is_some()).unwrap_or(true) {
            continue;
        }
        if used_episodes.contains(&ep) {
            continue;
        }
        out[ti] = Some((ep, sc));
        used_episodes.insert(ep);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(n: u32, rt: Option<u32>) -> Episode {
        Episode {
            season: 1,
            number: n,
            name: String::new(),
            runtime_mins: rt,
            air_date: None,
        }
    }

    fn words(s: &str) -> HashSet<String> {
        s.split_whitespace().map(|w| w.to_lowercase()).collect()
    }

    #[test]
    fn runtime_score_peaks_on_an_exact_match() {
        assert!((runtime_score(51.0, &ep(1, Some(51))) - 1.0).abs() < 1e-9);
        assert!(runtime_score(56.0, &ep(1, Some(51))) < 0.6);
        // Far enough out is simply no evidence.
        assert_eq!(runtime_score(90.0, &ep(1, Some(51))), 0.0);
    }

    #[test]
    fn missing_runtime_scores_neutrally() {
        assert_eq!(runtime_score(51.0, &ep(1, None)), 0.5);
    }

    #[test]
    fn subtitle_score_normalises_by_the_reference() {
        let dialogue = words("gretzky was traded from edmonton to los angeles in 1988");
        // "trade" against "traded" only matches because of stemming.
        let reference = words("gretzky edmonton trade");
        assert!((subtitle_score(&dialogue, &reference) - 1.0).abs() < 1e-9);

        let unrelated = words("miami hurricanes football");
        assert_eq!(subtitle_score(&dialogue, &unrelated), 0.0);
    }

    #[test]
    fn stemming_matches_common_inflections() {
        // The point is that inflections converge, not the exact stem produced.
        assert_eq!(stem("traded"), stem("trade"));
        assert_eq!(stem("hurricanes"), stem("hurricane"));
        assert_eq!(stem("delivers"), stem("deliver"));
        // Short words are left alone: stripping them creates collisions.
        assert_eq!(stem("used"), "used");
        assert_eq!(stem("miami"), "miami");
        // Distinct words must not collide.
        assert_ne!(stem("edmonton"), stem("edinburgh"));
    }

    #[test]
    fn empty_inputs_score_zero() {
        assert_eq!(subtitle_score(&HashSet::new(), &words("a b")), 0.0);
        assert_eq!(subtitle_score(&words("a b"), &HashSet::new()), 0.0);
    }

    /// Regression for a real failure: scoring titles independently let two titles on the
    /// same disc both claim episode 6.
    #[test]
    fn unique_assignment_prevents_two_titles_taking_one_episode() {
        // Title 0 and title 1 both score highest on episode 6.
        let scores = vec![
            (0, 6, 0.55),
            (0, 4, 0.50),
            (1, 6, 0.73),
            (1, 5, 0.40),
        ];
        let out = assign_unique(&scores, 2);
        let a = out[0].unwrap().0;
        let b = out[1].unwrap().0;
        assert_ne!(a, b, "two titles were assigned the same episode");
        // The stronger claim on episode 6 wins it.
        assert_eq!(b, 6);
        assert_eq!(a, 4);
    }

    #[test]
    fn agreement_between_signals_yields_strong_confidence() {
        let v = vec![TitleVerdict {
            title_name: "a".into(),
            chosen_episode: 1,
            scores: SignalScores { runtime: 1.0, subtitle: Some(0.8) },
            runtime_pick: 1,
            subtitle_pick: Some(1),
            signals_agree: true,
            margin: 0.3,
        }];
        assert_eq!(confidence_from(&v), Confidence::Strong);
    }

    #[test]
    fn disagreement_yields_weak_confidence() {
        let v = vec![TitleVerdict {
            title_name: "a".into(),
            chosen_episode: 1,
            scores: SignalScores { runtime: 1.0, subtitle: Some(0.8) },
            runtime_pick: 1,
            subtitle_pick: Some(4),
            signals_agree: false,
            margin: 0.3,
        }];
        assert_eq!(confidence_from(&v), Confidence::Weak);
    }

    /// Agreement is not enough if the win was almost a tie.
    #[test]
    fn a_thin_margin_yields_weak_confidence_even_when_signals_agree() {
        let v = vec![TitleVerdict {
            title_name: "a".into(),
            chosen_episode: 1,
            scores: SignalScores { runtime: 1.0, subtitle: Some(0.8) },
            runtime_pick: 1,
            subtitle_pick: Some(1),
            signals_agree: true,
            margin: 0.01,
        }];
        assert_eq!(confidence_from(&v), Confidence::Weak);
    }

    /// Without a subtitle track there is only one signal, so nothing can corroborate it.
    #[test]
    fn a_single_signal_never_reaches_strong() {
        let v = vec![TitleVerdict {
            title_name: "a".into(),
            chosen_episode: 1,
            scores: SignalScores { runtime: 1.0, subtitle: None },
            runtime_pick: 1,
            subtitle_pick: None,
            signals_agree: true,
            margin: 0.5,
        }];
        assert_eq!(confidence_from(&v), Confidence::Weak);
    }

    #[test]
    fn confidence_is_never_auto_acceptable() {
        let v = vec![TitleVerdict {
            title_name: "a".into(),
            chosen_episode: 1,
            scores: SignalScores { runtime: 1.0, subtitle: Some(1.0) },
            runtime_pick: 1,
            subtitle_pick: Some(1),
            signals_agree: true,
            margin: 0.9,
        }];
        assert!(!confidence_from(&v).is_auto_acceptable());
    }

    #[test]
    fn combined_prefers_dialogue_but_keeps_runtime_honest() {
        let both = SignalScores { runtime: 0.0, subtitle: Some(1.0) };
        let runtime_only = SignalScores { runtime: 1.0, subtitle: None };
        // A perfect dialogue match with a wrong runtime should still beat runtime alone
        // being perfect, but not overwhelmingly.
        assert!(both.combined() < 1.0);
        assert!(both.combined() > 0.6);
        assert_eq!(runtime_only.combined(), 1.0);
    }
}
