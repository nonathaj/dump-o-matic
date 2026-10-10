//! Compare a ripped title's dialogue with a reference subtitle of a known episode.
//!
//! A synopsis says what an episode is *about*, so matching dialogue against one is
//! inference from a few dozen words. A reference subtitle says what is *spoken*, so the
//! two transcripts of the same episode share long runs of identical words, and
//! transcripts of different episodes share almost none beyond stock phrases. That makes
//! this a check of a different kind: not "which episode fits best" but "is this the
//! episode it was said to be".
//!
//! The unit compared is the word trigram — three consecutive words. Single words are
//! shared by every episode of a series; trigrams of real dialogue mostly are not, and
//! they survive the small differences between a disc's captions and a subtitle someone
//! else made (line breaks, punctuation, the odd paraphrased line).

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

/// Fewest trigrams a transcript needs before its overlap means anything. Fewer is a
/// stray forced caption, not an episode's dialogue.
pub const MIN_PHRASES: usize = 200;

/// Share of the smaller transcript's trigrams the other must contain to call them the
/// same dialogue.
///
/// Different episodes of one series share stock phrases ("what are you", "let's get out
/// of here") and a theme song, which puts a floor under the overlap of unrelated
/// transcripts. Measured on the disc captions of twelve episodes of one season (66
/// pairs): median 2.6%, worst 5.3%. This sits about five times above that floor. What
/// two independent transcripts of the *same* episode share has not yet been measured
/// against real reference subtitles; it is expected to be far higher, since most lines
/// are transcribed word for word either way.
pub const MIN_SAME_SHARE: f64 = 0.25;

/// How many times its nearest rival a confirming overlap must be. Guards against a
/// series so formulaic that every episode clears [`MIN_SAME_SHARE`].
pub const MIN_SHARE_RATIO: f64 = 3.0;

/// A transcript reduced to the set of its word trigrams.
#[derive(Debug, Clone, PartialEq)]
pub struct Phrases(HashSet<u64>);

impl Phrases {
    /// Fingerprint plain dialogue text (markup already stripped).
    ///
    /// Words are lowercased and apostrophes dropped, so "DON'T" in a caption and "don't"
    /// or "dont" in a subtitle agree. Unlike word scoring elsewhere, short words are
    /// kept: "I", "a" and "to" are what make a trigram a particular sentence.
    pub fn from_text(text: &str) -> Self {
        let words: Vec<String> = text
            .to_lowercase()
            .replace(['\'', '’'], "")
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect();
        let set = words
            .windows(3)
            .map(|w| {
                let mut h = DefaultHasher::new();
                w.hash(&mut h);
                h.finish()
            })
            .collect();
        Phrases(set)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Enough dialogue to compare at all.
    pub fn is_usable(&self) -> bool {
        self.len() >= MIN_PHRASES
    }

    /// Share of the smaller transcript's trigrams found in the larger.
    ///
    /// Measured against the smaller so that a title holding only part of an episode, or
    /// a reference that leaves out a recap, is not penalised for what one side lacks.
    pub fn shared(&self, other: &Phrases) -> f64 {
        let (small, large) = if self.len() <= other.len() {
            (self, other)
        } else {
            (other, self)
        };
        if small.is_empty() {
            return 0.0;
        }
        small.0.intersection(&large.0).count() as f64 / small.len() as f64
    }
}

/// What a reference subtitle says about one title's proposed episode.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// The title's dialogue is the proposed episode's: high overlap with its reference,
    /// far above any other reference checked.
    Confirmed { share: f64, next_best: f64 },
    /// Another reference matches the title, and the proposed one does not.
    Contradicted { share: f64, better: usize, better_share: f64 },
    /// Neither confirmed nor contradicted: too little overlap with anything, or too
    /// close a contest. Says nothing either way.
    Inconclusive { share: f64, next_best: f64 },
    /// No usable dialogue on one side or the other.
    Unchecked,
}

impl Verdict {
    pub fn describe(&self, names: &[String]) -> String {
        match self {
            Verdict::Confirmed { share, next_best } => format!(
                "reference subtitles confirm it: {:.0}% of its phrases match, against at \
                 most {:.0}% for any other episode checked",
                share * 100.0,
                next_best * 100.0
            ),
            Verdict::Contradicted {
                share,
                better,
                better_share,
            } => format!(
                "reference subtitles CONTRADICT it: {:.0}% of its phrases match, but {:.0}% \
                 match {}",
                share * 100.0,
                better_share * 100.0,
                names.get(*better).map(String::as_str).unwrap_or("another episode")
            ),
            Verdict::Inconclusive { share, next_best } => format!(
                "reference subtitles are inconclusive: {:.0}% of its phrases match, {:.0}% \
                 for the closest other episode",
                share * 100.0,
                next_best * 100.0
            ),
            Verdict::Unchecked => "no reference subtitle or no dialogue to check".into(),
        }
    }
}

/// Check one title against the reference of the episode it was proposed as, with every
/// other reference at hand as the control.
///
/// `references[proposed]` is the proposed episode's; the rest are references already
/// fetched for other episodes, costing nothing more to compare. `None` marks an episode
/// with no reference available.
pub fn check(title: &Phrases, references: &[Option<Phrases>], proposed: usize) -> Verdict {
    let Some(Some(own)) = references.get(proposed) else {
        return Verdict::Unchecked;
    };
    if !title.is_usable() || !own.is_usable() {
        return Verdict::Unchecked;
    }
    let share = title.shared(own);
    let rival = references
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != proposed)
        .filter_map(|(i, r)| r.as_ref().filter(|r| r.is_usable()).map(|r| (i, title.shared(r))))
        .max_by(|a, b| a.1.total_cmp(&b.1));
    let next_best = rival.map(|r| r.1).unwrap_or(0.0);

    let clears = |s: f64, other: f64| s >= MIN_SAME_SHARE && s >= MIN_SHARE_RATIO * other;
    if clears(share, next_best) {
        Verdict::Confirmed { share, next_best }
    } else if let Some((better, better_share)) = rival.filter(|r| clears(r.1, share)) {
        Verdict::Contradicted {
            share,
            better,
            better_share,
        }
    } else {
        Verdict::Inconclusive { share, next_best }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pseudo-dialogue: `n` sentences drawn from a vocabulary seeded by `seed`, so
    /// different seeds give different "episodes" that still share common words.
    fn dialogue(seed: u64, n: usize) -> String {
        const WORDS: &[&str] = &[
            "the", "you", "we", "zordon", "rangers", "go", "now", "power", "morph", "time",
            "putty", "rita", "help", "school", "karate", "bulk", "skull", "alpha", "ay",
            "yi", "monster", "city", "park", "zord", "teleport", "come", "on", "guys",
            "what", "is", "that", "lets", "get", "out", "here", "of", "it", "a", "to",
        ];
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut out = String::new();
        for _ in 0..n * 6 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            out.push_str(WORDS[(x >> 33) as usize % WORDS.len()]);
            out.push(' ');
        }
        out
    }

    #[test]
    fn captions_and_subtitles_of_the_same_lines_agree() {
        let caption = Phrases::from_text("I DON'T THINK ZEDD'S DONE WITH US, GUYS.");
        let subtitle = Phrases::from_text("I don't think Zedd's done with us, guys!");
        assert_eq!(caption, subtitle);
        assert_eq!(caption.shared(&subtitle), 1.0);
    }

    #[test]
    fn same_dialogue_is_confirmed_and_different_dialogue_is_not() {
        let ep = |s| Some(Phrases::from_text(&dialogue(s, 400)));
        let refs = vec![ep(1), ep(2), ep(3)];
        let title = Phrases::from_text(&dialogue(2, 400));
        assert!(matches!(check(&title, &refs, 1), Verdict::Confirmed { .. }));
        match check(&title, &refs, 0) {
            Verdict::Contradicted { better, .. } => assert_eq!(better, 1),
            v => panic!("expected a contradiction, got {v:?}"),
        }
    }

    #[test]
    fn a_partial_title_still_matches_its_episode() {
        // The first half of the episode only, as a title split at a chapter would be.
        let full = Phrases::from_text(&dialogue(7, 600));
        let half_text: String = dialogue(7, 600).split(' ').take(1800).collect::<Vec<_>>().join(" ");
        let half = Phrases::from_text(&half_text);
        let refs = vec![Some(full), Some(Phrases::from_text(&dialogue(8, 600)))];
        assert!(matches!(check(&half, &refs, 0), Verdict::Confirmed { .. }));
    }

    #[test]
    fn no_reference_or_too_little_dialogue_checks_nothing() {
        let title = Phrases::from_text(&dialogue(1, 400));
        assert_eq!(check(&title, &[None], 0), Verdict::Unchecked);
        let stray = Phrases::from_text("previously on power rangers");
        let refs = vec![Some(Phrases::from_text(&dialogue(1, 400)))];
        assert_eq!(check(&stray, &refs, 0), Verdict::Unchecked);
    }

    #[test]
    fn unrelated_dialogue_with_no_better_match_is_inconclusive() {
        let refs = vec![
            Some(Phrases::from_text(&dialogue(1, 400))),
            Some(Phrases::from_text(&dialogue(2, 400))),
        ];
        let title = Phrases::from_text(&dialogue(99, 400));
        assert!(matches!(check(&title, &refs, 0), Verdict::Inconclusive { .. }));
    }
}
