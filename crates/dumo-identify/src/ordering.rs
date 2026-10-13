//! Which title is which episode, among the episodes a disc was given.
//!
//! Disc order is not episode order. Scott Pilgrim Takes Off's one disc holds its eight
//! episodes as 5, 4, 8, 7, 2, 3, 6, 1, so naming its titles in disc order got all eight
//! wrong. So titles are assigned to episodes on what they say, and disc order only
//! settles what nothing else can:
//!
//! - **title cards** ([`crate::cards`]) name the episode outright;
//! - **distinctive dialogue** ([`signals::distinctive_fit`]): the words that set one
//!   title apart from the *others*, against each episode's name and synopsis;
//! - **runtime**, which separates episodes of a series only by a minute or two.
//!
//! Dialogue is not proof. Measured on that Scott Pilgrim disc it placed six of eight
//! titles; the pilot's synopsis ("sparks fly ... the girl of his dreams") fitted another
//! episode's dialogue better than the pilot's own, and those two were swapped with a
//! clear margin. On Frieren S1 disc 1 it placed all eight. So every title is also judged
//! for how well it is pinned down, and the doubtful ones are named for a person to look
//! at.

use crate::cards;
use crate::matching::DiscTitle;
use crate::signals;
use crate::tmdb::Episode;
use std::collections::HashSet;

/// A title card is the disc naming the episode: far past any other signal's range.
const CARD_COST: f64 = 100.0;

/// Cost per minute of runtime difference, in the dialogue's units of standard
/// deviations. Two minutes is about one deviation: enough to separate a 22 and a 26
/// minute episode, not enough to outvote what the dialogue says.
const RUNTIME_COST_PER_MIN: f64 = 0.5;

/// Cost per position away from disc order: only ever a tie-break.
const DISC_ORDER_COST: f64 = 0.01;

/// A title placed by dialogue fits its episode by at least this many standard
/// deviations, or its placement is doubtful.
const MIN_DIALOGUE_FIT: f64 = 1.0;

/// Two titles whose swap would cost less than this are not told apart.
const MIN_SWAP_COST: f64 = 1.0;

/// What placed a title where it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reason {
    /// Its own title card names the episode.
    Card,
    /// Its dialogue fits the episode this many deviations better than its others.
    Dialogue(f64),
    /// Its dialogue fits, but by less than [`MIN_DIALOGUE_FIT`]: the rest of the disc
    /// fitting best this way put it here as much as its own words did.
    WeakDialogue(f64),
    /// Its runtime, there being no dialogue.
    Runtime,
    /// No dialogue, and nothing else telling it apart from another title, so disc
    /// order decided.
    DiscOrder,
}

#[derive(Debug, Clone)]
pub struct Placed {
    /// `order[t]` is the window position title `t` is assigned to.
    pub order: Vec<usize>,
    pub reasons: Vec<Reason>,
    /// Titles whose placement is doubtful: a weak fit, or another title that would fit
    /// that episode about as well.
    pub doubtful: Vec<usize>,
    /// For each title, the others it could trade places with at little cost.
    pub rivals: Vec<Vec<usize>>,
}

impl Placed {
    /// Whether any title sits somewhere other than its disc-order position.
    pub fn moved(&self) -> bool {
        self.order.iter().enumerate().any(|(t, &e)| t != e)
    }
}

/// Assign `titles` to the episodes of `window`, one each.
///
/// `titles` and `window` are the same length, both in disc order, so that position `i`
/// of the window is where disc order would put title `i`. `season` is every candidate
/// episode, for reading title cards against. `prior(t, e)` is any further cost of
/// title `t` at window position `e`: infinite to forbid it outright, as the box set
/// solver does for a runtime that cannot fit.
pub fn place(
    titles: &[&DiscTitle],
    window: &[&Episode],
    season: &[Episode],
    prior: impl Fn(usize, usize) -> f64,
) -> Option<Placed> {
    let n = titles.len();
    if n == 0 || window.len() != n {
        return None;
    }

    let card: Vec<Option<usize>> = titles
        .iter()
        .map(|t| {
            let own = cards::own_episode(&t.cards, season)?;
            window
                .iter()
                .position(|e| e.season == own.season && e.number == own.number)
        })
        .collect();

    // Distinctive fit needs at least two titles to compare; titles without dialogue
    // simply get no dialogue term.
    let spoken: Vec<usize> = (0..n).filter(|&t| titles[t].dialogue.is_some()).collect();
    let mut z = vec![vec![0.0; n]; n];
    let mut has_z = vec![false; n];
    if spoken.len() >= 2 {
        let dialogue: Vec<_> = spoken
            .iter()
            .map(|&t| signals::stem_counts(titles[t].dialogue.as_ref().expect("spoken")))
            .collect();
        let references: Vec<HashSet<String>> = window
            .iter()
            .map(|e| signals::stem_all(&dumo_core::text::tokenize(&e.reference_text())))
            .collect();
        for (row, &t) in signals::distinctive_fit(&dialogue, &references)
            .into_iter()
            .zip(&spoken)
        {
            z[t] = row;
            has_z[t] = true;
        }
    }

    // The evidence for title t at position e, without the disc-order tie-break.
    let evidence = |t: usize, e: usize| -> f64 {
        let card_cost = match card[t] {
            Some(c) if c == e => -CARD_COST,
            Some(_) => CARD_COST,
            None => 0.0,
        };
        let runtime = window[e]
            .runtime_mins
            .map(|rt| (titles[t].duration_secs / 60.0 - f64::from(rt)).abs() * RUNTIME_COST_PER_MIN)
            .unwrap_or(0.0);
        card_cost - z[t][e] + runtime + prior(t, e)
    };
    let cost: Vec<Vec<f64>> = (0..n)
        .map(|t| {
            (0..n)
                .map(|e| evidence(t, e) + DISC_ORDER_COST * t.abs_diff(e) as f64)
                .collect()
        })
        .collect();
    let mut order = signals::assign_min_cost(&cost);
    if (0..n).any(|t| !cost[t][order[t]].is_finite()) {
        return None;
    }

    // Parts of one story say nearly the same words, so dialogue cannot order them;
    // disc order can. Each story's parts go back into disc order among its slots.
    let mut stories: std::collections::HashMap<&str, Vec<usize>> = Default::default();
    for t in 0..n {
        if card[t].is_none() {
            if let Some(story) = story_name(&window[order[t]].name) {
                stories.entry(story).or_default().push(t);
            }
        }
    }
    for ts in stories.values() {
        let mut slots: Vec<usize> = ts.iter().map(|&t| order[t]).collect();
        slots.sort_unstable();
        for (&t, slot) in ts.iter().zip(slots) {
            order[t] = slot;
        }
    }

    // How cheaply could each title trade places with another?
    let swap = |t: usize, u: usize| -> f64 {
        let (e, f) = (order[t], order[u]);
        evidence(t, f) + evidence(u, e) - evidence(t, e) - evidence(u, f)
    };
    let mut doubtful = Vec::new();
    let mut reasons = Vec::with_capacity(n);
    let mut rivals = Vec::with_capacity(n);
    for t in 0..n {
        let e = order[t];
        let close: Vec<usize> = (0..n)
            .filter(|&u| u != t && swap(t, u) < MIN_SWAP_COST)
            .collect();
        let reason = if card[t] == Some(e) {
            Reason::Card
        } else if has_z[t] && z[t][e] >= MIN_DIALOGUE_FIT {
            Reason::Dialogue(z[t][e])
        } else if has_z[t] {
            Reason::WeakDialogue(z[t][e])
        } else if close.is_empty() {
            Reason::Runtime
        } else {
            Reason::DiscOrder
        };
        if reason != Reason::Card
            && (!close.is_empty() || matches!(reason, Reason::WeakDialogue(_)))
        {
            doubtful.push(t);
        }
        reasons.push(reason);
        rivals.push(if reason == Reason::Card { Vec::new() } else { close });
    }

    Some(Placed {
        order,
        reasons,
        doubtful,
        rivals,
    })
}

/// The story an episode is one part of, if its name numbers the part.
///
/// Both conventions TMDB uses: a trailing "Ninja Quest (2)", and a "Green with Evil
/// Part 2: Jason's Battle" whose subtitle differs per part.
pub fn story_name(name: &str) -> Option<&str> {
    let is_number = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let t = name.trim_end();
    if let Some(inner) = t.strip_suffix(')') {
        if let Some(open) = inner.rfind('(') {
            if is_number(&inner[open + 1..]) {
                return Some(inner[..open].trim_end());
            }
        }
    }
    let at = t.find(" Part ")?;
    let number: String = t[at + 6..].chars().take_while(|c| c.is_ascii_digit()).collect();
    (!number.is_empty()).then(|| &t[..at])
}

/// One line per title that is not where disc order would have put it, or whose place
/// is in doubt, for the evidence a person reads.
pub fn describe(placed: &Placed, titles: &[&DiscTitle], window: &[&Episode]) -> Vec<String> {
    let mut out = Vec::new();
    for (t, title) in titles.iter().enumerate() {
        let e = window[placed.order[t]];
        let moved = placed.order[t] != t;
        let doubtful = placed.doubtful.contains(&t);
        if !moved && !doubtful {
            continue;
        }
        let why = match placed.reasons[t] {
            Reason::Card => "its title card names it".to_string(),
            Reason::Dialogue(z) => format!("its dialogue fits that episode best ({z:.1} sd)"),
            Reason::WeakDialogue(z) => format!(
                "its dialogue barely singles that episode out ({z:.1} sd); the others \
                 fitting best this way put it there"
            ),
            Reason::Runtime => "its runtime fits that episode best".to_string(),
            Reason::DiscOrder => "nothing tells it apart, so disc order decided".to_string(),
        };
        let rivals: Vec<&str> = placed.rivals[t]
            .iter()
            .map(|&u| titles[u].name.as_str())
            .collect();
        let doubt = match (doubtful, rivals.is_empty()) {
            (false, _) => String::new(),
            (true, true) => " — DOUBTFUL, check it".to_string(),
            (true, false) => format!(
                " — DOUBTFUL: could as well trade places with {}",
                rivals.join(", ")
            ),
        };
        out.push(format!(
            "  {} is S{:02}E{:02} {}: {why}{doubt}",
            title.name, e.season, e.number, e.name
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(n: u32, name: &str, plot: &str) -> Episode {
        Episode {
            season: 1,
            number: n,
            name: name.into(),
            runtime_mins: Some(25),
            air_date: None,
            overview: Some(plot.into()),
        }
    }

    fn season() -> Vec<Episode> {
        vec![
            ep(1, "Dragons", "A dragon attacks the village granary"),
            ep(2, "The Ferry", "The ferryman refuses the river crossing"),
            ep(3, "Snowbound", "A blizzard traps everyone in the mountain lodge"),
            ep(4, "The Duel", "A swordsman challenges the knight to a duel"),
            ep(5, "Thieves", "Thieves rob the merchant caravan at night"),
        ]
    }

    fn spoken(name: &str, words: &str) -> DiscTitle {
        let mut text = String::from("hello there friend we should go now ");
        for _ in 0..6 {
            text.push_str(words);
            text.push(' ');
        }
        DiscTitle {
            name: name.into(),
            duration_secs: 25.0 * 60.0,
            dialogue: Some(dumo_core::text::count_words(&text)),
            cards: Vec::new(),
        }
    }

    fn run(titles: &[DiscTitle]) -> Placed {
        let s = season();
        let refs: Vec<&DiscTitle> = titles.iter().collect();
        let window: Vec<&Episode> = s.iter().collect();
        place(&refs, &window, &s, |_, _| 0.0).expect("placed")
    }

    #[test]
    fn dialogue_places_titles_authored_out_of_order() {
        // Disc order 3, 1, 5, 2, 4.
        let titles = vec![
            spoken("t0", "blizzard lodge mountain snow"),
            spoken("t1", "dragon granary fire"),
            spoken("t2", "thieves caravan merchant"),
            spoken("t3", "ferryman river crossing"),
            spoken("t4", "swordsman duel knight"),
        ];
        let p = run(&titles);
        assert_eq!(p.order, vec![2, 0, 4, 1, 3]);
        assert!(p.moved());
        assert!(p.doubtful.is_empty(), "{:?}", p.reasons);
        assert!(p.reasons.iter().all(|r| matches!(r, Reason::Dialogue(_))));
    }

    #[test]
    fn titles_nothing_separates_are_doubtful_and_keep_disc_order() {
        let titles: Vec<DiscTitle> = (0..5)
            .map(|i| DiscTitle {
                name: format!("t{i}"),
                duration_secs: 25.0 * 60.0,
                dialogue: None,
                cards: Vec::new(),
            })
            .collect();
        let p = run(&titles);
        assert_eq!(p.order, vec![0, 1, 2, 3, 4]);
        assert!(!p.moved());
        assert_eq!(p.doubtful, vec![0, 1, 2, 3, 4]);
        assert!(p.reasons.iter().all(|r| *r == Reason::DiscOrder));
    }

    #[test]
    fn a_title_card_outranks_the_dialogue() {
        let mut titles = vec![
            spoken("t0", "dragon granary fire"),
            spoken("t1", "ferryman river crossing"),
            spoken("t2", "blizzard lodge mountain snow"),
            spoken("t3", "swordsman duel knight"),
            spoken("t4", "thieves caravan merchant"),
        ];
        // t0 talks about dragons, but its card says it is episode 2, and t1's says 1.
        titles[0].cards = cards::find("\"Episode 2: The Ferry\"");
        titles[1].cards = cards::find("\"Episode 1: Dragons\"");
        let p = run(&titles);
        assert_eq!(&p.order[..2], &[1, 0]);
        assert_eq!(p.reasons[0], Reason::Card);
        assert!(!p.doubtful.contains(&0) && !p.doubtful.contains(&1));
    }

    #[test]
    fn runtime_separates_what_dialogue_cannot() {
        let mut titles: Vec<DiscTitle> = (0..2)
            .map(|i| DiscTitle {
                name: format!("t{i}"),
                duration_secs: 0.0,
                dialogue: None,
                cards: Vec::new(),
            })
            .collect();
        titles[0].duration_secs = 44.0 * 60.0;
        titles[1].duration_secs = 22.0 * 60.0;
        let mut s = season();
        s[0].runtime_mins = Some(22);
        s[1].runtime_mins = Some(44);
        let refs: Vec<&DiscTitle> = titles.iter().collect();
        let window: Vec<&Episode> = s[..2].iter().collect();
        let p = place(&refs, &window, &s, |_, _| 0.0).unwrap();
        assert_eq!(p.order, vec![1, 0]);
        assert_eq!(p.reasons, vec![Reason::Runtime, Reason::Runtime]);
        assert!(p.doubtful.is_empty());
    }

    #[test]
    fn a_forbidden_pairing_is_never_made() {
        let titles = vec![
            spoken("t0", "dragon granary fire"),
            spoken("t1", "ferryman river crossing"),
        ];
        let s = season();
        let refs: Vec<&DiscTitle> = titles.iter().collect();
        let window: Vec<&Episode> = s[..2].iter().collect();
        // Dialogue says t0 is episode 1, but that pairing is ruled out.
        let p = place(&refs, &window, &s, |t, e| {
            if t == 0 && e == 0 {
                f64::INFINITY
            } else {
                0.0
            }
        })
        .unwrap();
        assert_eq!(p.order, vec![1, 0]);
    }

    #[test]
    fn story_names_are_read_from_both_conventions() {
        assert_eq!(story_name("Ninja Quest (2)"), Some("Ninja Quest"));
        assert_eq!(story_name("Green with Evil Part 2: Jason's Battle"), Some("Green with Evil"));
        assert_eq!(story_name("Dragons"), None);
    }
}
