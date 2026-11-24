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
//!
//! A series with no subtitles at all can still draw its card on screen. Read by OCR
//! from the title's opening frames ([`from_picture`]), poster lettering comes back in
//! fragments — "BONDING" for "Parental Bonding", nothing at all for some — so such a
//! card is matched loosely against the season's names, and only a clear winner counts.

use crate::tmdb::Episode;
use std::collections::{HashMap, HashSet};

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
    /// Whether this was read from the picture rather than a caption: `following` then
    /// holds every word read from the opening frames, and the name is looked for
    /// loosely among them.
    pub picture: bool,
}

impl Card {
    /// What the card is, for the operator: `title card for episode 5`.
    pub fn describe(&self) -> String {
        if self.picture {
            return "words read from the opening frames".into();
        }
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
            picture: false,
        });
    }
    out
}

/// Whether a title is a director's or cast commentary over an episode, not an episode.
///
/// A commentary track captions its own credit at the start (`Commentary by: Executive
/// Director ...`), and then talks about the episode, naming it ("Episode 9, \"Ruler of
/// the Walls\"") in just the way a title card does, so left alone it matches that
/// episode as well as the real one.
pub fn is_commentary(text: &str) -> bool {
    let head: Vec<String> = words(text).into_iter().take(40).collect();
    head.windows(2).any(|w| w[0] == "commentary" && w[1] == "by")
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
    let own = cards.iter().filter(|c| !c.next && !c.picture && c.number.is_some());
    if let Some(e) = own.filter_map(|c| named(c, &names)).next() {
        return Some(e);
    }
    if let Some(e) = cards.iter().filter(|c| c.picture).find_map(|c| read_on_screen(c, episodes)) {
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

/// Words too common in episode names to tell one from another.
const COMMON: &[&str] = &["with", "from", "that", "this", "into", "part"];

/// A name word shorter than this is not looked for on screen: OCR makes three-letter
/// words out of any drawing.
const MIN_NAME_WORD: usize = 4;

/// How many of the opening frames a word must be read from. A card stays up for
/// seconds, so its words come back frame after frame; a misreading rarely repeats.
const MIN_FRAMES: usize = 2;

/// The share of an episode name's letters that must be read for the card to name it.
/// Poster lettering comes back in fragments: "BONDING" alone, for "Parental Bonding".
const MIN_SHARE: f64 = 0.4;

/// Letters of a name that must be read, however short the name.
const MIN_LETTERS: usize = 4;

/// The words found in the opening frames of every one of a disc's titles: the series'
/// own logo, its creator's credit, the set dressing of its opening. Read on every
/// title, they say nothing about which episode one is, and a name that shares one
/// ("Phantom Planet", on Danny Phantom) would otherwise match them all.
pub fn recurring_words(titles: &[Vec<String>]) -> HashSet<String> {
    let mut sets = titles
        .iter()
        .map(|frames| frames.iter().flat_map(|f| words(f)).collect::<HashSet<String>>());
    let Some(first) = sets.next() else {
        return HashSet::new();
    };
    sets.fold(first, |acc, s| acc.intersection(&s).cloned().collect())
}

/// A card read from the text of a title's opening frames, leaving out `recurring`
/// words. `None` when nothing else was read.
pub fn from_picture(frames: &[String], recurring: &HashSet<String>) -> Option<Card> {
    // Each frame's words once, so that how often a word occurs is how many frames
    // showed it.
    let mut read: Vec<String> = Vec::new();
    for f in frames {
        let mut seen = HashSet::new();
        for w in words(f) {
            if w.chars().count() >= MIN_NAME_WORD - 1 && !recurring.contains(&w) && seen.insert(w.clone())
            {
                read.push(w);
            }
        }
    }
    (!read.is_empty()).then(|| Card {
        number: None,
        following: read.join(" "),
        next: false,
        picture: true,
    })
}

/// The episode whose name a card read from the picture shows, if one clearly does.
///
/// Each name is scored by the share of its letters read, word by word, allowing for a
/// letter misread or lost at an edge. The best must cover enough of its name and score
/// at least twice the next, or nothing is claimed: a wrong card would outweigh every
/// other signal, so a miss is far the cheaper mistake.
fn read_on_screen<'a>(card: &Card, episodes: &'a [Episode]) -> Option<&'a Episode> {
    let mut frames: HashMap<&str, usize> = HashMap::new();
    for w in card.following.split(' ').filter(|w| !w.is_empty()) {
        *frames.entry(w).or_default() += 1;
    }
    let steady: Vec<&str> = frames
        .into_iter()
        .filter(|&(_, n)| n >= MIN_FRAMES)
        .map(|(w, _)| w)
        .collect();
    let mut scored: Vec<(f64, usize, &Episode)> = episodes
        .iter()
        .filter_map(|e| {
            let key: Vec<String> = words(&e.name)
                .into_iter()
                .filter(|w| w.chars().count() >= MIN_NAME_WORD && !COMMON.contains(&w.as_str()))
                .collect();
            let total: usize = key.iter().map(|w| w.chars().count()).sum();
            let found: usize = key
                .iter()
                .filter(|k| steady.iter().any(|w| close(w, k)))
                .map(|k| k.chars().count())
                .sum();
            (total > 0).then(|| (found as f64 / total as f64, found, e))
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let &(best, found, episode) = scored.first()?;
    let second = scored.get(1).map(|s| s.0).unwrap_or(0.0);
    (best >= MIN_SHARE && found >= MIN_LETTERS && second * 2.0 <= best).then_some(episode)
}

/// Whether OCR's `read` is the name word `name`: exactly, with one letter wrong, added
/// or lost, or as a fragment that lost up to two letters at an edge ("mage", "images").
fn close(read: &str, name: &str) -> bool {
    if read == name {
        return true;
    }
    let (r, n) = (read.chars().count(), name.chars().count());
    (n >= 5 && r + 1 >= n && r <= n + 1 && edit_distance(read, name) <= 1)
        || (r >= MIN_NAME_WORD && r < n && n - r <= 2 && name.contains(read))
}

/// Levenshtein distance between two words.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for j in 0..b.len() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != b[j])).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
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
    fn a_commentary_is_recognised_by_its_opening_credit() {
        assert!(is_commentary(
            "\"Commentary by: Executive Director Tetsuro Araki\" Hello. I'm Tetsuro Araki."
        ));
        // Dialogue that says the word later on is an episode.
        let later = format!("{} the commentary by the cast", "word ".repeat(60));
        assert!(!is_commentary(&later));
        assert!(!is_commentary("Let's move! Hange? Get me a horse."));
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

    fn phantom_season() -> Vec<Episode> {
        [
            "Mystery Meat",
            "Parental Bonding",
            "One of a Kind",
            "Attack of the Killer Garage Sale",
            "Splitting Images",
            "What You Want",
            "13",
        ]
        .iter()
        .enumerate()
        .map(|(i, n)| ep(i as u32 + 1, n))
        .chain(std::iter::once(Episode { season: 3, ..ep(13, "Phantom Planet") }))
        .collect()
    }

    /// The opening credits every Danny Phantom title shows, as OCR reads them.
    fn credits() -> Vec<String> {
        vec![
            "DANNY PHANTOM CREATED BY BUTCH HARTMAN".into(),
            "DANNY PHANTOM CREATED BY BUTCH HARTMAN".into(),
            "WRITTEN BY STEVE MARMEL STORYBOARD ART DIRECTION BOB BOYLE".into(),
            "MUSIC BY GUY MOON".into(),
        ]
    }

    /// Frames as Tesseract read Danny Phantom's cards: the poster lettering in fragments.
    fn title(card: &[&str]) -> Vec<String> {
        let mut frames = credits();
        frames.extend(card.iter().map(|s| s.to_string()));
        frames
    }

    fn read(frames: &[Vec<String>], i: usize) -> Option<u32> {
        let recurring = recurring_words(frames);
        let card = from_picture(&frames[i], &recurring)?;
        own_episode(&[card], &phantom_season()).map(|e| e.number)
    }

    #[test]
    fn a_card_read_in_fragments_names_its_episode() {
        let disc = vec![
            title(&["Vi I} aha MYSTERY", "LA MYSTERY Wola", "MYSTERY MEAT VISION"]),
            title(&["*arentaly, wo 5, BONDING ne", "DANNY PHANTOM ; Pre BONDING", "Grentall BONDING"]),
            title(&["ocKER {MAGE NG", "ookeR AGE MAGE"]),
            title(&["pA wy W) Wh aM", "TA lm"]),
        ];
        assert_eq!(read(&disc, 0), Some(1));
        assert_eq!(read(&disc, 1), Some(2));
        // "MAGE", twice: what was read of "Splitting Images".
        assert_eq!(read(&disc, 2), Some(5));
        // Nothing legible: no card, rather than a guess.
        assert_eq!(read(&disc, 3), None);
    }

    #[test]
    fn words_on_every_title_are_the_series_not_the_episode() {
        // "Phantom" is on every title's logo; left in, "Phantom Planet" would be half read
        // on every title of the series.
        let disc = vec![title(&["nothing legible"]), title(&["nor here"])];
        assert!(recurring_words(&disc).contains("phantom"));
        assert_eq!(read(&disc, 0), None);
        assert!(!recurring_words(&disc).contains("legible"));
    }

    #[test]
    fn a_word_read_from_a_single_frame_is_not_enough() {
        // A card stays up for seconds; one frame's misreading is not a card.
        let disc = vec![title(&["KIND"]), title(&["other"])];
        assert_eq!(read(&disc, 0), None);
        let held = vec![title(&["KIND", "ONE OF A KIND!"]), title(&["other"])];
        assert_eq!(read(&held, 0), Some(3));
    }

    #[test]
    fn two_names_read_equally_well_name_neither() {
        let disc = vec![title(&["MYSTERY BONDING", "MYSTERY BONDING"]), title(&["other"])];
        assert_eq!(read(&disc, 0), None);
    }

    #[test]
    fn a_caption_outranks_a_card_read_from_the_picture() {
        let picture = from_picture(&["MYSTERY".into(), "MYSTERY".into()], &HashSet::new()).unwrap();
        assert!(picture.picture);
        assert_eq!(picture.describe(), "words read from the opening frames");
        let caption = find("\"Episode 2: Parental Bonding\"");
        let both: Vec<Card> = caption.into_iter().chain(std::iter::once(picture)).collect();
        assert_eq!(own_episode(&both, &phantom_season()).map(|e| e.number), Some(2));
    }

    #[test]
    fn misread_letters_are_forgiven_but_not_other_words() {
        assert!(close("bondinq", "bonding"));
        assert!(close("mage", "images"));
        assert!(close("splittinq", "splitting"));
        assert!(!close("meat", "mystery"));
        assert!(!close("age", "images"));
        assert!(!close("kind", "kindred"));
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

}
