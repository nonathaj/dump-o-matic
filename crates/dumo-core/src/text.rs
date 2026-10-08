//! Text tokenisation shared by the parts that compare words.
//!
//! Lives here rather than beside either caller because both sides of a dialogue
//! comparison must tokenise identically: ripped subtitles (via `dumo-backends`) and
//! reference synopses (via `dumo-identify`). Two copies that drifted apart would quietly
//! stop matching, and the identification layer has no business depending on the
//! ffmpeg-wrapping layer just to borrow a string split.

use std::collections::{HashMap, HashSet};

/// Shortest token kept. Below this, tokens are punctuation fragments and initials rather
/// than words that could distinguish one episode from another.
pub const MIN_TOKEN_LEN: usize = 3;

/// Split text into lowercase word tokens.
///
/// Deliberately *not* filtered against a stopword list. A hand-written list of common
/// words would be English-only, arbitrary, and impossible to tune without recompiling —
/// and it is unnecessary: which words are uninformative is a property of the candidate
/// set, not of the language, and is measured directly by callers using inverse document
/// frequency. Words that appear in every candidate get almost no weight automatically,
/// in any language.
///
/// The only filter is a minimum length, which drops punctuation fragments rather than
/// making a judgement about meaning.
pub fn tokenize(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| w.chars().count() >= MIN_TOKEN_LEN)
        .map(str::to_string)
        .collect()
}

/// Split text into lowercase word tokens, counting how often each occurs.
///
/// The same split as [`tokenize`], keeping repetition. A word a title says twenty-five
/// times is about that title in a way a word said once is not, and only a count can
/// tell the two apart.
pub fn count_words(text: &str) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for w in text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| w.chars().count() >= MIN_TOKEN_LEN)
    {
        *counts.entry(w.to_string()).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_repeated_words() {
        let c = count_words("Brick by brick, the BRICK wall -- a wall!");
        assert_eq!(c.get("brick"), Some(&3));
        assert_eq!(c.get("wall"), Some(&2));
        assert_eq!(c.get("by"), None, "below the minimum length");
    }

    #[test]
    fn keeps_words_and_drops_fragments() {
        let w = tokenize("The Gretzky trade shocked Edmonton -- a 1988 deal!");
        assert!(w.contains("gretzky"));
        assert!(w.contains("edmonton"));
        assert!(w.contains("1988"));
        // Common words are kept here on purpose: weighting them down is the corpus's
        // job, not a hardcoded list's.
        assert!(w.contains("the"));
        assert!(!w.contains("--"));
        assert!(!w.iter().any(|t| t.is_empty()));
    }

    /// Subtitles arrive upper-case and synopses mixed-case; they must land on the same
    /// tokens or nothing ever matches.
    #[test]
    fn case_is_normalised_so_both_sides_agree() {
        assert_eq!(tokenize("JORDAN DELIVERS"), tokenize("Jordan delivers"));
    }

    #[test]
    fn apostrophes_stay_inside_words() {
        let w = tokenize("don't");
        assert!(w.contains("don't"));
    }

    #[test]
    fn empty_input_yields_no_tokens() {
        assert!(tokenize("").is_empty());
    }
}
