//! Matching a feature film disc to a TMDB movie.
//!
//! A movie disc offers far less to go on than a season: one title, one runtime and a
//! name. The name finds the candidates; the runtime is the check, since two films that
//! share a name (a remake, a foreign original) rarely share a running time to within a
//! few minutes. A disc's main title can still differ from the listed runtime when it
//! holds another cut, so a runtime miss is reported, not hidden, and leaves the match
//! weak rather than refusing it.

use crate::tmdb::MovieResult;
use dumo_core::Confidence;

/// Runtime agreement that counts as the same cut, in minutes, before the proportional
/// allowance below. TMDB lists whole minutes and some entries round loosely.
pub const RUNTIME_TOLERANCE_MINS: f64 = 4.0;

/// Proportional allowance, for long films whose listed runtime drifts further.
pub const RUNTIME_TOLERANCE_SHARE: f64 = 0.03;

/// How many search results are weighed. Past this, results are rarely the same film.
pub const CANDIDATES: usize = 5;

#[derive(Debug, Clone)]
pub struct MovieMatch {
    pub movie: MovieResult,
    /// Disc runtime minus listed runtime, in minutes; `None` when TMDB lists none.
    pub delta_mins: Option<f64>,
    /// Whether the chosen film's listed runtime agrees with the disc's.
    pub runtime_fits: bool,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
}

/// Split a trailing year off a search term: `"The Fly (1986)"` -> `("The Fly", 1986)`.
///
/// The way to say which of two same-named films is meant, in the same form the library
/// names files.
pub fn split_year(query: &str) -> (String, Option<u32>) {
    let q = query.trim();
    if let Some(open) = q.rfind('(') {
        let inner = q[open + 1..].trim_end_matches(')').trim();
        if q.ends_with(')') && inner.len() == 4 {
            if let Ok(y) = inner.parse::<u32>() {
                if (1880..=2100).contains(&y) {
                    return (q[..open].trim().to_string(), Some(y));
                }
            }
        }
    }
    (q.to_string(), None)
}

fn fits(title_mins: f64, listed: u32) -> bool {
    let listed = f64::from(listed);
    (title_mins - listed).abs() <= RUNTIME_TOLERANCE_MINS.max(listed * RUNTIME_TOLERANCE_SHARE)
}

/// Choose the film a disc's main title is.
///
/// `candidates` are in search-relevance order, with runtimes filled in. The first one
/// whose runtime fits is chosen; it is `strong` only when it is also the top result
/// and no other candidate fits as well, so a remake with the same running time can
/// never be picked quietly. With no runtime fit, the top result is proposed as `weak`
/// and the evidence says by how much it missed.
pub fn match_movie(title_mins: f64, candidates: &[MovieResult], year: Option<u32>) -> Option<MovieMatch> {
    let pool: Vec<&MovieResult> = candidates
        .iter()
        .filter(|m| year.map(|y| m.year() == Some(y)).unwrap_or(true))
        .take(CANDIDATES)
        .collect();
    let first = *pool.first()?;
    let fitting: Vec<&MovieResult> = pool
        .iter()
        .copied()
        .filter(|m| m.runtime_mins.map(|r| fits(title_mins, r)).unwrap_or(false))
        .collect();

    let delta = |m: &MovieResult| m.runtime_mins.map(|r| title_mins - f64::from(r));
    let mut evidence = Vec::new();
    if let Some(y) = year {
        evidence.push(format!("only films released in {y} considered, as asked"));
    }

    let (chosen, confidence) = match fitting.as_slice() {
        [] => {
            evidence.push(match first.runtime_mins {
                Some(r) => format!(
                    "no candidate's runtime fits: the top result lists {r} min against \
                     {title_mins:.0} min on the disc — another cut, or another film"
                ),
                None => "TMDB lists no runtime for the top result, so nothing checks it".into(),
            });
            (first, Confidence::Weak)
        }
        [only] => {
            let top = std::ptr::eq(*only, first);
            evidence.push(format!(
                "runtime fits: {title_mins:.0} min on the disc, {} min listed",
                only.runtime_mins.unwrap_or(0)
            ));
            if top {
                evidence.push("the top search result, and the only candidate whose runtime fits".into());
                (*only, Confidence::Strong)
            } else {
                evidence.push(
                    "not the top search result; chosen because it is the only runtime fit".into(),
                );
                (*only, Confidence::Weak)
            }
        }
        [best, rest @ ..] => {
            evidence.push(format!(
                "runtime fits {} min listed, but so do {} other candidate(s): {}",
                best.runtime_mins.unwrap_or(0),
                rest.len(),
                rest.iter()
                    .map(|m| format!(
                        "{} ({})",
                        m.title,
                        m.year().map(|y| y.to_string()).unwrap_or_else(|| "?".into())
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            evidence.push("add the year to --show, e.g. \"Title (1986)\", to choose".into());
            (*best, Confidence::Weak)
        }
    };
    Some(MovieMatch {
        movie: chosen.clone(),
        delta_mins: delta(chosen),
        runtime_fits: chosen.runtime_mins.map(|r| fits(title_mins, r)).unwrap_or(false),
        confidence,
        evidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn film(id: u64, title: &str, year: u32, runtime: Option<u32>) -> MovieResult {
        MovieResult {
            id,
            title: title.into(),
            release_date: Some(format!("{year}-06-01")),
            runtime_mins: runtime,
        }
    }

    #[test]
    fn splits_a_trailing_year() {
        assert_eq!(split_year("The Fly (1986)"), ("The Fly".to_string(), Some(1986)));
        assert_eq!(split_year("The Substance"), ("The Substance".to_string(), None));
        assert_eq!(split_year("Blade Runner (Final Cut)"), ("Blade Runner (Final Cut)".to_string(), None));
        assert_eq!(split_year("1917"), ("1917".to_string(), None));
    }

    #[test]
    fn the_top_result_that_alone_fits_is_strong() {
        let c = vec![film(1, "The Substance", 2024, Some(141)), film(2, "Substance", 2011, Some(88))];
        let m = match_movie(141.3, &c, None).unwrap();
        assert_eq!(m.movie.id, 1);
        assert_eq!(m.confidence, Confidence::Strong);
        assert!(m.runtime_fits);
    }

    #[test]
    fn two_films_with_the_same_runtime_stay_weak() {
        let c = vec![film(1, "The Fly", 1986, Some(96)), film(2, "The Fly", 1958, Some(94))];
        let m = match_movie(95.0, &c, None).unwrap();
        assert_eq!(m.confidence, Confidence::Weak);
        // The year settles it.
        let m = match_movie(95.0, &c, Some(1958)).unwrap();
        assert_eq!(m.movie.id, 2);
        assert_eq!(m.confidence, Confidence::Strong);
    }

    #[test]
    fn a_lower_result_chosen_on_runtime_is_weak() {
        let c = vec![film(1, "Dune", 2021, Some(155)), film(2, "Dune", 1984, Some(137))];
        let m = match_movie(137.0, &c, None).unwrap();
        assert_eq!(m.movie.id, 2);
        assert_eq!(m.confidence, Confidence::Weak);
    }

    #[test]
    fn no_runtime_fit_proposes_the_top_result_as_weak_and_says_why() {
        let c = vec![film(1, "Aliens", 1986, Some(137))];
        let m = match_movie(154.0, &c, None).unwrap();
        assert_eq!(m.movie.id, 1);
        assert_eq!(m.confidence, Confidence::Weak);
        assert!(m.evidence.iter().any(|e| e.contains("another cut")));
        assert!(!m.runtime_fits);
        assert_eq!(m.delta_mins, Some(17.0));
    }

    #[test]
    fn nothing_to_choose_from_is_none() {
        assert!(match_movie(100.0, &[], None).is_none());
        let c = vec![film(1, "The Fly", 1986, Some(96))];
        assert!(match_movie(96.0, &c, Some(1958)).is_none());
    }
}
