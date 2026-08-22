//! Solve a whole disc set at once.
//!
//! Matching discs one at a time throws away the strongest constraint available. The
//! discs of a season box set hold **consecutive, non-overlapping runs of episodes in
//! disc order** — so disc 2 begins after disc 1 ends. Runtimes that are hopelessly
//! ambiguous in isolation often admit only one arrangement once that is enforced.
//!
//! A worked example from real data: disc 2 of a documentary anthology, holding three
//! ~52 minute titles, fits episodes 2–4 and 4–6 *equally well* — an exact tie no
//! amount of runtime comparison can break. But disc 1 fits 1–3 best, and disc 3 fits
//! 7–8 almost uniquely, so 4–6 is the only placement for disc 2 that leaves a
//! consistent set. The constraint decides what the measurements could not.

use crate::matching::{DiscTitle, TitleMatch};
use crate::signals::{self, Corpus, ScoringParams, SignalScores};
use crate::tmdb::{Episode, Season};
use dumo_core::Confidence;
use std::collections::HashSet;

/// Everything needed to score titles against one candidate season.
///
/// Built once per season rather than per arrangement: the solver enumerates every
/// consistent placement, so tokenising synopses inside the inner loop would dominate the
/// run time for no benefit.
struct SeasonScorer<'a> {
    episodes: &'a [Episode],
    /// Tokenised reference text per episode, index-aligned with `episodes`.
    references: Vec<HashSet<String>>,
    /// Which words are informative *for this season* — measured, not hardcoded. A term
    /// appearing in every synopsis of a series carries no information about which
    /// episode this is, whatever the language.
    corpus: Corpus,
    params: ScoringParams,
}

impl<'a> SeasonScorer<'a> {
    fn new(season: &'a Season, params: ScoringParams) -> Self {
        let references: Vec<HashSet<String>> = season
            .episodes
            .iter()
            .map(|e| dumo_core::text::tokenize(&e.reference_text()))
            .collect();
        let corpus = Corpus::from_references(references.iter());
        Self {
            episodes: &season.episodes,
            references,
            corpus,
            params,
        }
    }

    /// Both signals' opinions of one pairing.
    fn scores(&self, title: &DiscTitle, idx: usize) -> SignalScores {
        let e = &self.episodes[idx];
        SignalScores {
            runtime: signals::runtime_score_with(title.duration_secs / 60.0, e, &self.params),
            subtitle: title.dialogue.as_ref().map(|d| {
                signals::subtitle_score_weighted(d, &self.references[idx], &self.corpus)
            }),
        }
    }

    /// Cost of a pairing: lower is better, so the solver still minimises.
    fn cost(&self, title: &DiscTitle, idx: usize) -> f64 {
        1.0 - self.scores(title, idx).combined_with(&self.params)
    }

    /// Which episode in the whole season a single signal would pick on its own.
    fn best_by(&self, title: &DiscTitle, f: impl Fn(&SignalScores) -> Option<f64>) -> Option<u32> {
        (0..self.episodes.len())
            .filter_map(|i| f(&self.scores(title, i)).map(|s| (i, s)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| self.episodes[i].number)
    }
}

/// One disc awaiting placement.
#[derive(Debug, Clone)]
pub struct SetDisc {
    /// Disc number within the set, from the volume label.
    pub disc_number: u32,
    /// Job id, so results can be attributed back.
    pub job_id: String,
    /// Main titles, already in disc order.
    pub titles: Vec<DiscTitle>,
}

/// Where one disc landed.
#[derive(Debug, Clone)]
pub struct DiscPlacement {
    pub disc_number: u32,
    pub job_id: String,
    pub first_episode: u32,
    pub matches: Vec<TitleMatch>,
    pub mean_delta: f64,
}

/// A consistent placement of every disc in the set.
#[derive(Debug, Clone)]
pub struct SetSolution {
    pub season: u32,
    pub placements: Vec<DiscPlacement>,
    /// Mean runtime difference across every matched title, in minutes. For reporting:
    /// it is what a person can sanity-check, but it is not what the solver ranks on.
    pub mean_delta: f64,
    /// Mean combined-signal cost, 0..1, lower is better. This is the ranking quantity.
    pub mean_cost: f64,
    /// Cost of the next-best consistent arrangement, if there was one.
    pub runner_up_cost: Option<f64>,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
}

impl SetSolution {
    /// How much worse the next consistent arrangement is. Larger is safer.
    pub fn margin(&self) -> Option<f64> {
        self.runner_up_cost.map(|r| r - self.mean_cost)
    }
}

/// Combined-signal cost of placing `titles` at `start`, plus how many were scored.
///
/// Every title is counted now, not just those whose episode publishes a runtime: a
/// missing runtime scores neutrally rather than dropping out, and dialogue can still
/// decide the pairing on its own.
fn window_cost(titles: &[DiscTitle], sc: &SeasonScorer, start: usize) -> Option<(f64, usize)> {
    if start + titles.len() > sc.episodes.len() {
        return None;
    }
    let mut total = 0.0;
    for (i, t) in titles.iter().enumerate() {
        total += sc.cost(t, start + i);
    }
    Some((total, titles.len()))
}

/// Mean absolute runtime difference over a window, for reporting in minutes.
///
/// Kept separate from the cost the solver minimises: a unitless score is the right thing
/// to rank on, but "3 minutes out" is what a person can judge.
fn window_runtime_delta(titles: &[DiscTitle], episodes: &[Episode], start: usize) -> f64 {
    let mut total = 0.0;
    let mut counted = 0usize;
    for (t, e) in titles.iter().zip(episodes[start..].iter()) {
        let Some(rt) = e.runtime_mins else { continue };
        total += (t.duration_secs / 60.0 - f64::from(rt)).abs();
        counted += 1;
    }
    if counted == 0 {
        f64::NAN
    } else {
        total / counted as f64
    }
}

/// Enumerate every consistent arrangement of the set within one season.
///
/// Discs are placed in disc-number order at strictly increasing, non-overlapping
/// positions. Returns `(total cost, counted titles, start index per disc)`.
fn arrangements(
    discs: &[SetDisc],
    sc: &SeasonScorer,
    idx: usize,
    min_start: usize,
    acc: &mut Vec<usize>,
    out: &mut Vec<(f64, usize, Vec<usize>)>,
) {
    if idx == discs.len() {
        let mut total = 0.0;
        let mut counted = 0;
        for (d, &s) in discs.iter().zip(acc.iter()) {
            match window_cost(&d.titles, sc, s) {
                Some((c, n)) => {
                    total += c;
                    counted += n;
                }
                None => return,
            }
        }
        out.push((total, counted, acc.clone()));
        return;
    }

    let k = discs[idx].titles.len();
    if k == 0 {
        return;
    }
    let mut start = min_start;
    while start + k <= sc.episodes.len() {
        acc.push(start);
        // The next disc must begin after this one ends: no overlap, disc order preserved.
        arrangements(discs, sc, idx + 1, start + k, acc, out);
        acc.pop();
        start += 1;
    }
}

/// Find the best consistent placement of a disc set across the given seasons.
pub fn solve(discs: &[SetDisc], seasons: &[Season]) -> Option<SetSolution> {
    if discs.is_empty() {
        return None;
    }
    let params = ScoringParams::default();

    // Placement is only meaningful in disc order.
    let mut ordered = discs.to_vec();
    ordered.sort_by_key(|d| d.disc_number);

    let total_titles: usize = ordered.iter().map(|d| d.titles.len()).sum();
    let with_dialogue = ordered
        .iter()
        .flat_map(|d| d.titles.iter())
        .filter(|t| t.dialogue.is_some())
        .count();

    let mut best: Option<(f64, Vec<usize>, &Season)> = None;
    let mut second_best: Option<f64> = None;

    for season in seasons {
        let sc = SeasonScorer::new(season, params);
        let mut found = Vec::new();
        arrangements(&ordered, &sc, 0, 0, &mut Vec::new(), &mut found);
        if found.is_empty() {
            continue;
        }
        found.sort_by(|a, b| {
            let am = a.0 / a.1.max(1) as f64;
            let bm = b.0 / b.1.max(1) as f64;
            am.partial_cmp(&bm).unwrap_or(std::cmp::Ordering::Equal)
        });

        for (i, cand) in found.iter().take(2).enumerate() {
            let mean = cand.0 / cand.1.max(1) as f64;
            let better = best.as_ref().map(|(bc, _, _)| mean < *bc).unwrap_or(true);
            if i == 0 && better {
                // Whatever was best becomes the runner-up.
                if let Some((prev, _, _)) = &best {
                    let prev = *prev;
                    second_best = Some(second_best.map_or(prev, |s: f64| s.min(prev)));
                }
                best = Some((mean, cand.2.clone(), season));
            } else {
                second_best = Some(second_best.map_or(mean, |s: f64| s.min(mean)));
            }
        }
    }

    let (mean_cost, starts, season) = best?;
    let sc = SeasonScorer::new(season, params);

    // Per-title verdicts: what each signal would have said on its own, so confidence can
    // rest on independent signals agreeing rather than on one number being small.
    let mut verdicts: Vec<signals::TitleVerdict> = Vec::new();
    let mut placements = Vec::new();
    let mut runtime_total = 0.0;
    let mut runtime_counted = 0usize;

    for (d, &start) in ordered.iter().zip(starts.iter()) {
        let window = &season.episodes[start..start + d.titles.len()];
        let matches: Vec<TitleMatch> = d
            .titles
            .iter()
            .zip(window.iter())
            .map(|(t, e)| TitleMatch {
                title_name: t.name.clone(),
                title_mins: t.duration_secs / 60.0,
                episode: e.clone(),
                delta_mins: e
                    .runtime_mins
                    .map(|rt| (t.duration_secs / 60.0 - f64::from(rt)).abs())
                    .unwrap_or(f64::NAN),
            })
            .collect();

        for (i, t) in d.titles.iter().enumerate() {
            let idx = start + i;
            let scores = sc.scores(t, idx);
            let runtime_pick = sc.best_by(t, |s| Some(s.runtime)).unwrap_or(0);
            let subtitle_pick = t.dialogue.as_ref().and_then(|_| sc.best_by(t, |s| s.subtitle));
            let chosen = season.episodes[idx].number;
            verdicts.push(signals::TitleVerdict {
                title_name: t.name.clone(),
                chosen_episode: chosen,
                scores,
                runtime_pick,
                subtitle_pick,
                signals_agree: subtitle_pick.map(|p| p == runtime_pick).unwrap_or(false),
                // The set constraint, not a per-title contest, decided this placement;
                // the margin that matters is the one between whole arrangements.
                margin: second_best.map(|s| s - mean_cost).unwrap_or(1.0),
            });
            if let Some(rt) = season.episodes[idx].runtime_mins {
                runtime_total += (t.duration_secs / 60.0 - f64::from(rt)).abs();
                runtime_counted += 1;
            }
        }

        placements.push(DiscPlacement {
            disc_number: d.disc_number,
            job_id: d.job_id.clone(),
            first_episode: window.first().map(|e| e.number).unwrap_or(0),
            matches,
            mean_delta: window_runtime_delta(&d.titles, &season.episodes, start),
        });
    }

    let mean_delta = if runtime_counted == 0 {
        f64::NAN
    } else {
        runtime_total / runtime_counted as f64
    };
    let margin = second_best.map(|s| s - mean_cost);

    // Confidence comes from the signals agreeing, then is capped by how much better this
    // arrangement is than the next consistent one. A set that fits beautifully but has an
    // equally good alternative is not something to accept unattended.
    let mut confidence = signals::confidence_from(&verdicts);
    if margin.map(|m| m < params.min_margin).unwrap_or(false) && confidence > Confidence::Weak {
        confidence = Confidence::Weak;
    }

    let mut evidence = vec![
        format!(
            "solved {} disc(s) together against season {}, requiring consecutive \
             non-overlapping episodes in disc order",
            ordered.len(),
            season.number
        ),
        format!(
            "{total_titles} title(s) placed; runtimes differ by {mean_delta:.1} min on average"
        ),
    ];

    if with_dialogue == 0 {
        evidence.push(
            "no title had a subtitle track, so this rests on runtime alone — the weakest \
             evidence available"
                .to_string(),
        );
    } else {
        let agreed = verdicts.iter().filter(|v| v.signals_agree).count();
        evidence.push(format!(
            "{with_dialogue} of {total_titles} title(s) had dialogue to compare; runtime and \
             dialogue independently picked the same episode for {agreed} of them"
        ));
    }

    match margin {
        Some(m) if m >= params.min_margin => evidence.push(format!(
            "the next consistent arrangement scores {m:.3} worse — the set constraint \
             leaves little room for doubt"
        )),
        Some(m) => evidence.push(format!(
            "another consistent arrangement is only {m:.3} worse; check the episode \
             titles before accepting"
        )),
        None => evidence.push(
            "this is the only arrangement that satisfies the set constraint".to_string(),
        ),
    }
    evidence
        .push("solving the set jointly is still inference — confirm before filing".to_string());

    Some(SetSolution {
        season: season.number,
        placements,
        mean_delta,
        mean_cost,
        runner_up_cost: second_best,
        confidence,
        evidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(n: u32, rt: u32, name: &str) -> Episode {
        Episode {
            season: 1,
            number: n,
            name: name.into(),
            runtime_mins: Some(rt),
            air_date: None,
            overview: None,
        }
    }

    fn t(name: &str, mins: f64) -> DiscTitle {
        DiscTitle {
            name: name.into(),
            duration_secs: mins * 60.0,
            dialogue: None,
        }
    }

    /// Real runtimes from TMDB for 30 for 30 season 1.
    fn season_one() -> Season {
        Season {
            number: 1,
            episodes: vec![
                ep(1, 51, "King's Ransom"),
                ep(2, 53, "The Band That Wouldn't Die"),
                ep(3, 51, "Small Potatoes"),
                ep(4, 52, "Muhammad and Larry"),
                ep(5, 51, "Without Bias"),
                ep(6, 51, "The Legend of Jimmy the Greek"),
                ep(7, 102, "The U"),
                ep(8, 69, "Winning Time"),
                ep(9, 52, "Guru of Go"),
                ep(10, 81, "No Crossover"),
            ],
        }
    }

    /// The real durations of all three discs. Disc 2 alone is an exact tie between
    /// episodes 2-4 and 4-6; solved as a set, only 4-6 is consistent.
    #[test]
    fn set_constraint_resolves_a_tie_that_runtime_cannot() {
        let discs = vec![
            SetDisc {
                disc_number: 1,
                job_id: "d1".into(),
                titles: vec![
                    t("B1_t01.mkv", 51.2),
                    t("D1_t02.mkv", 54.0),
                    t("E1_t04.mkv", 51.9),
                ],
            },
            SetDisc {
                disc_number: 2,
                job_id: "d2".into(),
                titles: vec![
                    t("B1_t00.mkv", 52.2),
                    t("D1_t01.mkv", 51.2),
                    t("E1_t02.mkv", 51.8),
                ],
            },
            SetDisc {
                disc_number: 3,
                job_id: "d3".into(),
                titles: vec![t("B1_t00.mkv", 102.5), t("D1_t01.mkv", 69.3)],
            },
        ];

        let s = solve(&discs, &[season_one()]).expect("solved");
        assert_eq!(s.season, 1);
        assert_eq!(s.placements[0].first_episode, 1);
        assert_eq!(s.placements[1].first_episode, 4, "disc 2 must be episodes 4-6");
        assert_eq!(s.placements[2].first_episode, 7);

        // Spot-check the actual episode names landed correctly.
        assert_eq!(s.placements[1].matches[0].episode.name, "Muhammad and Larry");
        assert_eq!(
            s.placements[1].matches[2].episode.name,
            "The Legend of Jimmy the Greek"
        );
        assert_eq!(s.placements[2].matches[0].episode.name, "The U");
    }

    #[test]
    fn placements_never_overlap_and_follow_disc_order() {
        let discs = vec![
            SetDisc {
                disc_number: 2,
                job_id: "b".into(),
                titles: vec![t("x_t00.mkv", 52.0), t("y_t01.mkv", 51.0)],
            },
            SetDisc {
                disc_number: 1,
                job_id: "a".into(),
                titles: vec![t("p_t00.mkv", 51.0), t("q_t01.mkv", 53.0)],
            },
        ];
        let s = solve(&discs, &[season_one()]).expect("solved");
        // Sorted into disc order regardless of input order.
        assert_eq!(s.placements[0].disc_number, 1);
        assert_eq!(s.placements[1].disc_number, 2);
        let end_of_first = s.placements[0].first_episode + 1;
        assert!(
            s.placements[1].first_episode > end_of_first,
            "disc 2 started at {} but disc 1 ended at {end_of_first}",
            s.placements[1].first_episode
        );
    }

    #[test]
    fn a_single_disc_still_solves() {
        let discs = vec![SetDisc {
            disc_number: 1,
            job_id: "only".into(),
            titles: vec![t("a_t00.mkv", 102.5), t("b_t01.mkv", 69.3)],
        }];
        let s = solve(&discs, &[season_one()]).expect("solved");
        assert_eq!(s.placements[0].first_episode, 7);
    }

    #[test]
    fn returns_none_when_the_season_cannot_hold_the_set() {
        let small = Season {
            number: 1,
            episodes: vec![ep(1, 51, "A")],
        };
        let discs = vec![SetDisc {
            disc_number: 1,
            job_id: "d".into(),
            titles: vec![t("a_t00.mkv", 51.0), t("b_t01.mkv", 51.0)],
        }];
        assert!(solve(&discs, &[small]).is_none());
    }

    #[test]
    fn empty_input_is_none() {
        assert!(solve(&[], &[season_one()]).is_none());
    }

    #[test]
    fn set_solution_is_never_auto_acceptable() {
        let discs = vec![SetDisc {
            disc_number: 1,
            job_id: "d".into(),
            titles: vec![t("a_t00.mkv", 102.5), t("b_t01.mkv", 69.3)],
        }];
        let s = solve(&discs, &[season_one()]).unwrap();
        assert!(!s.confidence.is_auto_acceptable());
    }

    fn ep_with(n: u32, rt: u32, name: &str, overview: &str) -> Episode {
        Episode {
            season: 1,
            number: n,
            name: name.into(),
            runtime_mins: Some(rt),
            air_date: None,
            overview: Some(overview.into()),
        }
    }

    fn t_with(name: &str, mins: f64, dialogue: &str) -> DiscTitle {
        DiscTitle {
            name: name.into(),
            duration_secs: mins * 60.0,
            dialogue: Some(dumo_core::text::tokenize(dialogue)),
        }
    }

    /// Dialogue decides what runtime cannot.
    ///
    /// Every episode here runs exactly 50 minutes, so runtime carries no information at
    /// all and the set constraint alone admits several arrangements. The dialogue on the
    /// single disc names the subject of episodes 3 and 4, and that is enough.
    #[test]
    fn dialogue_breaks_a_runtime_tie() {
        let season = Season {
            number: 1,
            episodes: vec![
                ep_with(1, 50, "Reykjavik", "A summit in Iceland between two leaders."),
                ep_with(2, 50, "Chernobyl", "The reactor at Chernobyl fails catastrophically."),
                ep_with(3, 50, "Gretzky", "Wayne Gretzky is traded from Edmonton to Los Angeles."),
                ep_with(4, 50, "Tiananmen", "Protesters occupy Tiananmen Square in Beijing."),
                ep_with(5, 50, "Berlin", "The Berlin Wall comes down."),
            ],
        };
        let discs = vec![SetDisc {
            disc_number: 1,
            job_id: "j1".into(),
            titles: vec![
                t_with("t01.mkv", 50.0, "GRETZKY WAS TRADED FROM EDMONTON TO LOS ANGELES TODAY"),
                t_with("t02.mkv", 50.0, "PROTESTERS FILLED TIANANMEN SQUARE IN BEIJING"),
            ],
        }];

        let sol = solve(&discs, &[season]).expect("solved");
        let picked: Vec<u32> = sol.placements[0]
            .matches
            .iter()
            .map(|m| m.episode.number)
            .collect();
        assert_eq!(picked, vec![3, 4], "dialogue should place these at episodes 3-4");
    }

    /// Runtime alone must never reach a confidence we would act on unattended.
    ///
    /// This is the whole reason the scorer exists: episode runtimes are a weak signal,
    /// and a tidy-looking runtime fit is not evidence enough to rename files by itself.
    #[test]
    fn runtime_only_sets_are_never_auto_acceptable() {
        let season = Season {
            number: 1,
            episodes: (1..=4).map(|n| ep(n, 50, &format!("Ep {n}"))).collect(),
        };
        let discs = vec![SetDisc {
            disc_number: 1,
            job_id: "j1".into(),
            titles: vec![t("t01.mkv", 50.0), t("t02.mkv", 50.0)],
        }];

        let sol = solve(&discs, &[season]).expect("solved");
        assert!(
            !sol.confidence.is_auto_acceptable(),
            "runtime alone reached {:?}",
            sol.confidence
        );
        assert!(
            sol.evidence.iter().any(|e| e.contains("runtime alone")),
            "the report must say the evidence is thin: {:?}",
            sol.evidence
        );
    }

    /// A title with no subtitle track must not be penalised against one that has them.
    #[test]
    fn titles_without_dialogue_still_place_on_runtime() {
        let season = Season {
            number: 1,
            episodes: vec![
                ep_with(1, 30, "One", "The first thing happens."),
                ep_with(2, 60, "Two", "The second thing happens."),
            ],
        };
        let discs = vec![SetDisc {
            disc_number: 1,
            job_id: "j1".into(),
            titles: vec![t("t01.mkv", 30.0), t("t02.mkv", 60.0)],
        }];
        let sol = solve(&discs, &[season]).expect("solved");
        let picked: Vec<u32> = sol.placements[0]
            .matches
            .iter()
            .map(|m| m.episode.number)
            .collect();
        assert_eq!(picked, vec![1, 2]);
    }

    /// The reported minute figure must stay in minutes even though ranking moved to a
    /// unitless score — it is the number a person checks the result against.
    #[test]
    fn runtime_delta_is_still_reported_in_minutes() {
        let season = Season {
            number: 1,
            episodes: vec![ep(1, 50, "One"), ep(2, 50, "Two")],
        };
        let discs = vec![SetDisc {
            disc_number: 1,
            job_id: "j1".into(),
            titles: vec![t("t01.mkv", 52.0), t("t02.mkv", 54.0)],
        }];
        let sol = solve(&discs, &[season]).expect("solved");
        // 2 minutes out and 4 minutes out: a 3 minute mean.
        assert!((sol.mean_delta - 3.0).abs() < 0.01, "got {}", sol.mean_delta);
        assert!(sol.mean_cost >= 0.0 && sol.mean_cost <= 1.0);
    }
}

// --- Deciding what actually constitutes a set -------------------------------------

/// A group of discs believed to belong together, with the reasoning and any doubts.
#[derive(Debug, Clone)]
pub struct SetGroup {
    /// Normalised series name the grouping was based on.
    pub series_key: String,
    pub discs: Vec<SetDisc>,
    /// Why these were grouped, in plain language.
    pub evidence: Vec<String>,
    /// Reasons to distrust the grouping. Non-empty means do not treat it as an ordered
    /// set without checking.
    pub warnings: Vec<String>,
}

impl SetGroup {
    /// Whether this group can safely be solved with the consecutive-episodes constraint.
    ///
    /// The constraint is only valid if these really are discs of one release, in a known
    /// order. When that is in doubt, solving jointly would turn a guess about grouping
    /// into confident-looking episode assignments — worse than not solving at all.
    pub fn is_orderable(&self) -> bool {
        self.warnings.is_empty() && self.discs.len() > 1
    }
}

/// One disc offered for grouping.
#[derive(Debug, Clone)]
pub struct GroupInput {
    pub job_id: String,
    /// Volume label as read from the disc.
    pub label: String,
    pub titles: Vec<DiscTitle>,
}

/// Group discs into sets by what their volume labels say, not by how they were selected.
///
/// This exists because the alternative — treating whatever the caller passed in as one
/// set — silently merges unrelated series. The consecutive-episode constraint is powerful
/// precisely because it is a strong claim, so the claim has to be earned: discs are only
/// grouped when their labels agree on a series once disc and season markers are removed,
/// and only ordered when every disc states its own number.
pub fn group_into_sets(inputs: &[GroupInput]) -> Vec<SetGroup> {
    use std::collections::BTreeMap;

    let mut by_key: BTreeMap<String, Vec<&GroupInput>> = BTreeMap::new();
    for i in inputs {
        let key = crate::matching::query_from_label(&i.label);
        // A disc with no usable label cannot be grouped with anything; it gets its own
        // key so it is never silently attached to a real set.
        let key = if key.trim().is_empty() {
            format!("<unlabelled:{}>", i.job_id)
        } else {
            key
        };
        by_key.entry(key).or_default().push(i);
    }

    let mut out = Vec::new();
    for (series_key, members) in by_key {
        let mut discs = Vec::new();
        let mut warnings = Vec::new();
        let mut evidence = vec![format!(
            "{} disc(s) share the series name {series_key:?} once disc and season \
             markers are stripped from their volume labels",
            members.len()
        )];

        let mut seen_numbers: BTreeMap<u32, String> = BTreeMap::new();
        for m in &members {
            match crate::matching::disc_number_from_label(&m.label) {
                Some(n) => {
                    if let Some(other) = seen_numbers.get(&n) {
                        warnings.push(format!(
                            "two discs both claim to be disc {n} ({other} and {}), so their \
                             order is unknown",
                            m.job_id
                        ));
                    }
                    seen_numbers.insert(n, m.job_id.clone());
                    discs.push(SetDisc {
                        disc_number: n,
                        job_id: m.job_id.clone(),
                        titles: m.titles.clone(),
                    });
                }
                None => {
                    // Never invent a position. An unnumbered disc could sit anywhere in
                    // the run, and guessing would corrupt every disc after it.
                    warnings.push(format!(
                        "{} has no disc number in its label ({:?}), so its position in \
                         the set is unknown",
                        m.job_id, m.label
                    ));
                    discs.push(SetDisc {
                        disc_number: 0,
                        job_id: m.job_id.clone(),
                        titles: m.titles.clone(),
                    });
                }
            }
        }

        discs.sort_by_key(|d| d.disc_number);

        // A run with gaps may just be discs not yet ripped, but the constraint assumes
        // consecutive coverage, so say so rather than quietly assuming it.
        let numbers: Vec<u32> = discs.iter().map(|d| d.disc_number).filter(|n| *n > 0).collect();
        if numbers.len() > 1 {
            let expected: Vec<u32> = (numbers[0]..=*numbers.last().unwrap()).collect();
            if numbers != expected {
                warnings.push(format!(
                    "disc numbers {numbers:?} are not a consecutive run, so episodes \
                     between them are missing and the set cannot be laid out end to end"
                ));
            } else {
                evidence.push(format!("disc numbers {numbers:?} form a consecutive run"));
            }
        }

        out.push(SetGroup {
            series_key,
            discs,
            evidence,
            warnings,
        });
    }
    out
}

#[cfg(test)]
mod grouping_tests {
    use super::*;

    fn input(job: &str, label: &str, n: usize) -> GroupInput {
        GroupInput {
            job_id: job.into(),
            label: label.into(),
            titles: (0..n)
                .map(|i| DiscTitle {
                    name: format!("t{i:02}.mkv"),
                    duration_secs: 3060.0,
                    dialogue: None,
                })
                .collect(),
        }
    }

    /// The failure that motivated this: two unrelated series must never merge.
    #[test]
    fn different_series_are_never_grouped_together() {
        let g = group_into_sets(&[
            input("j1", "ESPN_30_FOR_30_DISC_1", 3),
            input("j2", "ESPN_30_FOR_30_DISC_2", 3),
            input("j3", "THE_WIRE_SEASON_1_DISC_1", 3),
        ]);
        assert_eq!(g.len(), 2, "expected two distinct series, got {:?}",
            g.iter().map(|x| &x.series_key).collect::<Vec<_>>());
        let espn = g.iter().find(|x| x.series_key.contains("30 for 30")).unwrap();
        assert_eq!(espn.discs.len(), 2);
        assert!(espn.is_orderable());
    }

    #[test]
    fn a_disc_with_no_number_makes_the_group_unorderable() {
        let g = group_into_sets(&[
            input("j1", "ESPN_30_FOR_30_DISC_1", 3),
            input("j2", "ESPN_30_FOR_30", 3),
        ]);
        assert_eq!(g.len(), 1);
        assert!(!g[0].is_orderable(), "must not order a set with an unplaced disc");
        assert!(g[0].warnings.iter().any(|w| w.contains("no disc number")));
    }

    #[test]
    fn duplicate_disc_numbers_are_refused() {
        let g = group_into_sets(&[
            input("j1", "SHOW_DISC_1", 2),
            input("j2", "SHOW_DISC_1", 2),
        ]);
        assert!(!g[0].is_orderable());
        assert!(g[0].warnings.iter().any(|w| w.contains("both claim")));
    }

    #[test]
    fn a_gap_in_the_run_is_flagged() {
        let g = group_into_sets(&[
            input("j1", "SHOW_DISC_1", 2),
            input("j3", "SHOW_DISC_3", 2),
        ]);
        assert!(!g[0].is_orderable());
        assert!(g[0].warnings.iter().any(|w| w.contains("consecutive")));
    }

    #[test]
    fn a_consecutive_run_is_orderable_and_says_why() {
        let g = group_into_sets(&[
            input("j1", "SHOW_DISC_1", 2),
            input("j2", "SHOW_DISC_2", 2),
            input("j3", "SHOW_DISC_3", 2),
        ]);
        assert!(g[0].is_orderable());
        assert!(g[0].evidence.iter().any(|e| e.contains("consecutive run")));
        assert_eq!(
            g[0].discs.iter().map(|d| d.disc_number).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    /// A lone disc is not a set: the constraint adds nothing and claiming it would
    /// overstate the evidence.
    #[test]
    fn a_single_disc_is_not_orderable_as_a_set() {
        let g = group_into_sets(&[input("j1", "SHOW_DISC_1", 2)]);
        assert!(!g[0].is_orderable());
    }

    #[test]
    fn unlabelled_discs_stay_separate() {
        let g = group_into_sets(&[input("j1", "", 2), input("j2", "", 2)]);
        assert_eq!(g.len(), 2, "unlabelled discs must not be grouped with each other");
    }
}
