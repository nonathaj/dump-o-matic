//! Checking a proposed episode identification against reference subtitles.
//!
//! Runtimes and synopses say which episode a title most plausibly is; a subtitle made
//! from the broadcast says what was actually said in it. When OpenSubtitles credentials
//! are configured, every proposed title's own captions are compared with a reference
//! subtitle of the episode it was proposed as, and a title whose dialogue turns out to
//! belong to a different episode stops the filing.
//!
//! References are cached under the staging root, one file per episode, so each is
//! downloaded once however often a set is re-identified — the daily download
//! allowance is small.

use anyhow::Result;
use dumo_core::config::Config;
use dumo_identify::matching::TitleMatch;
use dumo_identify::opensubtitles::{self, OpenSubtitlesClient, OpenSubtitlesError};
use dumo_identify::transcript::{self, Phrases, Verdict};
use std::path::{Path, PathBuf};

/// Reference language. Disc captions on the sets this has met are English; making it
/// configurable waits for a disc that is not.
const LANGUAGE: &str = "en";

/// Same window the dialogue scoring reads: the whole caption track.
const CAPTION_WINDOW_SECS: u32 = 4 * 3600;

/// What the check concluded for a whole proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Not configured, or nothing could be checked. Filing proceeds as it would have.
    NotChecked,
    /// Every checked title confirmed and none contradicted.
    Confirmed { checked: usize, of: usize },
    /// Some checked, none contradicted, not all confirmed.
    Partial { confirmed: usize, of: usize },
    /// At least one title's dialogue belongs to another episode. Do not file.
    Contradicted,
}

/// Check each `(job id, proposed match)` against reference subtitles of series `tv_id`.
pub fn check(cfg: &Config, tv_id: u64, proposals: &[(&str, &TitleMatch)]) -> Result<Outcome> {
    let client = match OpenSubtitlesClient::from_config(&cfg.api) {
        Ok(c) => c,
        Err(OpenSubtitlesError::NoCredential) => {
            println!("Reference subtitles: not configured, so not checked.");
            return Ok(Outcome::NotChecked);
        }
        Err(e) => return Err(e.into()),
    };
    println!("Checking against reference subtitles ...");

    // One reference per distinct episode, in first-seen order.
    let mut episodes: Vec<(u32, u32)> = Vec::new();
    for (_, m) in proposals {
        let key = (m.episode.season, m.episode.number);
        if !episodes.contains(&key) {
            episodes.push(key);
        }
    }
    let mut quota_hit = false;
    let mut references: Vec<Option<Phrases>> = Vec::new();
    for &(season, number) in &episodes {
        let text = if quota_hit {
            cached(cfg, tv_id, season, number)
        } else {
            match reference(cfg, &client, tv_id, season, number) {
                Ok(t) => t,
                Err(OpenSubtitlesError::QuotaExhausted) => {
                    println!(
                        "  the download allowance ran out; episodes without a cached \
                         reference are not checked this time"
                    );
                    quota_hit = true;
                    cached(cfg, tv_id, season, number)
                }
                Err(e) => {
                    println!("  S{season:02}E{number:02}: no reference ({e})");
                    None
                }
            }
        };
        references.push(text.map(|t| {
            Phrases::from_text(&dumo_backends::subtitles::dialogue_text(&t))
        }));
    }
    let names: Vec<String> = episodes
        .iter()
        .map(|(s, e)| format!("S{s:02}E{e:02}"))
        .collect();

    let mut confirmed = 0;
    let mut checked = 0;
    let mut contradicted = false;
    for (job_id, m) in proposals {
        let key = (m.episode.season, m.episode.number);
        let idx = episodes.iter().position(|e| *e == key).unwrap_or(0);
        let path = title_path(cfg, job_id, &m.title_name);
        let title = dumo_backends::subtitles::extract_text(&path, CAPTION_WINDOW_SECS)
            .map(|srt| Phrases::from_text(&dumo_backends::subtitles::dialogue_text(&srt)))
            .unwrap_or_else(|_| Phrases::from_text(""));
        let verdict = transcript::check(&title, &references, idx);
        match verdict {
            Verdict::Confirmed { .. } => {
                confirmed += 1;
                checked += 1;
            }
            Verdict::Contradicted { .. } => {
                contradicted = true;
                checked += 1;
            }
            Verdict::Inconclusive { .. } => checked += 1,
            Verdict::Unchecked => {}
        }
        println!(
            "  {:<16} as {}: {}",
            m.title_name,
            names[idx],
            verdict.describe(&names)
        );
    }

    Ok(if contradicted {
        Outcome::Contradicted
    } else if checked == 0 {
        Outcome::NotChecked
    } else if confirmed == proposals.len() {
        Outcome::Confirmed {
            checked,
            of: proposals.len(),
        }
    } else {
        Outcome::Partial {
            confirmed,
            of: proposals.len(),
        }
    })
}

/// Print what an outcome means for filing, and say whether filing may go ahead.
pub fn permits_filing(outcome: Outcome) -> bool {
    match outcome {
        Outcome::Contradicted => {
            println!();
            println!(
                "Not filing: reference subtitles say at least one title above is a different \
                 episode from the one proposed. Nothing was moved."
            );
            false
        }
        Outcome::Confirmed { checked, of } => {
            println!("Reference subtitles confirm all {checked} of {of} title(s).");
            true
        }
        Outcome::Partial { confirmed, of } => {
            println!(
                "Reference subtitles confirm {confirmed} of {of} title(s) and contradict none."
            );
            true
        }
        Outcome::NotChecked => true,
    }
}

/// Where a ripped title lives in its job: under `raw/`, or at the top level for jobs
/// laid out before `raw/` existed. The same rule filing uses.
fn title_path(cfg: &Config, job_id: &str, title_name: &str) -> PathBuf {
    let job_dir = cfg.staging.job_dir(job_id);
    let raw = job_dir.join("raw").join(title_name);
    if raw.is_file() {
        raw
    } else {
        job_dir.join(title_name)
    }
}

fn cache_path(cfg: &Config, tv_id: u64, season: u32, number: u32) -> PathBuf {
    cfg.staging
        .root
        .join("cache")
        .join("subtitles")
        .join(format!("tmdb-tv-{tv_id}"))
        .join(format!("S{season:02}E{number:02}.{LANGUAGE}.srt"))
}

fn cached(cfg: &Config, tv_id: u64, season: u32, number: u32) -> Option<String> {
    std::fs::read_to_string(cache_path(cfg, tv_id, season, number)).ok()
}

/// The reference subtitle for one episode: from the cache, or searched for, downloaded
/// and cached. `Ok(None)` when no usable subtitle exists for it.
fn reference(
    cfg: &Config,
    client: &OpenSubtitlesClient,
    tv_id: u64,
    season: u32,
    number: u32,
) -> std::result::Result<Option<String>, OpenSubtitlesError> {
    if let Some(t) = cached(cfg, tv_id, season, number) {
        return Ok(Some(t));
    }
    let files = client.search_episode(tv_id, season, number, LANGUAGE)?;
    let Some(best) = opensubtitles::pick_best(&files) else {
        return Ok(None);
    };
    let text = client.download(best.file_id)?;
    // A cache write failing loses nothing but a future download, so it is reported and
    // the reference is still used.
    if let Err(e) = write_cache(&cache_path(cfg, tv_id, season, number), &text) {
        println!("  (could not cache the S{season:02}E{number:02} reference: {e})");
    }
    Ok(Some(text))
}

/// Write through a temporary name, so an interrupted write never leaves a truncated
/// reference that would later be read as a whole one.
fn write_cache(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("srt.partial");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_write_is_whole_or_absent() {
        let dir = std::env::temp_dir().join(format!("dumo-refsubs-{}", std::process::id()));
        let path = dir.join("tmdb-tv-1").join("S01E02.en.srt");
        write_cache(&path, "1\n00:00:01,000 --> 00:00:02,000\nHello\n").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("Hello"));
        assert!(!path.with_extension("srt.partial").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_contradiction_stops_filing() {
        assert!(!permits_filing(Outcome::Contradicted));
        assert!(permits_filing(Outcome::NotChecked));
        assert!(permits_filing(Outcome::Partial { confirmed: 1, of: 3 }));
        assert!(permits_filing(Outcome::Confirmed { checked: 3, of: 3 }));
    }
}
