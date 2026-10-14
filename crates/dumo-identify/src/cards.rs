//! On-screen episode title cards, read from a title's subtitles.
//!
//! Many series put the episode's number and name on screen, and the subtitle track
//! captions it: `"Episode 5: Phantoms of the Dead"`. That names the episode outright,
//! where runtime and a synopsis only narrow it down. Episodes of one series run to the
//! same minute, and a short synopsis shares few words with the dialogue, so on Frieren
//! S1 disc 1 both left episodes 1–8 almost tied with 3–10, while every title's card
//! said exactly which episode it was.
//!
//! A title usually also captions the *next* episode's card, in a preview after the
//! credits. The title's own card comes first, so the earliest card is the one that
//! counts; reading any card would let an alignment shifted by one episode score as
//! well as the right one.
//!
//! Some series caption no card of their own but do say what comes next: Attack on
//! Titan's narrator closes each episode with `Next episode: "Children."`. A preview
//! that says so names the episode after this one, which places the title just as well;
//! it is only used when the title has no card of its own.

use crate::tmdb::Episode;

/// Words read after "episode N" for the name. Longer than any episode name in practice.
const FOLLOWING_WORDS: usize = 16;

/// An "episode N ..." or "next episode ..." caption, in the order it appeared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// The number as captioned, if there was one; a "next episode" preview may give
    /// only the name. Some series number episodes across seasons, so this is shown in
    /// evidence but never trusted on its own.
    pub number: Option<u32>,
    /// The words that followed, normalised: where the episode's name should be.
    pub following: String,
    /// Whether the caption said "next episode": it names the episode after this one.
    pub next: bool,
}

impl Card {
    /// What the card is, for the operator: `title card for episode 5`.
    pub fn describe(&self) -> String {
        match (self.next, self.number) {
            (false, Some(n)) => format!("title card for episode {n}"),
            (true, Some(n)) => format!("preview of episode {n}"),
            (_, None) => "preview of the next episode".into(),
        }
    }
}

/// Lowercase words, with OCR's `|` for `I` undone and punctuation dropped, so that
/// `"Episode 2: It Didn't Have to Be Magic..."` and a TMDB name compare equal.
fn words(text: &str) -> Vec<String> {
    text.chars()
        .map(|c| match c {
            '|' => 'i',
            '\'' | '’' => '\u{0}',
            c if c.is_alphanumeric() => c.to_ascii_lowercase(),
            _ => ' ',
        })
        .filter(|&c| c != '\u{0}')
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Every "episode N" and "next episode" caption in a title's dialogue text, in order.
pub fn find(text: &str) -> Vec<Card> {
    let w = words(text);
    let mut out = Vec::new();
    for i in 0..w.len().saturating_sub(1) {
        if w[i] != "episode" {
            continue;
        }
        let next = i > 0 && w[i - 1] == "next";
        let number = w[i + 1].parse::<u32>().ok();
        // Without "next", a number is what makes it a caption rather than dialogue.
        if number.is_none() && !next {
            continue;
        }
        let start = i + 1 + usize::from(number.is_some());
        let end = (start + FOLLOWING_WORDS).min(w.len());
        out.push(Card {
            number,
            following: w[start..end].join(" "),
            next,
        });
    }
    out
}

/// The earliest card of `cards` worth reporting: the title's own, else a preview.
pub fn first(cards: &[Card]) -> Option<&Card> {
    cards.iter().find(|c| !c.next).or_else(|| cards.first())
}

/// The episode named by the earliest card that names one of `episodes`.
///
/// The name must follow the number: "episode 3" alone is as likely to be dialogue as
/// a caption. Where one name begins another ("Pilot", "Pilot Part 2"), the longer wins.
///
/// Only when no card of the title's own names an episode is a "next episode" preview
/// used, for the episode before the one it names. A preview of a season's first
/// episode says nothing about this season, so it places nothing.
pub fn own_episode<'a>(cards: &[Card], episodes: &'a [Episode]) -> Option<&'a Episode> {
    let names: Vec<(String, &Episode)> = episodes
        .iter()
        .map(|e| (words(&e.name).join(" "), e))
        .filter(|(n, _)| !n.is_empty())
        .collect();
    let own = cards.iter().filter(|c| !c.next && c.number.is_some());
    if let Some(e) = own.filter_map(|c| named(c, &names)).next() {
        return Some(e);
    }
    let after = cards.iter().filter(|c| c.next).find_map(|c| named(c, &names))?;
    episodes
        .iter()
        .find(|e| e.season == after.season && e.number + 1 == after.number)
}

/// The episode of `names` that `card` names, the longest name that fits.
fn named<'a>(card: &Card, names: &[(String, &'a Episode)]) -> Option<&'a Episode> {
    names
        .iter()
        .filter(|(n, _)| {
            card.following == *n
                || card
                    .following
                    .strip_prefix(n.as_str())
                    .map(|rest| rest.starts_with(' '))
                    .unwrap_or(false)
        })
        .max_by_key(|(n, _)| n.len())
        .map(|(_, e)| *e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(number: u32, name: &str) -> Episode {
        Episode {
            season: 1,
            number,
            name: name.into(),
            runtime_mins: Some(25),
            air_date: None,
            overview: None,
        }
    }

    fn season() -> Vec<Episode> {
        vec![
            ep(1, "The Journey's End"),
            ep(2, "It Didn't Have to Be Magic..."),
            ep(3, "Killing Magic"),
            ep(4, "The Land Where Souls Rest"),
        ]
    }

    /// The shape of Frieren S1 disc 1's OCR'd text: the opening song, the episode's own
    /// card, dialogue that happens to use another episode's name, then the preview card.
    #[test]
    fn the_earliest_card_is_the_titles_own_not_the_preview() {
        let text = "Like a fairy tale it's a sign the story has reached its end \
            \"Episode 3: Killing Magic\" \"27 years after the death of Himmel\" \
            Aureole, the land where souls rest. \
            \"Episode 4 The Land Where Souls Rest\"";
        let cards = find(text);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].number, Some(3));
        assert_eq!(own_episode(&cards, &season()).map(|e| e.number), Some(3));
    }

    #[test]
    fn ocr_and_punctuation_differences_still_match() {
        // OCR reads "I" as "|"; TMDB adds an ellipsis the caption may not have.
        let cards = find("\"Episode 2: |t Didn't Have to Be Magic\" Fern, come here.");
        assert_eq!(own_episode(&cards, &season()).map(|e| e.number), Some(2));
    }

    #[test]
    fn a_number_without_a_name_is_not_a_card() {
        // Dialogue, or a series numbered across seasons: the number alone proves nothing.
        let cards = find("Did you watch episode 3 last night? It was great.");
        assert_eq!(cards.len(), 1);
        assert_eq!(own_episode(&cards, &season()), None);
        assert!(find("no captions here").is_empty());
    }

    #[test]
    fn the_name_is_matched_whole_not_as_a_word_prefix() {
        let s = vec![ep(1, "Pilot"), ep(2, "Pilot Part 2"), ep(3, "Pi")];
        let named = |t: &str| own_episode(&find(t), &s).map(|e| e.number);
        assert_eq!(named("Episode 2: Pilot Part 2 previously"), Some(2));
        assert_eq!(named("Episode 1: Pilot and then"), Some(1));
        assert_eq!(named("Episode 9: Pizza night"), None);
    }

    /// Attack on Titan S2: no card of its own, and the narrator's preview at the end.
    #[test]
    fn a_next_episode_preview_places_the_title_before_it() {
        let text = "Let's move! Hange? Get me a horse. ... Ymir offers a plan of her \
            own. Next episode: \"Killing Magic.\"";
        let cards = find(text);
        assert_eq!(cards.len(), 1);
        assert!(cards[0].next && cards[0].number.is_none());
        assert_eq!(cards[0].describe(), "preview of the next episode");
        assert_eq!(own_episode(&cards, &season()).map(|e| e.number), Some(2));
    }

    #[test]
    fn the_titles_own_card_outranks_a_preview_and_a_first_episode_preview_places_nothing() {
        let both = find("\"Episode 3: Killing Magic\" ... Next episode: \"Killing Magic\"");
        assert_eq!(own_episode(&both, &season()).map(|e| e.number), Some(3));
        assert_eq!(first(&both).map(|c| c.describe()).as_deref(), Some("title card for episode 3"));
        let opener = find("Next episode: The Journey's End");
        assert_eq!(own_episode(&opener, &season()), None);
        // "Next episode 4: ..." is a preview even with its number.
        let numbered = find("Next episode 4: The Land Where Souls Rest");
        assert!(numbered[0].next);
        assert_eq!(own_episode(&numbered, &season()).map(|e| e.number), Some(3));
    }
}
