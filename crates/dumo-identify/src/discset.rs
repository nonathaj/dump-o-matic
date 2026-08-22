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
use crate::tmdb::{Episode, Season};
use dumo_core::Confidence;

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
    /// Mean runtime difference across every matched title in the set.
    pub mean_delta: f64,
    /// The next-best consistent arrangement, if any.
    pub runner_up_delta: Option<f64>,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
}

impl SetSolution {
    pub fn margin(&self) -> Option<f64> {
        self.runner_up_delta.map(|r| r - self.mean_delta)
    }
}

/// Total absolute runtime difference for placing `titles` at `start`, plus how many
/// titles could actually be scored.
fn window_cost(titles: &[DiscTitle], episodes: &[Episode], start: usize) -> Option<(f64, usize)> {
    if start + titles.len() > episodes.len() {
        return None;
    }
    let mut total = 0.0;
    let mut counted = 0usize;
    for (t, e) in titles.iter().zip(episodes[start..].iter()) {
        // Episodes with no runtime contribute nothing rather than a fabricated zero.
        let Some(rt) = e.runtime_mins else { continue };
        total += (t.duration_secs / 60.0 - f64::from(rt)).abs();
        counted += 1;
    }
    Some((total, counted))
}

/// Enumerate every consistent arrangement of the set within one season.
///
/// Discs are placed in disc-number order at strictly increasing, non-overlapping
/// positions. Returns `(total cost, counted titles, start index per disc)`.
fn arrangements(
    discs: &[SetDisc],
    episodes: &[Episode],
    idx: usize,
    min_start: usize,
    acc: &mut Vec<usize>,
    out: &mut Vec<(f64, usize, Vec<usize>)>,
) {
    if idx == discs.len() {
        let mut total = 0.0;
        let mut counted = 0;
        for (d, &s) in discs.iter().zip(acc.iter()) {
            match window_cost(&d.titles, episodes, s) {
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
    while start + k <= episodes.len() {
        acc.push(start);
        // The next disc must begin after this one ends: no overlap, disc order preserved.
        arrangements(discs, episodes, idx + 1, start + k, acc, out);
        acc.pop();
        start += 1;
    }
}

/// Find the best consistent placement of a disc set across the given seasons.
pub fn solve(discs: &[SetDisc], seasons: &[Season]) -> Option<SetSolution> {
    if discs.is_empty() {
        return None;
    }
    // Placement is only meaningful in disc order.
    let mut ordered = discs.to_vec();
    ordered.sort_by_key(|d| d.disc_number);

    let total_titles: usize = ordered.iter().map(|d| d.titles.len()).sum();

    let mut best: Option<(f64, usize, Vec<usize>, &Season)> = None;
    let mut second_best: Option<f64> = None;

    for season in seasons {
        let mut found = Vec::new();
        arrangements(&ordered, &season.episodes, 0, 0, &mut Vec::new(), &mut found);
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
            let better = best
                .as_ref()
                .map(|(bc, bn, _, _)| mean < bc / (*bn).max(1) as f64)
                .unwrap_or(true);
            if i == 0 && better {
                // Whatever was best becomes the runner-up.
                if let Some((bc, bn, _, _)) = &best {
                    let prev = bc / (*bn).max(1) as f64;
                    second_best = Some(second_best.map_or(prev, |s: f64| s.min(prev)));
                }
                best = Some((cand.0, cand.1, cand.2.clone(), season));
            } else {
                second_best = Some(second_best.map_or(mean, |s: f64| s.min(mean)));
            }
        }
    }

    let (total_cost, counted, starts, season) = best?;
    let mean_delta = total_cost / counted.max(1) as f64;

    let mut placements = Vec::new();
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
        let (c, n) = window_cost(&d.titles, &season.episodes, start).unwrap_or((0.0, 1));
        placements.push(DiscPlacement {
            disc_number: d.disc_number,
            job_id: d.job_id.clone(),
            first_episode: window.first().map(|e| e.number).unwrap_or(0),
            matches,
            mean_delta: c / n.max(1) as f64,
        });
    }

    let margin = second_best.map(|s| s - mean_delta);
    let confidence = if mean_delta <= 2.0 && margin.map(|m| m >= 1.0).unwrap_or(true) {
        Confidence::Strong
    } else {
        Confidence::Weak
    };

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
    match margin {
        Some(m) if m >= 1.0 => evidence.push(format!(
            "the next consistent arrangement is {m:.1} min/title worse — the set \
             constraint leaves little room for doubt"
        )),
        Some(m) => evidence.push(format!(
            "another consistent arrangement is only {m:.1} min/title worse; check the \
             episode titles before accepting"
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
        runner_up_delta: second_best,
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
        }
    }

    fn t(name: &str, mins: f64) -> DiscTitle {
        DiscTitle {
            name: name.into(),
            duration_secs: mins * 60.0,
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
}
