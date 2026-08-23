//! Structural analysis of a ripped video disc.
//!
//! A DVD rip arrives as a set of anonymous titles. Before any online lookup can help, we
//! need to know the disc's *shape*: is this one feature film, a set of episodes, or a
//! feature plus extras? That question is answerable offline, from duration and chapter
//! layout alone, and answering it first is what makes the online lookup tractable —
//! searching for "an episode around 51 minutes" is a far better query than "something".
//!
//! Everything here is a **heuristic**. Nothing it concludes is ever auto-accepted: per
//! the project's identification policy, only cryptographic matches qualify, and disc
//! shape is inference. The output is a proposal with its reasoning attached.

use dumo_core::Confidence;

/// What a single title on the disc appears to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleRole {
    /// A feature-length main title.
    Feature,
    /// One of several similar-length main titles.
    Episode,
    /// Short content: trailers, featurettes, menus, stingers.
    Extra,
}

impl std::fmt::Display for TitleRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TitleRole::Feature => "feature",
            TitleRole::Episode => "episode",
            TitleRole::Extra => "extra",
        })
    }
}

/// Overall shape of the disc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscShape {
    /// One feature-length title: a film.
    Movie,
    /// Several similar-length titles: episodes of a series.
    Series,
    /// Several main titles of clearly different lengths — could be a double feature, a
    /// film plus a long documentary, or a mixed compilation. Genuinely ambiguous.
    Mixed,
    /// Nothing of feature length; extras only.
    ExtrasOnly,
    Unknown,
}

impl std::fmt::Display for DiscShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DiscShape::Movie => "movie",
            DiscShape::Series => "series (episodes)",
            DiscShape::Mixed => "mixed / ambiguous",
            DiscShape::ExtrasOnly => "extras only",
            DiscShape::Unknown => "unknown",
        })
    }
}

/// The media category a disc's content should be filed under.
///
/// This is the routing key `migrate` uses, so it decides which share the content lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCategory {
    Movies,
    Tv,
}

impl MediaCategory {
    pub fn slug(self) -> &'static str {
        match self {
            MediaCategory::Movies => "movies",
            MediaCategory::Tv => "tv",
        }
    }
}

/// One analysed title.
#[derive(Debug, Clone)]
pub struct AnalysedTitle {
    /// Filename as staged.
    pub name: String,
    pub duration_secs: f64,
    pub chapters: usize,
    pub role: TitleRole,
    pub why: String,
}

impl AnalysedTitle {
    pub fn duration_hms(&self) -> String {
        let s = self.duration_secs.round() as u64;
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    }
}

/// The result of analysing a whole disc.
#[derive(Debug, Clone)]
pub struct DiscAnalysis {
    pub shape: DiscShape,
    /// Suggested category, when the shape implies one.
    pub category: Option<MediaCategory>,
    pub confidence: Confidence,
    pub titles: Vec<AnalysedTitle>,
    pub evidence: Vec<String>,
}

impl DiscAnalysis {
    pub fn main_titles(&self) -> impl Iterator<Item = &AnalysedTitle> {
        self.titles.iter().filter(|t| t.role != TitleRole::Extra)
    }

    pub fn extras(&self) -> impl Iterator<Item = &AnalysedTitle> {
        self.titles.iter().filter(|t| t.role == TitleRole::Extra)
    }
}

/// Tunables for the analysis.
#[derive(Debug, Clone, Copy)]
pub struct AnalysisParams {
    /// Titles shorter than this are extras rather than content.
    pub min_main_secs: f64,
    /// Below this, even a chaptered title is treated as an extra.
    pub max_extra_secs: f64,
    /// Main titles within this fraction of the median count as "the same length",
    /// which is what distinguishes an episode set from a double feature.
    pub episode_tolerance: f64,
    /// A single title at least this long is a feature rather than a long episode.
    pub feature_secs: f64,
}

impl Default for AnalysisParams {
    fn default() -> Self {
        Self {
            // 15 minutes: longer than any trailer or featurette, shorter than a
            // half-hour episode minus adverts.
            min_main_secs: 15.0 * 60.0,
            max_extra_secs: 15.0 * 60.0,
            // Episodes of one series vary by a few minutes; a film and a documentary
            // on the same disc differ by far more.
            episode_tolerance: 0.20,
            // 75 minutes is comfortably past TV episode length.
            feature_secs: 75.0 * 60.0,
        }
    }
}

/// Input for analysis: one staged title.
#[derive(Debug, Clone)]
pub struct TitleInput {
    pub name: String,
    pub duration_secs: f64,
    pub chapters: usize,
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    if v.len() % 2 == 0 {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    }
}

/// Work out what a disc contains from its titles' durations and chapter layout.
pub fn analyse(titles: &[TitleInput], params: AnalysisParams) -> DiscAnalysis {
    let mut analysed: Vec<AnalysedTitle> = Vec::new();
    let mut evidence: Vec<String> = Vec::new();

    if titles.is_empty() {
        return DiscAnalysis {
            shape: DiscShape::Unknown,
            category: None,
            confidence: Confidence::Unknown,
            titles: analysed,
            evidence: vec!["no titles to analyse".into()],
        };
    }

    // Length is the first filter, but not the only one.
    //
    // Authored content carries chapter markers; bonus features usually do not. That is
    // only worth acting on when the disc itself demonstrates the distinction — if *some*
    // titles here have chapters and others have none, the authoring made a deliberate
    // split and the chapterless ones are extras whatever their length. Where no title has
    // chapters, the signal says nothing and length decides alone.
    //
    // The case this exists for: a 17-minute chapterless featurette sitting beside a
    // 104-minute 8-chapter film and a 52-minute 4-chapter episode. On length alone it
    // cleared the 15-minute threshold and was counted as an episode, which pushed a
    // box set to 31 titles against a 30-episode season and made the whole set unsolvable.
    let long_enough: Vec<&TitleInput> = titles
        .iter()
        .filter(|t| t.duration_secs >= params.min_main_secs)
        .collect();
    let chaptering_is_meaningful = long_enough.iter().any(|t| t.chapters > 0)
        && long_enough.iter().any(|t| t.chapters == 0);

    let is_content = |t: &TitleInput| -> bool {
        t.duration_secs >= params.min_main_secs && !(chaptering_is_meaningful && t.chapters == 0)
    };

    let mains: Vec<&TitleInput> = titles.iter().filter(|t| is_content(t)).collect();

    let main_durations: Vec<f64> = mains.iter().map(|t| t.duration_secs).collect();
    let med = median(main_durations.clone());

    // Are the main titles all about the same length?
    //
    // Measured as spread across the whole set — (max - min) / max — rather than each
    // item's distance from the median. With only two titles the median sits exactly
    // between them, so a 102 min film and a 69 min documentary each land 19% from it and
    // would pass a 20% median test despite differing by a third. Spread does not have
    // that blind spot.
    let lo = main_durations.iter().cloned().fold(f64::MAX, f64::min);
    let hi = main_durations.iter().cloned().fold(0.0_f64, f64::max);
    let spread = if hi > 0.0 { (hi - lo) / hi } else { 0.0 };
    let uniform = mains.len() > 1 && spread <= params.episode_tolerance;

    let (shape, category, confidence) = match mains.len() {
        0 => (DiscShape::ExtrasOnly, None, Confidence::Weak),
        1 => {
            let d = main_durations[0];
            if d >= params.feature_secs {
                (DiscShape::Movie, Some(MediaCategory::Movies), Confidence::Strong)
            } else {
                // A single ~45 minute title is more likely one episode than a film.
                (DiscShape::Mixed, None, Confidence::Weak)
            }
        }
        _ if uniform => (DiscShape::Series, Some(MediaCategory::Tv), Confidence::Strong),
        _ => (DiscShape::Mixed, None, Confidence::Weak),
    };

    for t in titles {
        let is_extra = !is_content(t);
        let short = t.duration_secs < params.min_main_secs;
        let role = if is_extra {
            TitleRole::Extra
        } else {
            match shape {
                DiscShape::Series => TitleRole::Episode,
                _ => TitleRole::Feature,
            }
        };
        let why = if is_extra && !short {
            format!(
                "{:.0} min but no chapters, where other titles here have them — \
                 authored as an extra",
                t.duration_secs / 60.0
            )
        } else if is_extra {
            format!(
                "{:.0} min is under the {:.0} min content threshold{}",
                t.duration_secs / 60.0,
                params.min_main_secs / 60.0,
                if t.chapters == 0 {
                    ", and it has no chapters"
                } else {
                    ""
                }
            )
        } else {
            format!(
                "{:.0} min with {} chapter(s)",
                t.duration_secs / 60.0,
                t.chapters
            )
        };
        analysed.push(AnalysedTitle {
            name: t.name.clone(),
            duration_secs: t.duration_secs,
            chapters: t.chapters,
            role,
            why,
        });
    }

    // Explain the conclusion in the same terms a person would use.
    evidence.push(format!(
        "{} title(s): {} of content length, {} shorter",
        titles.len(),
        mains.len(),
        titles.len() - mains.len()
    ));

    match shape {
        DiscShape::Series => {
            evidence.push(format!(
                "main titles run {:.0}–{:.0} min, a spread of {:.0}% (within {:.0}%) — \
                 consistent with episodes of one series",
                lo / 60.0,
                hi / 60.0,
                spread * 100.0,
                params.episode_tolerance * 100.0
            ));
        }
        DiscShape::Movie => evidence.push(format!(
            "a single {:.0} min title, past the {:.0} min feature threshold",
            med / 60.0,
            params.feature_secs / 60.0
        )),
        DiscShape::Mixed if mains.len() > 1 => {
            evidence.push(format!(
                "main titles run {:.0}–{:.0} min, a spread of {:.0}% — too varied to be one \
                 episode set; could be a double feature, or a feature plus a long extra",
                lo / 60.0,
                hi / 60.0,
                spread * 100.0
            ));
        }
        DiscShape::Mixed => evidence.push(format!(
            "a single {:.0} min title: too short to assume a feature, too long to be an extra",
            med / 60.0
        )),
        DiscShape::ExtrasOnly => evidence.push(format!(
            "nothing reaches the {:.0} min content threshold",
            params.min_main_secs / 60.0
        )),
        DiscShape::Unknown => {}
    }

    let chaptered = analysed
        .iter()
        .filter(|t| t.role != TitleRole::Extra && t.chapters > 0)
        .count();
    if chaptered > 0 {
        evidence.push(format!(
            "{chaptered} main title(s) carry chapter markers, as authored content does"
        ));
    }

    evidence.push(
        "structural inference only — never auto-accepted; confirm before filing".to_string(),
    );

    DiscAnalysis {
        shape,
        category,
        confidence,
        titles: analysed,
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str, mins: f64, chapters: usize) -> TitleInput {
        TitleInput {
            name: name.into(),
            duration_secs: mins * 60.0,
            chapters,
        }
    }

    /// Real data from ESPN 30 for 30 disc 1: three ~51 min titles plus two short extras.
    #[test]
    fn recognises_an_episode_disc() {
        let a = analyse(
            &[
                t("B1_t01.mkv", 51.2, 4),
                t("C5_t00.mkv", 7.1, 0),
                t("D1_t02.mkv", 54.0, 4),
                t("E1_t04.mkv", 51.9, 4),
                t("F5_t03.mkv", 6.0, 0),
            ],
            AnalysisParams::default(),
        );
        assert_eq!(a.shape, DiscShape::Series);
        assert_eq!(a.category, Some(MediaCategory::Tv));
        assert_eq!(a.main_titles().count(), 3);
        assert_eq!(a.extras().count(), 2);
        assert!(a.main_titles().all(|t| t.role == TitleRole::Episode));
    }

    /// Real data from disc 3: 102 min and 69 min. Too different to be one episode set,
    /// so the honest answer is "ambiguous", not a guess.
    #[test]
    fn flags_a_mixed_disc_rather_than_guessing() {
        let a = analyse(
            &[
                t("B1_t00.mkv", 102.5, 6),
                t("D1_t01.mkv", 69.3, 5),
                t("I3_t02.mkv", 6.3, 0),
            ],
            AnalysisParams::default(),
        );
        assert_eq!(a.shape, DiscShape::Mixed);
        assert_eq!(a.category, None, "must not pick a category when ambiguous");
        assert_eq!(a.confidence, Confidence::Weak);
    }

    /// Regression test for the median pitfall: with two titles the median sits midway
    /// between them, so each is equidistant and a median-based tolerance wrongly passes.
    /// Spread across the set is the correct measure.
    #[test]
    fn two_very_different_titles_are_not_an_episode_set() {
        let a = analyse(
            &[t("long.mkv", 102.0, 6), t("short.mkv", 69.0, 5)],
            AnalysisParams::default(),
        );
        assert_eq!(a.shape, DiscShape::Mixed);
    }

    #[test]
    fn recognises_a_single_feature_as_a_movie() {
        let a = analyse(
            &[t("main.mkv", 118.0, 24), t("trailer.mkv", 2.0, 0)],
            AnalysisParams::default(),
        );
        assert_eq!(a.shape, DiscShape::Movie);
        assert_eq!(a.category, Some(MediaCategory::Movies));
        assert_eq!(a.main_titles().count(), 1);
    }

    /// A lone 45 minute title is more likely one episode than a film, so it must not be
    /// filed as a movie on length alone.
    #[test]
    fn a_single_short_title_is_ambiguous_not_a_movie() {
        let a = analyse(&[t("only.mkv", 45.0, 4)], AnalysisParams::default());
        assert_eq!(a.shape, DiscShape::Mixed);
        assert_eq!(a.category, None);
    }

    #[test]
    fn a_disc_of_only_shorts_is_extras_only() {
        let a = analyse(
            &[t("a.mkv", 5.0, 0), t("b.mkv", 8.0, 0)],
            AnalysisParams::default(),
        );
        assert_eq!(a.shape, DiscShape::ExtrasOnly);
        assert_eq!(a.main_titles().count(), 0);
    }

    #[test]
    fn no_titles_is_unknown() {
        let a = analyse(&[], AnalysisParams::default());
        assert_eq!(a.shape, DiscShape::Unknown);
        assert_eq!(a.confidence, Confidence::Unknown);
    }

    /// Half-hour comedies: shorter, but still a uniform set.
    #[test]
    fn recognises_short_form_episodes() {
        let a = analyse(
            &[
                t("e1.mkv", 22.0, 3),
                t("e2.mkv", 23.0, 3),
                t("e3.mkv", 22.5, 3),
                t("e4.mkv", 21.8, 3),
            ],
            AnalysisParams::default(),
        );
        assert_eq!(a.shape, DiscShape::Series);
        assert_eq!(a.main_titles().count(), 4);
    }

    /// Nothing here may ever be auto-accepted, however confident it looks.
    #[test]
    fn analysis_is_never_auto_acceptable() {
        let a = analyse(
            &[t("a.mkv", 51.0, 4), t("b.mkv", 52.0, 4)],
            AnalysisParams::default(),
        );
        assert_eq!(a.shape, DiscShape::Series);
        assert!(
            !a.confidence.is_auto_acceptable(),
            "structural inference must always require confirmation"
        );
    }

    #[test]
    fn evidence_is_always_populated() {
        let a = analyse(&[t("a.mkv", 51.0, 4)], AnalysisParams::default());
        assert!(!a.evidence.is_empty());
    }

    #[test]
    fn median_handles_even_and_odd_counts() {
        assert_eq!(median(vec![1.0, 3.0]), 2.0);
        assert_eq!(median(vec![1.0, 2.0, 9.0]), 2.0);
        assert_eq!(median(vec![]), 0.0);
    }

    /// Chapter markers separate authored content from bonus features.
    ///
    /// Regression test for a real misclassification: a 17-minute chapterless featurette
    /// sat beside a 104-minute 8-chapter film and a 52-minute 4-chapter episode. It
    /// cleared the 15-minute length threshold and was counted as content, which pushed a
    /// twelve-disc box set to 31 titles against a 30-episode season and made the whole
    /// set unsolvable.
    #[test]
    fn a_long_chapterless_title_is_an_extra_when_the_disc_uses_chapters() {
        let titles = vec![
            t("C7_t00.mkv", 17.2, 0),
            t("B1_t01.mkv", 103.7, 8),
            t("D1_t02.mkv", 51.5, 4),
        ];
        let a = analyse(&titles, AnalysisParams::default());
        let by = |n: &str| a.titles.iter().find(|x| x.name == n).unwrap();

        assert_eq!(by("C7_t00.mkv").role, TitleRole::Extra, "17 min, no chapters");
        assert_ne!(by("B1_t01.mkv").role, TitleRole::Extra);
        assert_ne!(by("D1_t02.mkv").role, TitleRole::Extra);
        assert_eq!(a.main_titles().count(), 2);
        assert!(
            by("C7_t00.mkv").why.contains("no chapters"),
            "the reason should name the evidence: {}",
            by("C7_t00.mkv").why
        );
    }

    /// Where nothing has chapters the signal says nothing, so length must decide alone —
    /// otherwise a disc authored without any chapter markers would lose every title.
    #[test]
    fn chapterless_discs_still_classify_on_length() {
        let titles = vec![t("a.mkv", 52.0, 0), t("b.mkv", 51.0, 0), t("c.mkv", 6.0, 0)];
        let a = analyse(&titles, AnalysisParams::default());
        assert_eq!(a.main_titles().count(), 2, "the two long titles are content");
    }

    /// And a disc where everything has chapters is unaffected.
    #[test]
    fn discs_where_everything_has_chapters_are_unaffected() {
        let titles = vec![t("a.mkv", 52.0, 4), t("b.mkv", 51.0, 4)];
        let a = analyse(&titles, AnalysisParams::default());
        assert_eq!(a.main_titles().count(), 2);
    }
}
