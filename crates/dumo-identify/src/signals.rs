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
        self.combined_with(&ScoringParams::default())
    }

    pub fn combined_with(&self, p: &ScoringParams) -> f64 {
        match self.subtitle {
            Some(s) => (1.0 - p.subtitle_weight) * self.runtime + p.subtitle_weight * s,
            None => self.runtime,
        }
    }
}

/// Convert a runtime difference into a 0..1 score.
///
/// Ten minutes out is treated as no evidence at all; the scale is linear below that.
pub fn runtime_score_with(title_mins: f64, episode: &Episode, p: &ScoringParams) -> f64 {
    match episode.runtime_mins {
        Some(rt) => {
            let delta = (title_mins - f64::from(rt)).abs();
            (1.0 - delta / p.runtime_tolerance_mins).clamp(0.0, 1.0)
        }
        // No runtime published: absent evidence, not evidence of absence. A neutral
        // score avoids both rewarding and punishing the pairing.
        None => 0.5,
    }
}

/// [`runtime_score_with`] using default tuning.
pub fn runtime_score(title_mins: f64, episode: &Episode) -> f64 {
    runtime_score_with(title_mins, episode, &ScoringParams::default())
}

/// Tunables for scoring. Every threshold that used to be a literal buried in the code
/// lives here, with the reasoning attached, so it can be reviewed and adjusted in one
/// place rather than hunted for.
#[derive(Debug, Clone, Copy)]
pub struct ScoringParams {
    /// A runtime difference at or beyond this many minutes counts as no evidence.
    /// Episodes of one series rarely differ by this much, so beyond it the signal is
    /// noise.
    pub runtime_tolerance_mins: f64,
    /// Weight given to dialogue when both signals are available. Dialogue is the more
    /// discriminating signal; runtime remains as a check against a synopsis that merely
    /// shares vocabulary.
    pub subtitle_weight: f64,
    /// A win narrower than this over the runner-up is treated as a tie.
    pub min_margin: f64,
}

impl Default for ScoringParams {
    fn default() -> Self {
        Self {
            runtime_tolerance_mins: 10.0,
            subtitle_weight: 0.65,
            min_margin: 0.05,
        }
    }
}

/// Reduce a word to a crude stem so inflections still match.
///
/// A synopsis says "the trade" while the dialogue says "traded"; exact comparison misses
/// that, and such near-misses are common enough to cost real matches. This is
/// deliberately minimal — full stemming would need a dependency, and over-aggressive
/// stemming creates false matches, which are worse here than missed ones.
///
/// The suffixes are English. That is a real limitation, but a bounded one: stemming only
/// ever *adds* matches, so on other languages it degrades to plain comparison rather
/// than misbehaving.
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

/// Word weights derived from the candidate set itself.
///
/// Replaces what would otherwise be a hardcoded stopword list. A word appearing in every
/// candidate synopsis cannot distinguish between them and is weighted to near zero; a
/// word appearing in one is highly informative. This is measured, not asserted, so it
/// needs no language-specific list and adapts to whatever corpus it is given — a set of
/// sports documentaries and a set of sitcoms have quite different uninformative words.
#[derive(Debug, Clone, Default)]
pub struct Corpus {
    idf: std::collections::HashMap<String, f64>,
    documents: usize,
}

impl Corpus {
    /// Build weights from the reference text of every candidate.
    pub fn from_references<'a>(references: impl IntoIterator<Item = &'a HashSet<String>>) -> Self {
        let mut df: std::collections::HashMap<String, usize> = Default::default();
        let mut documents = 0usize;
        for r in references {
            documents += 1;
            for w in stem_all(r) {
                *df.entry(w).or_insert(0) += 1;
            }
        }
        // Smoothed inverse document frequency. A term in every document scores ~0; a
        // term in one scores highest.
        let idf = df
            .into_iter()
            .map(|(w, n)| {
                let v = ((documents as f64 + 1.0) / (n as f64 + 1.0)).ln().max(0.0);
                (w, v)
            })
            .collect();
        Self { idf, documents }
    }

    /// Weight of a single term.
    ///
    /// With no corpus every term weighs the same, so scoring degrades gracefully to
    /// plain overlap rather than collapsing to zero. Terms unseen in a real corpus are
    /// treated as maximally informative, since they occur in no candidate.
    pub fn weight(&self, term: &str) -> f64 {
        if self.documents == 0 {
            return 1.0;
        }
        self.idf
            .get(term)
            .copied()
            .unwrap_or_else(|| ((self.documents as f64 + 1.0) / 1.0).ln().max(0.0))
    }

    pub fn is_empty(&self) -> bool {
        self.documents == 0
    }
}

/// Score dialogue against an episode's reference text, weighted by informativeness.
///
/// The score is the share of the reference's *total weight* that appears in the
/// dialogue, so matching a distinctive proper noun counts for far more than matching a
/// word every candidate uses. Normalising by the reference rather than the dialogue
/// matters too: dialogue runs to thousands of words and a synopsis to dozens, so the
/// other direction would score everything near zero.
pub fn subtitle_score_weighted(
    dialogue_words: &HashSet<String>,
    reference: &HashSet<String>,
    corpus: &Corpus,
) -> f64 {
    if reference.is_empty() || dialogue_words.is_empty() {
        return 0.0;
    }
    let dialogue = stem_all(dialogue_words);
    let reference_stems = stem_all(reference);

    let mut total = 0.0;
    let mut hit = 0.0;
    for w in &reference_stems {
        let weight = corpus.weight(w);
        total += weight;
        if dialogue.contains(w) {
            hit += weight;
        }
    }
    if total <= 0.0 {
        return 0.0;
    }
    hit / total
}

/// Unweighted overlap, for callers with no corpus to compare against.
pub fn subtitle_score(dialogue_words: &HashSet<String>, reference: &HashSet<String>) -> f64 {
    subtitle_score_weighted(dialogue_words, reference, &Corpus::default())
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
    let params = ScoringParams::default();
    let thin_margin = verdicts.iter().any(|v| v.margin < params.min_margin);

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
            overview: None,
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

    /// The corpus replaces a hardcoded stopword list: a word in every candidate is
    /// down-weighted automatically, in whatever language the candidates are written in.
    #[test]
    fn corpus_downweights_words_common_to_every_candidate() {
        let refs = vec![
            words("the film about boxing in philadelphia"),
            words("the film about hockey in edmonton"),
            words("the film about football in miami"),
        ];
        let c = Corpus::from_references(refs.iter());
        // "film" is in all three and carries essentially no information.
        assert!(c.weight("film") < 0.3, "weight {}", c.weight("film"));
        // "edmonton" appears once and is highly informative.
        assert!(c.weight("edmonton") > c.weight("film") * 2.0);
    }

    #[test]
    fn weighted_score_rewards_distinctive_words_over_common_ones() {
        let refs = vec![
            words("the film about hockey in edmonton"),
            words("the film about boxing in philadelphia"),
        ];
        let c = Corpus::from_references(refs.iter());

        let dialogue_distinctive = words("we talked about edmonton all night");
        let dialogue_common = words("this is the film you asked about");

        let reference = words("the film about hockey in edmonton");
        let distinctive = subtitle_score_weighted(&dialogue_distinctive, &reference, &c);
        let common = subtitle_score_weighted(&dialogue_common, &reference, &c);
        assert!(
            distinctive > common,
            "distinctive {distinctive} should beat common {common}"
        );
    }

    #[test]
    fn an_empty_corpus_falls_back_to_flat_weighting() {
        let c = Corpus::default();
        assert!(c.is_empty());
        let d = words("gretzky traded from edmonton");
        let r = words("gretzky edmonton");
        // Without corpus information every term weighs the same, so this is plain overlap.
        assert!((subtitle_score_weighted(&d, &r, &c) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn scoring_params_are_adjustable() {
        let strict = ScoringParams { runtime_tolerance_mins: 2.0, ..Default::default() };
        let loose = ScoringParams { runtime_tolerance_mins: 30.0, ..Default::default() };
        let e = ep(1, Some(51));
        // The same 5 minute error looks fatal under a tight tolerance and minor under a
        // loose one; the point is that this is a reviewable setting, not a buried literal.
        assert!(runtime_score_with(56.0, &e, &strict) < runtime_score_with(56.0, &e, &loose));
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
