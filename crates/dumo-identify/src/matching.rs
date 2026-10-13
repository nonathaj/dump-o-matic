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

use crate::signals;
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
    /// Whether dialogue chose between alignments that runtime could not separate.
    pub dialogue_decided: bool,
    /// Whether on-screen episode title cards chose between tied alignments.
    pub cards_decided: bool,
    /// How titles were assigned to the window's episodes, once that was decided.
    pub placement: Option<crate::ordering::Placed>,
    /// Every scored window: (mean, max, counted, start index).
    pub all_windows: Vec<(f64, f64, usize, usize)>,
    /// How many titles in the chosen window actually had a runtime to compare against.
    ///
    /// Kept because the mean is meaningless without it: TMDB often publishes no runtime
    /// for documentary episodes, and a "1.5 min average" over one title of three is a
    /// measurement of that one title, not of the window.
    pub counted: usize,
}

/// Whether enough of the window was actually measured to claim more than weak confidence.
///
/// Regression guard for a real overconfidence bug: a disc of three titles was reported as
/// a **strong** match to a season when two of the three episodes had no published runtime
/// and were never scored. The evidence line said so in as many words while the verdict
/// ignored it. One corroborating measurement is not corroboration.
pub fn enough_measured(counted: usize, titles: usize) -> bool {
    counted >= 2 && counted * 3 >= titles * 2
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
    /// Dialogue from the title's subtitle track, as word -> occurrences, when it has one.
    ///
    /// Far more discriminating than a runtime: two episodes of a series routinely run to
    /// the same minute, but they do not say the same words. `None` means the title had
    /// no text subtitle track, and scoring falls back to runtime alone.
    pub dialogue: Option<std::collections::HashMap<String, u32>>,
    /// "Episode N: Name" captions in the subtitles, in order; see [`crate::cards`].
    pub cards: Vec<crate::cards::Card>,
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
/// Titles are paired with the window's episodes by best runtime fit, not by position:
/// disc order is not episode order, and comparing title 3 with the window's third
/// episode on a disc authored 5, 4, 8, 7, ... compares the wrong pairs.
///
/// Episodes without runtime data are skipped rather than counted as a perfect or
/// terrible match; TMDB omits runtime often enough that treating a missing value as
/// zero would invent confidence that isn't there.
fn score(titles: &[DiscTitle], episodes: &[Episode]) -> Option<(f64, f64, usize)> {
    let k = titles.len().min(episodes.len());
    let delta = |t: &DiscTitle, e: &Episode| e.runtime_mins.map(|rt| (t.mins() - f64::from(rt)).abs());
    let cost: Vec<Vec<f64>> = titles[..k]
        .iter()
        .map(|t| episodes[..k].iter().map(|e| delta(t, e).unwrap_or(0.0)).collect())
        .collect();
    let pairing = crate::signals::assign_min_cost(&cost);

    let mut total = 0.0;
    let mut worst: f64 = 0.0;
    let mut counted = 0usize;

    for (t, e) in titles[..k].iter().zip(pairing.iter().map(|&p| &episodes[p])) {
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
///
/// Half a minute per episode: TMDB lists whole minutes, so a smaller difference is
/// rounding. It was a quarter while titles were compared by position, where a
/// misaligned window scored badly anyway; paired by best fit, every window scores
/// closer, and ESPN 30 for 30 disc 1 (episodes 1-3) lost to 2-4 by 0.27 min/episode,
/// which said nothing and kept the label's "DISC_1" from deciding.
const TIE_EPSILON_MINS: f64 = 0.5;

/// Split a token that glues season and disc together, `S3D1`, into `S3 D1`, the form
/// the label parsers below already understand. Expects separators already turned into
/// spaces, so each caller keeps its own idea of what separates words. Labels are written
/// both ways:
/// `MMPR_S1_D1` on one box set and `MMPR_S3D1` on the next season's.
fn split_season_disc(label: &str) -> String {
    label
        .split_whitespace()
        .map(|w| {
            let up = w.to_ascii_uppercase();
            let parts = up.strip_prefix('S').and_then(|rest| {
                let (season, disc) = rest.split_once('D')?;
                let digits = |x: &str| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit());
                (digits(season) && digits(disc)).then(|| format!("S{season} D{disc}"))
            });
            parts.unwrap_or_else(|| w.to_string())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract the disc number from a volume label, e.g. `ESPN_30_FOR_30_DISC_2` → 2.
///
/// Worth doing because runtimes frequently tie. A three-episode disc labelled DISC 2 in
/// a set almost certainly holds the episodes after DISC 1's, and that ordering resolves
/// ties that runtime cannot.
pub fn disc_number_from_label(label: &str) -> Option<u32> {
    let spaced = split_season_disc(&label.replace(['_', '.', '-'], " ")).to_ascii_uppercase();
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

/// The season number named in a disc's volume label, if it names one.
///
/// The publisher printed this on the disc, which makes it far better evidence than a
/// runtime fit: episode runtimes across seasons of the same show differ by seconds,
/// while the label is an explicit statement of what the disc holds. A Wonder Woman
/// season 3 disc was matched to season 2 because season 2's runtimes fitted by
/// 0.3 min/episode more — 18 seconds of noise overriding the printed answer.
///
/// Deliberately conservative about what counts as a season number. `SEASON_3` and `S02`
/// are unambiguous; a bare number is not, and neither is the `3` in `WONDER_WOMAN_3`,
/// which could be a sequel or a disc index.
pub fn season_number_from_label(label: &str) -> Option<u32> {
    let spaced = split_season_disc(&label.replace(['_', '.', '-'], " ")).to_ascii_uppercase();
    let words: Vec<&str> = spaced.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if matches!(*w, "SEASON" | "SERIES") {
            if let Some(n) = words.get(i + 1).and_then(|n| n.parse::<u32>().ok()) {
                return Some(n);
            }
        }
        // "S02" / "S2" / "S1P2", but not "S" alone and not the "S" of a word like
        // "SEASON".
        if let Some((season, _)) = season_token(w) {
            return Some(season);
        }
    }
    None
}

/// A season glued into one token, with the part of it if one is given: `S05` is
/// season 5, `S1P2` is season 1 part 2. Frieren's season 1 came as two box sets,
/// `FRIEREN S1P1 D1` .. and `FRIEREN S1P2 D1` ...
fn season_token(word: &str) -> Option<(u32, Option<u32>)> {
    let up = word.to_ascii_uppercase();
    let rest = up.strip_prefix('S')?;
    let digits = |x: &str| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit());
    match rest.split_once('P') {
        Some((season, part)) if digits(season) && digits(part) => {
            Some((season.parse().ok()?, Some(part.parse().ok()?)))
        }
        None if digits(rest) => Some((rest.parse().ok()?, None)),
        _ => None,
    }
}

/// Which part of a season a disc's label says it belongs to, as in `S1P2` or `PART 2`.
///
/// A season split across box sets numbers each set's discs from 1, so "disc 1" of part
/// 2 is not where the season starts. Callers use this to stop trusting the disc number
/// as a position.
pub fn part_number_from_label(label: &str) -> Option<u32> {
    let spaced = label.replace(['_', '.', '-'], " ").to_ascii_uppercase();
    let words: Vec<&str> = spaced.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if matches!(*w, "PART" | "PT") {
            if let Some(n) = words.get(i + 1).and_then(|n| n.parse::<u32>().ok()) {
                return Some(n);
            }
        }
        if let Some((_, Some(part))) = season_token(w) {
            return Some(part);
        }
    }
    None
}

/// What to search a metadata provider for, given a label's [`query_from_label`].
///
/// The query keeps a trailing season token such as `s05` or `s1p1` on purpose: it is
/// also the key that groups a box set's discs, and two seasons, or two parts of one,
/// must not be grouped as one set. A search engine only sees an unknown word in it, and
/// "frieren s1p1" found nothing. The season itself is read from the label separately.
pub fn search_terms(query: &str) -> String {
    let mut words: Vec<&str> = query.split_whitespace().collect();
    while words.len() > 1 && words.last().map(|w| season_token(w).is_some()).unwrap_or(false) {
        words.pop();
    }
    words.join(" ")
}

/// Smallest dialogue-score margin that counts as a decision.
///
/// Dialogue overlap on a 15-minute window is noisy; a hair's-breadth lead is not a
/// finding. Below this the alignments are treated as unresolved by dialogue and the
/// weaker disc-number signal still gets its turn.
const DIALOGUE_MARGIN: f64 = 0.02;

/// Pick the tied alignment whose episodes the dialogue actually matches.
///
/// Runtime ties constantly between alignments — episodes of one series run to the same
/// minute — and a disc number only says where a disc sits in a box, which is an
/// assumption about packing rather than evidence about content. The words spoken are
/// evidence about content, so they get to decide first.
///
/// Returns the winning window start and its margin over the runner-up, or `None` when
/// no title has usable dialogue or the scores are too close to separate.
fn best_window_by_dialogue(
    titles: &[DiscTitle],
    season: &Season,
    starts: &[usize],
) -> Option<(usize, f64)> {
    if titles.iter().all(|t| t.dialogue.is_none()) {
        return None;
    }
    let references: Vec<std::collections::HashSet<String>> = season
        .episodes
        .iter()
        .map(|e| signals::stem_all(&dumo_core::text::tokenize(&e.reference_text())))
        .collect();
    let corpus = signals::Corpus::from_references(references.iter());
    let dialogue: Vec<Option<std::collections::HashSet<String>>> = titles
        .iter()
        .map(|t| t.dialogue.as_ref().map(|d| signals::stem_all(d.keys())))
        .collect();

    let mut scored: Vec<(usize, f64)> = Vec::new();
    for &start in starts {
        if start + titles.len() > season.episodes.len() {
            continue;
        }
        // Paired by best fit within the window, not by position: disc order is not
        // episode order.
        let k = titles.len();
        let fit: Vec<Vec<f64>> = dialogue
            .iter()
            .map(|d| {
                (0..k)
                    .map(|e| match d {
                        Some(d) => signals::subtitle_score_stemmed(d, &references[start + e], &corpus),
                        None => 0.0,
                    })
                    .collect()
            })
            .collect();
        let cost: Vec<Vec<f64>> = fit.iter().map(|r| r.iter().map(|v| -v).collect()).collect();
        let pairing = signals::assign_min_cost(&cost);
        let mut total = 0.0;
        let mut counted = 0usize;
        for (i, d) in dialogue.iter().enumerate() {
            if d.is_some() {
                total += fit[i][pairing[i]];
                counted += 1;
            }
        }
        if counted > 0 {
            scored.push((start, total / counted as f64));
        }
    }
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let (best, best_score) = scored[0];
    let margin = match scored.get(1) {
        Some((_, second)) => best_score - second,
        // Only one alignment could be scored, so nothing competed with it.
        None => f64::INFINITY,
    };
    (margin >= DIALOGUE_MARGIN).then_some((best, margin))
}

/// Find the best contiguous run of episodes matching these titles.
///
/// `titles` must already be in disc order. `disc_hint` is the disc's number within its
/// set, used only to break ties that runtime cannot.
///
/// Any tie-break that relocates the window must call [`SeasonMatch::restate_window`],
/// since the summary evidence is built from the runtime-best window and would otherwise
/// keep naming episodes that are no longer being proposed.
impl SeasonMatch {
    /// Rewrite the summary line after a tie-break moved the window.
    fn restate_window(&mut self, season_number: u32, k: usize) {
        if let Some(first) = self.evidence.first_mut() {
            *first = format!(
                "matched {k} title(s) against season {season_number} episodes {}–{}",
                self.first_episode,
                self.first_episode + k as u32 - 1
            );
        }
    }
}

pub fn match_season_with_hint(
    titles: &[DiscTitle],
    season: &Season,
    disc_hint: Option<u32>,
) -> Option<SeasonMatch> {
    let mut m = match_season(titles, season)?;

    // Only intervene where runtime genuinely cannot decide.
    let k = titles.len();
    // Owned rather than borrowed: the tie-breaks below mutate `m`, and holding
    // references into m.all_windows across that is not expressible.
    let tied: Vec<(f64, f64, usize, usize)> = m
        .all_windows
        .iter()
        .filter(|w| (w.0 - m.mean_delta).abs() <= TIE_EPSILON_MINS)
        .copied()
        .collect();

    if tied.len() > 1 {
        m.tied_alternatives = tied
            .iter()
            .map(|w| season.episodes[w.3].number)
            .filter(|n| *n != m.first_episode)
            .collect();

        // Dialogue first: it is evidence about what is on the disc, where a disc number
        // is only an assumption about how the box was packed. A double-sided disc breaks
        // that assumption outright — both sides are "disc 1" — so the packing signal can
        // be confidently wrong in a way dialogue is not.
        let tied_starts: Vec<usize> = tied.iter().map(|w| w.3).collect();
        // Title cards before either: a caption naming the episode is the disc saying
        // outright what a synopsis or a disc number can only suggest.
        if let Some((start, agree)) = best_window_by_cards(titles, season, &tied_starts) {
            let window = &season.episodes[start..start + k];
            m.first_episode = window.first().map(|e| e.number).unwrap_or(0);
            if let Some(w) = tied.iter().find(|w| w.3 == start) {
                m.mean_delta = w.0;
                m.max_delta = w.1;
            }
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
            m.cards_decided = true;
            m.restate_window(season.number, k);
            m.evidence.push(format!(
                "runtimes tie between {} alignments; on-screen title cards name the \
                 episode for {agree} title(s), and only episodes {}-{} agree with all of them",
                tied.len(),
                m.first_episode,
                m.first_episode + k as u32 - 1,
            ));
        } else if let Some((start, margin)) = best_window_by_dialogue(titles, season, &tied_starts) {
            let window = &season.episodes[start..start + k];
            m.first_episode = window.first().map(|e| e.number).unwrap_or(0);
            if let Some(w) = tied.iter().find(|w| w.3 == start) {
                m.mean_delta = w.0;
                m.max_delta = w.1;
            }
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
            m.dialogue_decided = true;
            m.restate_window(season.number, k);
            m.evidence.push(format!(
                "runtimes tie between {} alignments; the dialogue matches episodes {}-{} \
                 best, by {margin:.2} over the next alignment",
                tied.len(),
                m.first_episode,
                m.first_episode + k as u32 - 1,
            ));
        } else if let Some(disc) = disc_hint {
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
                m.restate_window(season.number, k);
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
    place_titles(&mut m, titles, season);
    check_against_cards(&mut m, titles, season);
    Some(m)
}

/// What the title cards say about one window of episodes: (agreeing, disagreeing)
/// titles. A card agrees when it names an episode in the window, wherever: disc order
/// is not episode order, so which title is which is settled afterwards.
fn card_votes(titles: &[DiscTitle], season: &Season, start: usize) -> (usize, usize) {
    let end = (start + titles.len()).min(season.episodes.len());
    let window = &season.episodes[start..end];
    titles.iter().fold((0, 0), |(agree, disagree), t| {
        match crate::cards::own_episode(&t.cards, &season.episodes) {
            Some(own) if window.iter().any(|e| e.number == own.number) => (agree + 1, disagree),
            Some(_) => (agree, disagree + 1),
            None => (agree, disagree),
        }
    })
}

/// Decide which title is which episode within the chosen window; see
/// [`crate::ordering`]. Until here every match pairs titles and episodes by position,
/// which was only ever a placeholder.
fn place_titles(m: &mut SeasonMatch, titles: &[DiscTitle], season: &Season) {
    let k = titles.len();
    let Some(start) = season
        .episodes
        .iter()
        .position(|e| e.number == m.first_episode)
    else {
        return;
    };
    if start + k > season.episodes.len() {
        return;
    }
    let refs: Vec<&DiscTitle> = titles.iter().collect();
    let window: Vec<&Episode> = season.episodes[start..start + k].iter().collect();
    let Some(placed) = crate::ordering::place(&refs, &window, &season.episodes, |_, _| 0.0)
    else {
        return;
    };

    m.matches = titles
        .iter()
        .zip(placed.order.iter().map(|&e| window[e]))
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
    let measured: Vec<f64> = m
        .matches
        .iter()
        .map(|x| x.delta_mins)
        .filter(|d| !d.is_nan())
        .collect();
    if !measured.is_empty() {
        m.mean_delta = measured.iter().sum::<f64>() / measured.len() as f64;
        m.max_delta = measured.iter().cloned().fold(0.0, f64::max);
    }

    let moved = (0..k).filter(|&t| placed.order[t] != t).count();
    if moved > 0 {
        m.evidence.push(format!(
            "{moved} of {k} title(s) are not in episode order on the disc; each was placed \
             on what it says:"
        ));
    } else if !placed.doubtful.is_empty() {
        m.evidence.push("how the doubtful titles were placed:".to_string());
    }
    m.evidence
        .extend(crate::ordering::describe(&placed, &refs, &window));
    if !placed.doubtful.is_empty() {
        m.evidence.push(format!(
            "{} title(s) are placed with doubt — check them before filing",
            placed.doubtful.len()
        ));
        m.confidence = Confidence::Weak;
    }
    m.placement = Some(placed);
}

/// The tied alignment the title cards support, with how many titles' cards agree.
///
/// Only an alignment no card contradicts can win, and it must be the only one with
/// that much support; otherwise the cards decide nothing and the weaker signals get
/// their turn.
fn best_window_by_cards(
    titles: &[DiscTitle],
    season: &Season,
    starts: &[usize],
) -> Option<(usize, usize)> {
    let k = titles.len();
    let votes: Vec<(usize, usize, usize)> = starts
        .iter()
        .filter(|&&s| s + k <= season.episodes.len())
        .map(|&s| {
            let (agree, disagree) = card_votes(titles, season, s);
            (s, agree, disagree)
        })
        .collect();
    let &(start, agree, _) = votes
        .iter()
        .filter(|v| v.2 == 0 && v.1 > 0)
        .max_by_key(|v| v.1)?;
    let rivals = votes.iter().filter(|v| v.0 != start && v.1 >= agree).count();
    (rivals == 0).then_some((start, agree))
}

/// Hold the chosen alignment up against the title cards, whatever chose it.
///
/// A card naming another episode than the one proposed means the proposal is wrong
/// somewhere, so it can be no better than weak and the evidence says which title. A
/// card on every title, each naming the proposed episode, is as close to proof as
/// inference gets, and makes it strong — still inference, so still confirmed by hand.
fn check_against_cards(m: &mut SeasonMatch, titles: &[DiscTitle], season: &Season) {
    let mut agree = 0usize;
    let mut wrong: Vec<String> = Vec::new();
    for (t, proposed) in titles.iter().zip(m.matches.iter().map(|x| &x.episode)) {
        let Some(own) = crate::cards::own_episode(&t.cards, &season.episodes) else {
            continue;
        };
        if own.number == proposed.number {
            agree += 1;
        } else {
            wrong.push(format!(
                "{} is captioned as E{:02} \"{}\", not E{:02}",
                t.name, own.number, own.name, proposed.number
            ));
        }
    }
    if !wrong.is_empty() {
        m.evidence.push(format!(
            "title cards contradict this for {} title(s): {}",
            wrong.len(),
            wrong.join("; ")
        ));
        m.confidence = Confidence::Weak;
    } else if agree == titles.len() {
        // The runtime summary's advice to check the episode titles has been followed.
        m.evidence
            .retain(|e| !(e.starts_with("next-best") && e.ends_with("check the episode titles")));
        m.evidence.push(format!(
            "every title's on-screen title card names the episode proposed for it ({agree} of {agree})"
        ));
        m.confidence = Confidence::Strong;
    } else if agree > 0 {
        m.evidence.push(format!(
            "{agree} of {} titles carry a title card, each naming the episode proposed for it",
            titles.len()
        ));
    }
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
    let confidence = if enough_measured(counted, k)
        && mean_delta <= 2.0
        && margin.map(|m| m >= 2.0).unwrap_or(true)
    {
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
            "{} of {k} episodes had no runtime in TMDB and were not scored{}",
            k - counted,
            if enough_measured(counted, k) {
                ""
            } else {
                " — too little of this window was measured to claim more than weak \
                 confidence, whatever the average says"
            }
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
        counted,
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
        dialogue_decided: false,
        cards_decided: false,
        placement: None,
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
        let measured = enough_measured(out[0].counted, out[0].matches.len());
        out[0].confidence = if measured && best <= 2.0 && margin >= 2.0 {
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
    let spaced = split_season_disc(&label.replace(['_', '.'], " "));
    let words: Vec<&str> = spaced.split_whitespace().collect();

    let mut end = words.len();
    // Strip a trailing "DISC 1", "D2", "SEASON 3", "VOL 2" and similar, including a glued
    // disc form like "D1" (longest marker first, so "DISC1" doesn't stop at "D1"). A glued
    // season form like "S05" is deliberately left alone: unlike "SEASON", a bare "S" is too
    // easily something else, and season_number_from_label still gets to read it later.
    let marker = ["DISC", "DISK", "SEASON", "VOL", "VOLUME", "SET", "S"];
    let glued_disc_marker = ["DISC", "DISK", "D"];
    while end >= 1 {
        let last = words[end - 1].to_ascii_uppercase();
        let prev = if end >= 2 {
            words[end - 2].to_ascii_uppercase()
        } else {
            String::new()
        };
        let is_number = last.chars().all(|c| c.is_ascii_digit());
        let glued = glued_disc_marker
            .iter()
            .filter(|m| last.len() > m.len())
            .find(|m| last.starts_with(**m) && last[m.len()..].chars().all(|c| c.is_ascii_digit()));
        if is_number && marker.contains(&prev.as_str()) {
            end -= 2;
        } else if marker.contains(&last.as_str()) || glued.is_some() {
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
            cards: Vec::new(),
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
        // Paired by best fit rather than by position, episodes 2-4 fit these runtimes
        // 0.13 min/episode better than 1-3: noise, and reported as a tie. The label's
        // disc number is what places the real disc, labelled DISC_1.
        let m = match_season(&titles, &season_one()).expect("matched");
        assert!(m.tied_alternatives.contains(&1) || m.first_episode == 1, "{:?}", m.evidence);
        let m = match_season_with_hint(&titles, &season_one(), Some(1)).expect("matched");
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

    /// Nine 25-minute episodes, so runtime ties everywhere; titles captioned like
    /// Frieren's, each with its own card and the next episode's preview card.
    fn carded(first: u32, count: u32) -> (Season, Vec<DiscTitle>) {
        let names = [
            "The Journey's End", "It Didn't Have to Be Magic...", "Killing Magic",
            "The Land Where Souls Rest", "Phantoms of the Dead", "The Hero of the Village",
            "Like a Fairy Tale", "Frieren the Slayer", "Aura the Guillotine",
        ];
        let episodes: Vec<Episode> = (1..=9)
            .map(|n| Episode {
                season: 1,
                number: n,
                name: names[n as usize - 1].into(),
                runtime_mins: Some(25),
                air_date: None,
                overview: None,
            })
            .collect();
        let titles = (first..first + count)
            .map(|n| {
                let mut text = format!("\"Episode {n}: {}\" dialogue", names[n as usize - 1]);
                if let Some(next) = names.get(n as usize) {
                    text += &format!(" \"Episode {} {next}\"", n + 1);
                }
                DiscTitle {
                    name: format!("t{:02}.mkv", n - first),
                    duration_secs: 25.0 * 60.0,
                    dialogue: None,
                    cards: crate::cards::find(&text),
                }
            })
            .collect();
        (Season { number: 1, episodes }, titles)
    }

    #[test]
    fn title_cards_break_a_tie_and_outrank_the_disc_number() {
        // A "disc 1" holding episodes 3-6: the disc number would say 1-4. The preview
        // cards also name 4-7, which must not make the window one later score as well.
        let (season, titles) = carded(3, 4);
        let m = match_season_with_hint(&titles, &season, Some(1)).expect("matched");
        assert_eq!(m.first_episode, 3, "{:?}", m.evidence);
        assert!(m.cards_decided);
        assert!(!m.disc_hint_used && !m.dialogue_decided);
        assert_eq!(m.confidence, Confidence::Strong, "{:?}", m.evidence);
        assert!(m.evidence[0].contains("episodes 3"), "{:?}", m.evidence[0]);
        assert!(
            !m.evidence.iter().any(|e| e.contains("check the episode titles")),
            "the cards already did: {:?}",
            m.evidence
        );
    }

    #[test]
    fn a_title_card_naming_another_episode_keeps_the_match_weak() {
        let (season, mut titles) = carded(1, 4);
        // Swap two titles' cards: the alignment by runtime can no longer be right.
        let c = titles[1].cards.clone();
        titles[1].cards = titles[2].cards.clone();
        titles[2].cards = c;
        let mut m = match_season(&titles, &season).expect("matched");
        m.confidence = Confidence::Strong;
        check_against_cards(&mut m, &titles, &season);
        assert_eq!(m.confidence, Confidence::Weak);
        let said = m.evidence.last().unwrap();
        assert!(said.contains("contradict") && said.contains("t01.mkv"), "{said}");
    }

    #[test]
    fn dialogue_breaks_a_runtime_tie_and_outranks_the_disc_number() {
        // Every episode runs 47 min, so runtime ties across the whole season and the
        // disc-number signal would put a "disc 1" at episodes 1-3. The dialogue points
        // at episodes 4-6 instead — the real case of a double-sided disc, where both
        // sides are labelled disc 1 and the packing assumption is simply wrong.
        let episodes: Vec<Episode> = (1..=9)
            .map(|n| Episode {
                season: 3,
                number: n,
                name: format!("Episode {n}"),
                runtime_mins: Some(47),
                air_date: None,
                overview: Some(match n {
                    4 => "A forger steals a priceless painting from the gallery".into(),
                    5 => "A disco owner hypnotises dancers with a strange record".into(),
                    6 => "An entomologist controls ants to attack the chemical plant".into(),
                    _ => format!("Unrelated filler plot number {n}"),
                }),
            })
            .collect();
        let season = Season { number: 3, episodes };

        let titles = vec![
            DiscTitle {
                name: "a.mkv".into(),
                duration_secs: 47.0 * 60.0,
                dialogue: Some(dumo_core::text::count_words(
                    "the forger stole a priceless painting from the gallery",
                )),
                cards: Vec::new(),
            },
            DiscTitle {
                name: "b.mkv".into(),
                duration_secs: 47.0 * 60.0,
                dialogue: Some(dumo_core::text::count_words(
                    "the disco owner hypnotises dancers with that record",
                )),
                cards: Vec::new(),
            },
            DiscTitle {
                name: "c.mkv".into(),
                duration_secs: 47.0 * 60.0,
                dialogue: Some(dumo_core::text::count_words(
                    "an entomologist controls ants attacking the chemical plant",
                )),
                cards: Vec::new(),
            },
        ];

        let m = match_season_with_hint(&titles, &season, Some(1)).expect("matched");
        assert_eq!(m.first_episode, 4, "{:?}", m.evidence);
        assert!(m.dialogue_decided);
        // The disc number must not have been consulted once dialogue decided.
        assert!(!m.disc_hint_used);
        // The summary line must name the episodes actually proposed.
        assert!(
            m.evidence[0].contains("episodes 4"),
            "stale summary: {:?}",
            m.evidence[0]
        );
    }

    #[test]
    fn the_disc_number_still_decides_when_there_is_no_dialogue() {
        let episodes: Vec<Episode> = (1..=9)
            .map(|n| Episode {
                season: 3,
                number: n,
                name: format!("Episode {n}"),
                runtime_mins: Some(47),
                air_date: None,
                overview: None,
            })
            .collect();
        let season = Season { number: 3, episodes };
        let titles: Vec<DiscTitle> = ["a.mkv", "b.mkv", "c.mkv"]
            .iter()
            .map(|n| DiscTitle {
                name: (*n).into(),
                duration_secs: 47.0 * 60.0,
                dialogue: None,
                cards: Vec::new(),
            })
            .collect();

        let m = match_season_with_hint(&titles, &season, Some(2)).expect("matched");
        assert!(m.disc_hint_used);
        assert!(!m.dialogue_decided);
        assert_eq!(m.first_episode, 4);
        assert!(m.evidence[0].contains("episodes 4"), "{:?}", m.evidence[0]);
    }

    #[test]
    fn reads_a_season_number_from_a_label() {
        assert_eq!(season_number_from_label("WONDER_WOMAN_SEASON_3"), Some(3));
        assert_eq!(season_number_from_label("THE_WIRE_S01_D3"), Some(1));
        assert_eq!(season_number_from_label("SHOW_SERIES_2_DISC_1"), Some(2));
        assert_eq!(season_number_from_label("BLACKADDER_S02"), Some(2));
    }

    #[test]
    fn does_not_invent_a_season_from_an_ambiguous_number() {
        // A sequel or a disc index, not a season.
        assert_eq!(season_number_from_label("WONDER_WOMAN_3"), None);
        assert_eq!(season_number_from_label("ROCKY_II"), None);
        assert_eq!(season_number_from_label("MOVIE_TITLE"), None);
        // "DISC 2" must not be read as a season.
        assert_eq!(season_number_from_label("ESPN_30_FOR_30_DISC_2"), None);
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

    /// Regression test: a glued disc marker like "D1" used to survive stripping because
    /// only separate-token "DISC 1" form was recognised, so six discs of one box set
    /// ("MMPR_S1_D1" .. "MMPR_S1_D6") each produced a distinct series key and the set
    /// could never be solved jointly.
    /// Season 3 of the same box set glues season and disc into one token.
    #[test]
    fn reads_a_season_and_disc_glued_into_one_token() {
        for n in 1..=6 {
            let label = format!("MMPR_S3D{n}");
            assert_eq!(query_from_label(&label), "mmpr s3");
            assert_eq!(disc_number_from_label(&label), Some(n));
            assert_eq!(season_number_from_label(&label), Some(3));
        }
        // Words that merely start with S and contain a D are left alone.
        assert_eq!(query_from_label("SHADOWS_D2"), "shadows");
        assert_eq!(season_number_from_label("SHADOWS_D2"), None);
        // A hyphen stays part of the title in a search query, as it always has.
        assert_eq!(query_from_label("SPIDER-MAN_D1"), "spider-man");
    }

    #[test]
    fn searches_without_the_season_token_the_set_key_keeps() {
        let key = query_from_label("FRIEREN S1P1 D1");
        assert_eq!(key, "frieren s1p1");
        assert_ne!(key, query_from_label("FRIEREN S1P2 D1"), "two parts, two sets");
        assert_eq!(search_terms(&key), "frieren");
        assert_eq!(search_terms(&query_from_label("FRIENDS_S05_DISC_3")), "friends");
        assert_eq!(search_terms(&query_from_label("MMPR_S3D1")), "mmpr");
        // Nothing else is touched, and a label that is only a season keeps it.
        assert_eq!(search_terms("espn 30 for 30"), "espn 30 for 30");
        assert_eq!(search_terms("shadows"), "shadows");
        assert_eq!(search_terms("s2"), "s2");
    }

    #[test]
    fn reads_a_season_and_part_glued_into_one_token() {
        assert_eq!(season_number_from_label("FRIEREN S1P2 D1"), Some(1));
        assert_eq!(part_number_from_label("FRIEREN S1P2 D1"), Some(2));
        assert_eq!(disc_number_from_label("FRIEREN S1P2 D1"), Some(1));
        assert_eq!(part_number_from_label("SHOW_SEASON_2_PART_3_DISC_1"), Some(3));
        assert_eq!(part_number_from_label("FRIENDS_S05_DISC_3"), None);
        assert_eq!(part_number_from_label("SPIDER-MAN_D1"), None);
    }

    #[test]
    fn strips_a_glued_disc_number_so_a_box_sets_discs_share_one_key() {
        for n in 1..=6 {
            assert_eq!(query_from_label(&format!("MMPR_S1_D{n}")), "mmpr s1");
        }
    }

    #[test]
    fn query_from_label_handles_odd_input() {
        assert_eq!(query_from_label(""), "");
        assert_eq!(query_from_label("DISC_1"), "");
    }

    /// Regression test for a real overconfidence bug.
    ///
    /// A disc of three titles was reported as a **strong** match to a season where two of
    /// the three episodes had no published runtime and were never scored. The "1.5 min
    /// average" was a measurement of one title, and the evidence line even said two were
    /// unscored — while the verdict ignored it. On real data the answer was also wrong:
    /// the disc belonged to season 1 and it chose season 3.
    #[test]
    fn a_mostly_unmeasured_window_is_never_strong() {
        let season = Season {
            number: 3,
            episodes: vec![
                Episode { season: 3, number: 27, name: "Seau".into(), runtime_mins: None, air_date: None, overview: None },
                Episode { season: 3, number: 28, name: "42 to 1".into(), runtime_mins: None, air_date: None, overview: None },
                Episode { season: 3, number: 29, name: "Deion's Double Play".into(), runtime_mins: Some(50), air_date: None, overview: None },
            ],
        };
        let titles = vec![title("a.mkv", 17.0), title("b.mkv", 104.0), title("c.mkv", 52.0)];

        let m = match_season(&titles, &season).expect("matched");
        assert_eq!(m.counted, 1, "only one episode had a runtime to compare");
        assert_eq!(
            m.confidence,
            Confidence::Weak,
            "one measurement out of three cannot be strong"
        );
        assert!(
            m.evidence.iter().any(|e| e.contains("too little of this window was measured")),
            "the report must explain the downgrade: {:?}",
            m.evidence
        );
    }

    #[test]
    fn measurement_coverage_thresholds() {
        // A single measurement never suffices, however many titles there are.
        assert!(!enough_measured(1, 1));
        assert!(!enough_measured(1, 3));
        // Two of three is the boundary and counts.
        assert!(enough_measured(2, 3));
        assert!(enough_measured(3, 3));
        // One of two does not.
        assert!(!enough_measured(1, 2));
        assert!(enough_measured(2, 2));
        // Two of five does not.
        assert!(!enough_measured(2, 5));
        assert!(enough_measured(4, 5));
        assert!(!enough_measured(0, 0));
    }

    /// A fully measured window is still allowed to be strong; the guard must not make
    /// every match weak.
    #[test]
    fn a_fully_measured_distinctive_window_is_still_strong() {
        let season = Season {
            number: 1,
            episodes: vec![
                Episode { season: 1, number: 1, name: "A".into(), runtime_mins: Some(30), air_date: None, overview: None },
                Episode { season: 1, number: 2, name: "B".into(), runtime_mins: Some(60), air_date: None, overview: None },
                Episode { season: 1, number: 3, name: "C".into(), runtime_mins: Some(90), air_date: None, overview: None },
            ],
        };
        let titles = vec![title("a.mkv", 30.0), title("b.mkv", 60.0)];
        let m = match_season(&titles, &season).expect("matched");
        assert_eq!(m.counted, 2);
        assert_eq!(m.confidence, Confidence::Strong);
    }
}
