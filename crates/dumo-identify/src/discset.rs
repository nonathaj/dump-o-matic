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
/// Scores are computed **once per (title, episode) pair** and cached. The solver
/// enumerates every consistent arrangement of the set and evaluates the same pairings
/// inside each one, so scoring on demand is quadratically wasteful: a five-disc set ran
/// for over 72 minutes without finishing, re-stemming ~1,500 dialogue words on each of
/// roughly 340,000 evaluations. There are only titles x episodes distinct pairings — 390
/// for that same set — so the table is small and the solve becomes table lookups.
struct SeasonScorer<'a> {
    episodes: &'a [Episode],
    /// `cost[title][episode]`, indexed by position in the flattened title list.
    cost: Vec<Vec<f64>>,
    /// `scores[title][episode]`, kept for reporting each signal separately.
    scores: Vec<Vec<SignalScores>>,
}

impl<'a> SeasonScorer<'a> {
    /// `titles` must be the flattened list, in disc order, that the solver will index.
    fn new(episodes: &'a [Episode], titles: &[&DiscTitle], params: ScoringParams) -> Self {
        // Reference text and its weighting depend only on the season, so both are built
        // once here rather than per pairing.
        let references: Vec<HashSet<String>> = episodes
            .iter()
            .map(|e| signals::stem_all(&dumo_core::text::tokenize(&e.reference_text())))
            .collect();
        let corpus = Corpus::from_references(references.iter());
        // Likewise the dialogue: stemming it is the expensive part, and it does not
        // change between episodes.
        let dialogue: Vec<Option<HashSet<String>>> = titles
            .iter()
            .map(|t| t.dialogue.as_ref().map(|d| signals::stem_all(d.keys())))
            .collect();

        let mut scores = Vec::with_capacity(titles.len());
        let mut cost = Vec::with_capacity(titles.len());
        for (ti, t) in titles.iter().enumerate() {
            let mut srow = Vec::with_capacity(episodes.len());
            let mut crow = Vec::with_capacity(episodes.len());
            for (ei, e) in episodes.iter().enumerate() {
                let mins = t.duration_secs / 60.0;
                let sc = SignalScores {
                    runtime: signals::runtime_score_with(mins, e, &params),
                    subtitle: dialogue[ti].as_ref().map(|d| {
                        signals::subtitle_score_stemmed(d, &references[ei], &corpus)
                    }),
                };
                // An impossible pairing is excluded outright rather than merely scored
                // badly, so no amount of dialogue agreement can vote it back in.
                crow.push(if signals::runtime_is_implausible(mins, e, &params) {
                    f64::INFINITY
                } else {
                    1.0 - sc.combined_with(&params)
                });
                srow.push(sc);
            }
            scores.push(srow);
            cost.push(crow);
        }

        Self {
            episodes,
            cost,
            scores,
        }
    }

    /// Which episode a single signal would pick for this title, on its own.
    fn best_by(&self, title: usize, f: impl Fn(&SignalScores) -> Option<f64>) -> Option<u32> {
        (0..self.episodes.len())
            .filter_map(|i| f(&self.scores[title][i]).map(|s| (i, s)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| self.episodes[i].number)
    }
}

/// One disc awaiting placement./// One disc awaiting placement.
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
    /// First season the placement touches, for callers that want a single number.
    pub season: u32,
    /// Every season the placement spans. A box set laid out in release order routinely
    /// crosses the season boundaries a metadata provider invents.
    pub seasons: Vec<u32>,
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
    /// Per-title breakdown: what each signal said, and whether they concurred.
    ///
    /// Reported rather than kept private because the whole point of scoring on more than
    /// one signal is to be able to see them disagree. A confidence number with no way to
    /// inspect how it was reached is not much better than a guess.
    pub verdicts: Vec<signals::TitleVerdict>,
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
fn window_cost(
    sc: &SeasonScorer,
    title_offset: usize,
    len: usize,
    start: usize,
) -> Option<(f64, usize)> {
    if start + len > sc.episodes.len() {
        return None;
    }
    let mut total = 0.0;
    for i in 0..len {
        total += sc.cost[title_offset + i][start + i];
    }
    Some((total, len))
}

/// Mean absolute runtime difference over a disc's matches, for reporting in minutes.
///
/// Kept separate from the cost the solver minimises: a unitless score is the right thing
/// to rank on, but "3 minutes out" is what a person can judge.
fn mean_runtime_delta(matches: &[TitleMatch]) -> f64 {
    let measured: Vec<f64> = matches
        .iter()
        .map(|m| m.delta_mins)
        .filter(|d| !d.is_nan())
        .collect();
    if measured.is_empty() {
        f64::NAN
    } else {
        measured.iter().sum::<f64>() / measured.len() as f64
    }
}

/// Best and next-best placements of the whole set, by dynamic programming.
///
/// Placing discs in order at strictly increasing, non-overlapping positions is a
/// shortest-path problem, and it must be solved as one. Enumerating the arrangements
/// instead is fine while the candidate list is one short season — twelve discs of 30
/// titles against a 30-episode season admit exactly one arrangement — but the moment the
/// search widened to every episode of the series it became C(82, 12), about 10^14
/// placements. Materialising them killed the process after it had allocated 128 GB.
///
/// Two tables make it linear instead:
///   `suffix[i][s]` — best cost for discs `i..` given disc `i` starts at or after `s`
///   `prefix[i][s]` — best cost for discs `..i` all placed strictly before `s`
///
/// Their sum around a pinned placement gives the best arrangement containing it, which
/// yields both the winner and a runner-up that genuinely differs somewhere.
struct SetPlacement {
    /// Start index of each disc, in disc order.
    starts: Vec<usize>,
    /// Mean cost per scored title of the winning arrangement.
    mean_cost: f64,
    /// Mean cost of the best arrangement that places some disc differently.
    runner_up: Option<f64>,
}

/// Largest candidate list this will consider, in episodes.
///
/// The tables below are `(discs + 1) x (episodes + 2)` doubles — a few megabytes even for
/// an absurdly long series — so this is not about the DP. It is a backstop against a
/// future change reintroducing something super-linear: an explicit ceiling fails a solve
/// loudly instead of letting a bug consume the machine. A series with more episodes than
/// this is beyond what disc-order matching can meaningfully constrain anyway.
pub const MAX_CANDIDATE_EPISODES: usize = 2_000;

/// Largest number of discs solved jointly.
pub const MAX_SET_DISCS: usize = 100;

fn place(discs: &[SetDisc], sc: &SeasonScorer, gap_penalty: f64) -> Option<SetPlacement> {
    let n = discs.len();
    let e_count = sc.episodes.len();
    if n == 0 || e_count == 0 {
        return None;
    }
    if e_count > MAX_CANDIDATE_EPISODES || n > MAX_SET_DISCS {
        return None;
    }
    let lens: Vec<usize> = discs.iter().map(|d| d.titles.len()).collect();
    if lens.iter().any(|&k| k == 0) {
        return None;
    }
    let total_titles: usize = lens.iter().sum();
    if total_titles > e_count {
        return None;
    }
    let mut offsets = Vec::with_capacity(n);
    let mut acc = 0usize;
    for &k in &lens {
        offsets.push(acc);
        acc += k;
    }

    let inf = f64::INFINITY;
    let lam = gap_penalty;
    let at = |i: usize, s: usize| -> f64 {
        match window_cost(sc, offsets[i], lens[i], s) {
            Some((c, _)) => c,
            None => inf,
        }
    };
    let width = e_count + 2;

    // g[i][s]  — discs i.. with disc i starting exactly at s, gaps between them charged.
    // trans[i][b] — the same but entered from a previous disc ending at b, so the gap
    //               from b to wherever disc i starts is charged too.
    //
    // A linear penalty keeps this O(discs x episodes): minimising `lam*(s-b) + g[i][s]`
    // over `s >= b` is a suffix minimum of `g[i][s] + lam*s`, shifted by `lam*b`.
    let mut g = vec![vec![inf; width]; n];
    let mut trans = vec![vec![inf; width]; n + 1];
    for b in 0..width {
        trans[n][b] = 0.0; // nothing after the last disc, and no trailing penalty
    }

    for i in (0..n).rev() {
        for s in 0..=e_count.saturating_sub(lens[i]) {
            let cost = at(i, s);
            g[i][s] = if cost.is_finite() {
                cost + trans[i + 1][s + lens[i]]
            } else {
                inf
            };
        }
        let mut m = inf;
        for b in (0..=e_count).rev() {
            let here = if g[i][b].is_finite() {
                g[i][b] + lam * b as f64
            } else {
                inf
            };
            if here < m {
                m = here;
            }
            trans[i][b] = if m.is_finite() { m - lam * b as f64 } else { inf };
        }
    }

    // The first disc pays no penalty for where the set begins: a box set may legitimately
    // start part-way into a series. Likewise nothing is charged after the last disc.
    let mut best = inf;
    for s in 0..=e_count.saturating_sub(lens[0]) {
        if g[0][s] < best {
            best = g[0][s];
        }
    }
    if !best.is_finite() {
        return None;
    }

    // Reconstruct: first disc by plain minimum, the rest through the penalised transition.
    let mut starts = Vec::with_capacity(n);
    let mut cursor = 0usize;
    for i in 0..n {
        let target = if i == 0 { best } else { trans[i][cursor] };
        let mut chosen = None;
        for s in cursor..=e_count.saturating_sub(lens[i]) {
            let candidate = if i == 0 {
                g[i][s]
            } else {
                lam * (s - cursor) as f64 + g[i][s]
            };
            if (candidate - target).abs() < 1e-9 {
                chosen = Some(s);
                break;
            }
        }
        let s = chosen?;
        starts.push(s);
        cursor = s + lens[i];
    }

    // pf[i][s] — best cost for discs before i, given disc i starts at s (gap charged).
    let mut pf = vec![vec![inf; width]; n + 1];
    for s in 0..width {
        pf[0][s] = 0.0;
    }
    for i in 0..n {
        // end_cost[e] — discs 0..=i placed with disc i ending exactly at e.
        let mut end_cost = vec![inf; width];
        for s in 0..=e_count.saturating_sub(lens[i]) {
            let c = pf[i][s] + at(i, s);
            if c < end_cost[s + lens[i]] {
                end_cost[s + lens[i]] = c;
            }
        }
        let mut m = inf;
        for s in 0..=e_count {
            let here = if end_cost[s].is_finite() {
                end_cost[s] - lam * s as f64
            } else {
                inf
            };
            if here < m {
                m = here;
            }
            pf[i + 1][s] = if m.is_finite() { m + lam * s as f64 } else { inf };
        }
    }

    // Runner-up: best arrangement placing at least one disc somewhere else.
    let mut runner = inf;
    for i in 0..n {
        for s in 0..=e_count.saturating_sub(lens[i]) {
            if s == starts[i] {
                continue;
            }
            let c = pf[i][s] + at(i, s) + trans[i + 1][s + lens[i]];
            if c < runner {
                runner = c;
            }
        }
    }

    let scored = total_titles.max(1) as f64;
    Some(SetPlacement {
        starts,
        mean_cost: best / scored,
        runner_up: runner.is_finite().then(|| runner / scored),
    })
}

/// Fewest titles a set must have before dialogue may reorder it.
///
/// Reordering compares each title against the others' episodes, normalised across the
/// window. Over a handful of episodes that normalisation is mostly noise; across a
/// season it is not.
const MIN_REORDER_TITLES: usize = 6;

/// How much better, on average, a moved title's dialogue must fit its new episode than
/// its disc-order one, in standard deviations of that title's scores.
///
/// Measured on a 33-title season whose discs follow broadcast rather than TMDB order:
/// the correct reordering moved 16 titles at a mean gain of 2.6, and every move was
/// confirmed by reading the captions.
const MIN_REORDER_GAIN: f64 = 1.0;

/// Cost per position a title is moved from disc order, in the same units as the gain.
///
/// Enough that dialogue which cannot tell two episodes apart leaves them in disc
/// order; far too little to hold a title in place against dialogue that names its
/// episode outright.
const REORDER_DISTANCE_COST: f64 = 0.1;

/// A reordering of a set's titles within the episodes disc order gave them.
struct Reorder {
    /// For each title, the window position it moves to.
    order: Vec<usize>,
    /// Mean z-score gain over the moved titles.
    mean_gain: f64,
    /// Per moved title: (title index, gain, z-score at the new position).
    moves: Vec<(usize, f64, f64)>,
}

/// Least-cost one-to-one assignment of rows to columns of a square matrix.
///
/// The Hungarian algorithm, O(n^3) — a season's 50 titles are nothing. Infinite costs
/// are honoured as "never", provided some finite assignment exists.
fn assign_min_cost(cost: &[Vec<f64>]) -> Vec<usize> {
    const NEVER: f64 = 1e12;
    let n = cost.len();
    let c = |i: usize, j: usize| {
        let v = cost[i][j];
        if v.is_finite() {
            v
        } else {
            NEVER
        }
    };
    // 1-indexed potentials, with column 0 a sentinel, as in the textbook formulation.
    let mut u = vec![0.0; n + 1];
    let mut v = vec![0.0; n + 1];
    let mut row_of = vec![0usize; n + 1];
    let mut way = vec![0usize; n + 1];
    for i in 1..=n {
        row_of[0] = i;
        let mut j0 = 0usize;
        let mut min_v = vec![f64::INFINITY; n + 1];
        let mut used = vec![false; n + 1];
        loop {
            used[j0] = true;
            let i0 = row_of[j0];
            let mut delta = f64::INFINITY;
            let mut j1 = 0usize;
            for j in 1..=n {
                if used[j] {
                    continue;
                }
                let reduced = c(i0 - 1, j - 1) - u[i0] - v[j];
                if reduced < min_v[j] {
                    min_v[j] = reduced;
                    way[j] = j0;
                }
                if min_v[j] < delta {
                    delta = min_v[j];
                    j1 = j;
                }
            }
            for j in 0..=n {
                if used[j] {
                    u[row_of[j]] += delta;
                    v[j] -= delta;
                } else {
                    min_v[j] -= delta;
                }
            }
            j0 = j1;
            if row_of[j0] == 0 {
                break;
            }
        }
        loop {
            let j1 = way[j0];
            row_of[j0] = row_of[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }
    let mut out = vec![0usize; n];
    for j in 1..=n {
        out[row_of[j] - 1] = j - 1;
    }
    out
}

/// An episode name with a trailing part number removed: "Ninja Quest (2)" -> "Ninja Quest".
fn story_name(name: &str) -> Option<&str> {
    let t = name.trim_end();
    let inner = t.strip_suffix(')')?;
    let open = inner.rfind('(')?;
    let digits = &inner[open + 1..];
    (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
        .then(|| inner[..open].trim_end())
}

/// Let dialogue reorder titles among the episodes the consecutive solve gave them.
///
/// Disc order is an assumption about how a box was packed, and it fails in a specific
/// way: a set authored in broadcast order puts an episode the metadata provider files
/// elsewhere — a holiday special, typically — in the middle, and every title after it
/// is then placed one episode early. Measured on such a season, 16 of 33 titles were
/// misnamed, and no runtime could have said so: every episode ran 20 minutes.
///
/// Dialogue can, once it is scored for what distinguishes titles from *each other*. The
/// existing score weighs a synopsis word by how rare it is among synopses; here it is
/// also weighed by how rare it is among the set's dialogue, so a character named in
/// every episode counts for nothing and a monster named in one counts for a great deal.
/// Repetition counts too, logarithmically: a title that says "brick" 25 times is about
/// a brick. Each title's scores are then normalised, and the titles are assigned to
/// episodes jointly, one each, with a small cost for distance from disc order.
///
/// Parts of a multi-part story share nearly all their words, so dialogue cannot order
/// them; disc order can. Each story's parts are therefore put back in disc order among
/// the slots the assignment gave that story.
///
/// Returns `None` — leave disc order alone — unless the moves are well supported.
fn reorder_by_dialogue(
    titles: &[&DiscTitle],
    window: &[&Episode],
    implausible: impl Fn(usize, usize) -> bool,
) -> Option<Reorder> {
    let n = titles.len();
    if n < MIN_REORDER_TITLES || window.len() != n {
        return None;
    }
    let dialogue: Vec<std::collections::HashMap<String, u32>> = titles
        .iter()
        .map(|t| t.dialogue.as_ref().map(signals::stem_counts))
        .collect::<Option<_>>()?;
    let references: Vec<HashSet<String>> = window
        .iter()
        .map(|e| signals::stem_all(&dumo_core::text::tokenize(&e.reference_text())))
        .collect();

    // Inverse document frequency on both sides: among the synopses, and among the titles.
    let idf = |documents: usize, containing: usize| {
        ((documents as f64 + 1.0) / (containing as f64 + 1.0)).ln().max(0.0)
    };
    let mut ref_df: std::collections::HashMap<&str, usize> = Default::default();
    for r in &references {
        for w in r {
            *ref_df.entry(w).or_insert(0) += 1;
        }
    }
    let mut dlg_df: std::collections::HashMap<&str, usize> = Default::default();
    for d in &dialogue {
        for w in d.keys() {
            *dlg_df.entry(w).or_insert(0) += 1;
        }
    }

    let mut z = Vec::with_capacity(n);
    for d in &dialogue {
        let raw: Vec<f64> = references
            .iter()
            .map(|r| {
                let mut total = 0.0;
                let mut hit = 0.0;
                for w in r {
                    let rw = idf(n, ref_df[w.as_str()]);
                    total += rw;
                    if let Some(&count) = d.get(w) {
                        hit += rw * idf(n, dlg_df[w.as_str()]) * (1.0 + f64::from(count)).ln();
                    }
                }
                if total > 0.0 {
                    hit / total
                } else {
                    0.0
                }
            })
            .collect();
        let mean = raw.iter().sum::<f64>() / n as f64;
        let sd = (raw.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
        z.push(
            raw.iter()
                .map(|x| if sd > 0.0 { (x - mean) / sd } else { 0.0 })
                .collect::<Vec<f64>>(),
        );
    }

    let cost: Vec<Vec<f64>> = (0..n)
        .map(|t| {
            (0..n)
                .map(|e| {
                    if implausible(t, e) {
                        f64::INFINITY
                    } else {
                        -z[t][e] + REORDER_DISTANCE_COST * t.abs_diff(e) as f64
                    }
                })
                .collect()
        })
        .collect();
    let mut order = assign_min_cost(&cost);
    if (0..n).any(|t| implausible(t, order[t])) {
        return None;
    }

    // Parts of one story keep their disc order among the slots the story was given.
    let mut stories: std::collections::HashMap<&str, Vec<usize>> = Default::default();
    for t in 0..n {
        if let Some(story) = story_name(&window[order[t]].name) {
            stories.entry(story).or_default().push(t);
        }
    }
    for ts in stories.values() {
        let mut slots: Vec<usize> = ts.iter().map(|&t| order[t]).collect();
        slots.sort_unstable();
        for (&t, slot) in ts.iter().zip(slots) {
            order[t] = slot;
        }
    }

    let moves: Vec<(usize, f64, f64)> = (0..n)
        .filter(|&t| order[t] != t)
        .map(|t| (t, z[t][order[t]] - z[t][t], z[t][order[t]]))
        .collect();
    if moves.is_empty() {
        return None;
    }
    let mean_gain = moves.iter().map(|m| m.1).sum::<f64>() / moves.len() as f64;
    (mean_gain >= MIN_REORDER_GAIN).then_some(Reorder {
        order,
        mean_gain,
        moves,
    })
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

    // The solver indexes titles by position in this flattened, disc-ordered list.
    let flat: Vec<&DiscTitle> = ordered.iter().flat_map(|d| d.titles.iter()).collect();

    // Solve against every episode of the series as one ordered list, not season by
    // season.
    //
    // A physical box set is laid out in release order, and release order crosses the
    // season boundaries a metadata provider invents. Requiring the whole set to fit
    // inside one season is an assumption that holds only while a set is small: twelve
    // discs of this anthology came to 31 titles against a 30-episode season 1, so no
    // arrangement fit, and the solver silently fell back to a longer season full of
    // shorts and produced 68-minute runtime deltas. Flattening removes the false
    // constraint; the real one — consecutive, non-overlapping, in disc order — is
    // unchanged and still does the work.
    let mut flat_episodes: Vec<Episode> = seasons
        .iter()
        .flat_map(|s| s.episodes.iter().cloned())
        .collect();
    flat_episodes.sort_by_key(|e| (e.season, e.number));
    if flat_episodes.is_empty() {
        return None;
    }

    let sc = SeasonScorer::new(&flat_episodes, &flat, params);
    let solved = place(&ordered, &sc, params.gap_penalty)?;
    let mean_cost = solved.mean_cost;
    let starts = solved.starts;
    let second_best = solved.runner_up;

    let episodes: &[Episode] = &flat_episodes;

    // Where disc order puts each title, as an index into `episodes`...
    let disc_order: Vec<usize> = ordered
        .iter()
        .zip(starts.iter())
        .flat_map(|(d, &start)| start..start + d.titles.len())
        .collect();
    // ...and where the dialogue says each one belongs among those same episodes.
    let window: Vec<&Episode> = disc_order.iter().map(|&i| &episodes[i]).collect();
    let reorder = reorder_by_dialogue(&flat, &window, |t, e| {
        sc.cost[t][disc_order[e]].is_infinite()
    });
    let assigned: Vec<usize> = match &reorder {
        Some(r) => r.order.iter().map(|&p| disc_order[p]).collect(),
        None => disc_order.clone(),
    };

    // Per-title verdicts: what each signal would have said on its own, so confidence can
    // rest on independent signals agreeing rather than on one number being small.
    let mut verdicts: Vec<signals::TitleVerdict> = Vec::new();
    let mut placements = Vec::new();
    let mut runtime_total = 0.0;
    let mut runtime_counted = 0usize;

    let mut title_offset = 0usize;
    for d in &ordered {
        let disc_episodes: Vec<&Episode> = (0..d.titles.len())
            .map(|i| &episodes[assigned[title_offset + i]])
            .collect();
        let matches: Vec<TitleMatch> = d
            .titles
            .iter()
            .zip(disc_episodes.iter().copied())
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
            let idx = assigned[title_offset + i];
            let scores = sc.scores[title_offset + i][idx];
            let runtime_pick = sc.best_by(title_offset + i, |s| Some(s.runtime)).unwrap_or(0);
            let subtitle_pick = t
                .dialogue
                .as_ref()
                .and_then(|_| sc.best_by(title_offset + i, |s| s.subtitle));
            let chosen = episodes[idx].number;
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
            if let Some(rt) = episodes[idx].runtime_mins {
                runtime_total += (t.duration_secs / 60.0 - f64::from(rt)).abs();
                runtime_counted += 1;
            }
        }

        placements.push(DiscPlacement {
            disc_number: d.disc_number,
            job_id: d.job_id.clone(),
            first_episode: disc_episodes.iter().map(|e| e.number).min().unwrap_or(0),
            mean_delta: mean_runtime_delta(&matches),
            matches,
        });
        title_offset += d.titles.len();
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
    // A reordering overrides the set constraint on the dialogue's word alone. It is the
    // right call when the captions are this clear, but it is exactly the kind of call a
    // person should look at.
    if reorder.is_some() && confidence > Confidence::Weak {
        confidence = Confidence::Weak;
    }

    // Which seasons the placement actually touched — a box set may cross them.
    let mut touched: Vec<u32> = placements
        .iter()
        .flat_map(|p| p.matches.iter().map(|m| m.episode.season))
        .collect();
    touched.sort_unstable();
    touched.dedup();
    let where_ = match touched.as_slice() {
        [] => "the series".to_string(),
        [one] => format!("season {one}"),
        many => format!(
            "seasons {}",
            many.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", ")
        ),
    };

    let mut evidence = vec![
        format!(
            "solved {} disc(s) together against {where_}, requiring consecutive \
             non-overlapping episodes in disc order",
            ordered.len()
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
        // Report corroboration of the answer, not agreement between the signals: the
        // latter reads as a problem when it is only the weaker signal being weak.
        let confirmed = verdicts.iter().filter(|v| v.subtitle_confirms()).count();
        let rt = verdicts.iter().filter(|v| v.runtime_confirms()).count();
        evidence.push(format!(
            "{with_dialogue} of {total_titles} title(s) had dialogue; choosing freely across \
             the whole season it reached this same placement for {confirmed} of them \
             (runtime alone: {rt})"
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
    if let Some(r) = &reorder {
        evidence.push(format!(
            "dialogue moved {} title(s) out of disc order (mean fit {:.1} standard \
             deviations better than in disc order) — the disc order did not match the \
             episode numbering",
            r.moves.len(),
            r.mean_gain
        ));
        let mut strongest = r.moves.clone();
        strongest.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        let disc_of: Vec<u32> = ordered
            .iter()
            .flat_map(|d| std::iter::repeat(d.disc_number).take(d.titles.len()))
            .collect();
        for &(t, _, fit) in strongest.iter().take(3) {
            let e = &episodes[assigned[t]];
            evidence.push(format!(
                "  disc {} {} is S{:02}E{:02} {}: its dialogue fits that synopsis {fit:.1} \
                 standard deviations above its other candidates",
                disc_of[t], flat[t].name, e.season, e.number, e.name
            ));
        }
    }
    evidence
        .push("solving the set jointly is still inference — confirm before filing".to_string());

    Some(SetSolution {
        season: touched.first().copied().unwrap_or(0),
        seasons: touched,
        placements,
        mean_delta,
        mean_cost,
        runner_up_cost: second_best,
        confidence,
        evidence,
        verdicts,
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

    /// A box set is laid out in release order and may cross season boundaries.
    ///
    /// Regression test for a real failure: twelve discs of an anthology came to 31 titles
    /// against a 30-episode season 1, so nothing fit, and solving season-by-season fell
    /// back to a longer season of shorts — producing 68-minute runtime deltas and placing
    /// every disc wrongly. Flattening lets the set straddle the boundary.
    #[test]
    fn a_set_may_span_two_seasons() {
        let s1 = Season {
            number: 1,
            episodes: vec![ep(1, 50, "S1E1"), ep(2, 50, "S1E2")],
        };
        let s2 = Season {
            number: 2,
            episodes: vec![
                Episode { season: 2, number: 1, name: "S2E1".into(), runtime_mins: Some(90), air_date: None, overview: None },
                Episode { season: 2, number: 2, name: "S2E2".into(), runtime_mins: Some(30), air_date: None, overview: None },
            ],
        };
        // Two discs whose four titles can only be satisfied by running off the end of
        // season 1 and into season 2.
        let discs = vec![
            SetDisc { disc_number: 1, job_id: "j1".into(), titles: vec![t("a.mkv", 50.0), t("b.mkv", 50.0)] },
            SetDisc { disc_number: 2, job_id: "j2".into(), titles: vec![t("c.mkv", 90.0), t("d.mkv", 30.0)] },
        ];

        let sol = solve(&discs, &[s1, s2]).expect("solved");
        assert_eq!(sol.seasons, vec![1, 2], "the set should straddle both seasons");
        let placed: Vec<(u32, u32)> = sol
            .placements
            .iter()
            .flat_map(|p| p.matches.iter().map(|m| (m.episode.season, m.episode.number)))
            .collect();
        assert_eq!(placed, vec![(1, 1), (1, 2), (2, 1), (2, 2)]);
        assert!(sol.mean_delta < 0.01, "every runtime should match exactly");
    }

    /// The common case must not regress: a set inside one season still reports that one
    /// season, not a spurious span.
    #[test]
    fn a_set_within_one_season_reports_just_that_season() {
        let season = Season {
            number: 1,
            episodes: vec![ep(1, 30, "A"), ep(2, 60, "B"), ep(3, 90, "C")],
        };
        let discs = vec![SetDisc {
            disc_number: 1,
            job_id: "j1".into(),
            titles: vec![t("a.mkv", 30.0), t("b.mkv", 60.0)],
        }];
        let sol = solve(&discs, &[season]).expect("solved");
        assert_eq!(sol.seasons, vec![1]);
        assert_eq!(sol.season, 1);
        assert!(sol.evidence.iter().any(|e| e.contains("season 1")));
    }

    /// The solve must stay cheap as the candidate list grows.
    ///
    /// Regression test for an out-of-memory kill. Enumerating arrangements was fine
    /// against one 30-episode season — twelve discs of 30 titles admit exactly one
    /// placement — but once the search widened to every episode of a series it became
    /// C(82, 12), around 10^14 placements. The process allocated 128 GB and the kernel
    /// killed it, taking the machine down with it. The DP that replaced it is
    /// O(discs x episodes); this test would not finish under the old code.
    #[test]
    fn a_large_candidate_list_solves_without_exploding() {
        let episodes: Vec<Episode> = (1..=400)
            .map(|n| Episode {
                season: 1 + (n / 50) as u32,
                number: n as u32,
                name: format!("Ep {n}"),
                // Distinctive runtimes so there is a single right answer to find.
                runtime_mins: Some(30 + (n % 7) as u32 * 10),
                air_date: None,
                overview: None,
            })
            .collect();
        let season = Season {
            number: 1,
            episodes,
        };

        // Twelve discs, thirty titles — the shape that killed the process.
        let sizes = [3, 3, 2, 2, 3, 2, 2, 3, 3, 2, 3, 2];
        let discs: Vec<SetDisc> = sizes
            .iter()
            .enumerate()
            .map(|(i, &k)| SetDisc {
                disc_number: i as u32 + 1,
                job_id: format!("j{i}"),
                titles: (0..k).map(|j| t(&format!("d{i}t{j}.mkv"), 45.0)).collect(),
            })
            .collect();

        let started = std::time::Instant::now();
        let sol = solve(&discs, &[season]).expect("solved");
        assert!(
            started.elapsed().as_secs() < 10,
            "took {:?}; the solve must not be super-linear",
            started.elapsed()
        );
        assert_eq!(sol.placements.len(), 12);
        // Placements must remain consecutive and non-overlapping in disc order.
        let mut last = None;
        for p in &sol.placements {
            let first = p.matches.first().unwrap().episode.number;
            if let Some(prev) = last {
                assert!(first > prev, "disc placements must not overlap or reorder");
            }
            last = Some(p.matches.last().unwrap().episode.number);
        }
    }

    /// A candidate list beyond the ceiling is refused, not attempted.
    #[test]
    fn an_absurd_candidate_list_is_refused() {
        let episodes: Vec<Episode> = (1..=(MAX_CANDIDATE_EPISODES as u32 + 1))
            .map(|n| Episode {
                season: 1,
                number: n,
                name: String::new(),
                runtime_mins: Some(45),
                air_date: None,
                overview: None,
            })
            .collect();
        let season = Season { number: 1, episodes };
        let discs = vec![SetDisc {
            disc_number: 1,
            job_id: "j".into(),
            titles: vec![t("a.mkv", 45.0)],
        }];
        assert!(solve(&discs, &[season]).is_none());
    }

    /// The discs of a box set run continuously, and a gap is evidence against.
    ///
    /// Regression test for a confirmed misplacement. Ten discs placed correctly and
    /// contiguously across S01E01-E25; the remaining two belonged on E26-E30. With gaps
    /// free, the solver skipped those five episodes and three whole seasons to reach a
    /// marginally better-scoring window in season 4 — and skipped an episode again
    /// inside it. Charging for skipped episodes makes the contiguous reading win.
    #[test]
    fn a_gap_between_discs_costs_something() {
        // Ten episodes; two discs of two titles each, all the same length so runtime
        // cannot choose. Only the gap penalty distinguishes the arrangements.
        let season = Season {
            number: 1,
            episodes: (1..=10).map(|n| ep(n, 50, &format!("Ep {n}"))).collect(),
        };
        let discs = vec![
            SetDisc { disc_number: 1, job_id: "j1".into(), titles: vec![t("a.mkv", 50.0), t("b.mkv", 50.0)] },
            SetDisc { disc_number: 2, job_id: "j2".into(), titles: vec![t("c.mkv", 50.0), t("d.mkv", 50.0)] },
        ];
        let sol = solve(&discs, &[season]).expect("solved");
        let placed: Vec<u32> = sol
            .placements
            .iter()
            .flat_map(|p| p.matches.iter().map(|m| m.episode.number))
            .collect();
        assert_eq!(placed, vec![1, 2, 3, 4], "discs should sit back to back");
    }

    /// The penalty must not become a hard rule: a set may legitimately begin part-way
    /// into a series, and nothing after the last disc should be charged for either.
    #[test]
    fn leading_and_trailing_gaps_are_free() {
        // Only episodes 4 and 5 have runtimes matching the disc, so the set must start
        // at 4 despite three unused episodes before it and five after.
        let mut episodes: Vec<Episode> = (1..=10).map(|n| ep(n, 20, &format!("Ep {n}"))).collect();
        episodes[3] = ep(4, 90, "Long A");
        episodes[4] = ep(5, 90, "Long B");
        let season = Season { number: 1, episodes };
        let discs = vec![SetDisc {
            disc_number: 1,
            job_id: "j1".into(),
            titles: vec![t("a.mkv", 90.0), t("b.mkv", 90.0)],
        }];
        let sol = solve(&discs, &[season]).expect("solved");
        let placed: Vec<u32> = sol.placements[0].matches.iter().map(|m| m.episode.number).collect();
        assert_eq!(placed, vec![4, 5], "starting late must not be penalised");
    }

    /// A gap is discouraged, not forbidden — where the evidence clearly requires one it
    /// must still be reachable.
    #[test]
    fn a_gap_is_still_possible_when_the_evidence_demands_it() {
        // Runtime pins both discs: only episodes 1-2 can hold the 50 minute titles and
        // only 8-9 can hold the 120 minute ones, so the gap between them is forced.
        // (Episodes 3-7 are far too short for either, and the veto excludes them.)
        let mut episodes: Vec<Episode> = (1..=10).map(|n| ep(n, 20, &format!("Ep {n}"))).collect();
        episodes[0] = ep(1, 50, "Mid A");
        episodes[1] = ep(2, 50, "Mid B");
        episodes[7] = ep(8, 120, "Long A");
        episodes[8] = ep(9, 120, "Long B");
        let season = Season { number: 1, episodes };
        let discs = vec![
            SetDisc { disc_number: 1, job_id: "j1".into(), titles: vec![t("a.mkv", 50.0), t("b.mkv", 50.0)] },
            SetDisc { disc_number: 2, job_id: "j2".into(), titles: vec![t("c.mkv", 120.0), t("d.mkv", 120.0)] },
        ];
        let sol = solve(&discs, &[season]).expect("solved");
        let placed: Vec<u32> = sol
            .placements
            .iter()
            .flat_map(|p| p.matches.iter().map(|m| m.episode.number))
            .collect();
        assert_eq!(placed, vec![1, 2, 8, 9]);
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
            dialogue: Some(dumo_core::text::count_words(dialogue)),
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

    #[test]
    fn min_cost_assignment_finds_the_optimum_not_the_greedy_pick() {
        // Greedy takes (0,0)=1 and is then forced into (1,1)=10, total 11; the optimum
        // is (0,1)+(1,0) = 2+2 = 4.
        let cost = vec![vec![1.0, 2.0], vec![2.0, 10.0]];
        assert_eq!(assign_min_cost(&cost), vec![1, 0]);
        let never = vec![vec![f64::INFINITY, 5.0], vec![1.0, 1.0]];
        assert_eq!(assign_min_cost(&never), vec![1, 0]);
    }

    #[test]
    fn story_names_drop_only_a_numeric_part_suffix() {
        assert_eq!(story_name("Ninja Quest (2)"), Some("Ninja Quest"));
        assert_eq!(
            story_name("Master Vile and the Metallic Armor (1)"),
            Some("Master Vile and the Metallic Armor")
        );
        assert_eq!(story_name("Follow that Cab!"), None);
        assert_eq!(story_name("The Return (Part Two)"), None);
    }

    /// A season of equal-length episodes, each with a monster only it names.
    fn monster_season() -> Season {
        let plots = [
            ("Day of the Dumpster", "Rita escapes the dumpster and sends Squatt"),
            ("Ninja Quest (1)", "The Rangers lose their powers to Rito"),
            ("Ninja Quest (2)", "The Rangers search for Ninjor and Rito"),
            ("Fourth Down and Long", "Centiback turns people into footballs"),
            ("Another Brick in the Wall", "The Brick Bully traps the Rangers"),
            ("A Chimp in Charge", "A chimp becomes the Sinister Simian"),
            ("I'm Dreaming of a White Ranger", "The Rangers save Christmas for Santa"),
            ("The Sound of Dischordia", "Dischordia puts the Rangers under a terrible tune"),
        ];
        Season {
            number: 3,
            episodes: plots
                .iter()
                .enumerate()
                .map(|(i, (name, plot))| Episode {
                    season: 3,
                    number: i as u32 + 1,
                    name: (*name).into(),
                    runtime_mins: Some(20),
                    air_date: None,
                    overview: Some((*plot).into()),
                })
                .collect(),
        }
    }

    /// A title whose captions name `words` among talk common to every episode.
    fn spoken(name: &str, words: &str) -> DiscTitle {
        let mut text = String::from("go go power rangers zordon alpha rangers morph ");
        for _ in 0..10 {
            text.push_str(words);
            text.push(' ');
        }
        DiscTitle {
            name: name.into(),
            duration_secs: 20.0 * 60.0,
            dialogue: Some(dumo_core::text::count_words(&text)),
        }
    }

    #[test]
    fn dialogue_moves_a_holiday_episode_authored_out_of_order() {
        // Broadcast order put the Christmas episode (E7) fourth. Every runtime is the
        // same, so disc order alone names titles 4-7 one episode early.
        let discs = vec![
            SetDisc {
                disc_number: 1,
                job_id: "a".into(),
                titles: vec![
                    spoken("t00", "dumpster squatt"),
                    spoken("t01", "rito powers ninja"),
                    spoken("t02", "ninjor rito temple"),
                    spoken("t03", "christmas santa presents"),
                ],
            },
            SetDisc {
                disc_number: 2,
                job_id: "b".into(),
                titles: vec![
                    spoken("t00", "centiback football"),
                    spoken("t01", "brick bully wall"),
                    spoken("t02", "chimp simian"),
                    spoken("t03", "dischordia tune"),
                ],
            },
        ];
        let s = solve(&discs, &[monster_season()]).expect("solved");
        let numbers: Vec<Vec<u32>> = s
            .placements
            .iter()
            .map(|p| p.matches.iter().map(|m| m.episode.number).collect())
            .collect();
        assert_eq!(numbers, vec![vec![1, 2, 3, 7], vec![4, 5, 6, 8]]);
        assert_eq!(s.placements[1].first_episode, 4);
        assert!(s.evidence.iter().any(|e| e.contains("out of disc order")));
        assert_eq!(s.confidence, Confidence::Weak, "a reordering is for a person to confirm");
    }

    #[test]
    fn dialogue_leaves_a_set_already_in_order_alone() {
        let discs = vec![
            SetDisc {
                disc_number: 1,
                job_id: "a".into(),
                titles: vec![
                    spoken("t00", "dumpster squatt"),
                    spoken("t01", "rito powers ninja"),
                    spoken("t02", "ninjor rito temple"),
                    spoken("t03", "centiback football"),
                ],
            },
            SetDisc {
                disc_number: 2,
                job_id: "b".into(),
                titles: vec![
                    spoken("t00", "brick bully wall"),
                    spoken("t01", "chimp simian"),
                    spoken("t02", "christmas santa presents"),
                    spoken("t03", "dischordia tune"),
                ],
            },
        ];
        let s = solve(&discs, &[monster_season()]).expect("solved");
        let numbers: Vec<u32> = s
            .placements
            .iter()
            .flat_map(|p| p.matches.iter().map(|m| m.episode.number))
            .collect();
        assert_eq!(numbers, (1..=8).collect::<Vec<_>>());
        assert!(!s.evidence.iter().any(|e| e.contains("out of disc order")));
    }

    #[test]
    fn parts_of_one_story_keep_disc_order_when_dialogue_cannot_separate_them() {
        // Both Ninja Quest parts say exactly the same words, which favour neither.
        let discs = vec![
            SetDisc {
                disc_number: 1,
                job_id: "a".into(),
                titles: vec![
                    spoken("t00", "dumpster squatt"),
                    spoken("t01", "christmas santa presents"),
                    spoken("t02", "rito ninjor"),
                    spoken("t03", "rito ninjor"),
                ],
            },
            SetDisc {
                disc_number: 2,
                job_id: "b".into(),
                titles: vec![
                    spoken("t00", "centiback football"),
                    spoken("t01", "brick bully wall"),
                    spoken("t02", "chimp simian"),
                    spoken("t03", "dischordia tune"),
                ],
            },
        ];
        let s = solve(&discs, &[monster_season()]).expect("solved");
        let d1: Vec<&str> = s.placements[0]
            .matches
            .iter()
            .map(|m| m.episode.name.as_str())
            .collect();
        assert_eq!(
            d1,
            vec![
                "Day of the Dumpster",
                "I'm Dreaming of a White Ranger",
                "Ninja Quest (1)",
                "Ninja Quest (2)"
            ]
        );
    }
}
