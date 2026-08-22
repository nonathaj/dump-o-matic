//! The `identify` command: Stage 3, work out what a staged job actually contains.
//!
//! Read-only. It hashes staged artifacts, matches them against datfiles, and prints the
//! proposed naming — it does not rename or move anything. Applying the result is a
//! separate, explicit step.

use anyhow::{bail, Context, Result};
use dumo_core::config::Config;
use dumo_core::job::Job;
use dumo_core::{hash, Confidence, MediaKind};
use dumo_identify::{es_de_slug, DatfileSet};
use std::io::Write;
use std::path::PathBuf;

pub struct IdentifyArgs {
    /// Job id or fragment. Omit to identify every job.
    pub job: Option<String>,
    pub config_file: Option<PathBuf>,
    /// Move exactly-matched files into `ready/` under their canonical names.
    pub apply: bool,
    /// Search term for a video disc, overriding the guess from the volume label.
    pub show: Option<String>,
    /// Solve all matching video discs together as one set.
    pub set: bool,
}

pub fn run(args: IdentifyArgs) -> Result<()> {
    let cfg = Config::load(args.config_file.as_deref())?;

    let Some(dat_dir) = cfg.datfiles.redump_dir.clone() else {
        bail!(
            "no datfile directory configured.\n\
             Set datfiles.redump_dir in {} to a directory of Redump .dat files.",
            cfg.source_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "your config".into())
        );
    };

    print!("Loading datfiles from {} ... ", dat_dir.display());
    std::io::stdout().flush().ok();
    let set = DatfileSet::load_dir(&dat_dir)
        .with_context(|| format!("loading datfiles from {}", dat_dir.display()))?;

    if set.is_empty() {
        bail!("no .dat files found in {}", dat_dir.display());
    }
    println!("{} games across {} datfile(s)", set.game_count(), set.datfiles().len());
    for d in set.datfiles() {
        println!(
            "  {} ({} games{})",
            d.platform,
            d.games.len(),
            d.version
                .as_ref()
                .map(|v| format!(", {v}"))
                .unwrap_or_default()
        );
    }
    println!();

    let jobs_dir = cfg.staging.jobs_dir();
    if !jobs_dir.is_dir() {
        bail!("no jobs directory at {}", jobs_dir.display());
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&jobs_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    if let Some(filter) = &args.job {
        dirs.retain(|d| {
            d.file_name()
                .map(|n| n.to_string_lossy().contains(filter.as_str()))
                .unwrap_or(false)
        });
        if dirs.is_empty() {
            bail!("no job matching {filter:?}");
        }
    }

    if args.apply {
        println!("Apply mode: exact matches will be moved into {}", cfg.staging.ready_dir().display());
        println!();
    }

    if args.set {
        return solve_video_set(&dirs, &cfg, args.show.as_deref());
    }

    for dir in dirs {
        identify_job(&dir, &set, &cfg, args.apply, args.show.as_deref())?;
        println!();
    }
    Ok(())
}

fn identify_job(
    job_dir: &std::path::Path,
    set: &DatfileSet,
    cfg: &Config,
    apply: bool,
    show: Option<&str>,
) -> Result<()> {
    let mut job = Job::load(job_dir)?;
    println!("Job {} ({})", job.id, job.stage);

    let kind = job
        .probe
        .as_ref()
        .map(|p| p.content.kind)
        .unwrap_or(MediaKind::Unknown);

    // Datfile matching only applies to disc images. Video rips need an entirely
    // different (and fuzzy) identification path, which is not built yet.
    if matches!(kind, MediaKind::DvdVideo | MediaKind::BluRayVideo) {
        return analyse_video(&job, job_dir, cfg, show.as_deref());
    }
    if !matches!(kind, MediaKind::GameDisc | MediaKind::Data) {
        println!("  {kind} — no identification path for this media type yet.");
        return Ok(());
    }

    // Only content files are worth matching; redumper's sidecars are not in datfiles.
    let candidates: Vec<_> = job
        .artifacts
        .iter()
        .filter(|a| {
            let p = a.relative_path.to_ascii_lowercase();
            p.ends_with(".iso") || p.ends_with(".bin") || p.ends_with(".cue")
        })
        .collect();

    if candidates.is_empty() {
        println!("  no disc images among the artifacts");
        return Ok(());
    }

    // Collected so a multi-file set is only applied once every file is accounted for.
    let mut applicable: Vec<(PathBuf, String, dumo_identify::datfile::DatMatch)> = Vec::new();

    for a in candidates {
        let path = job_dir.join(&a.relative_path);
        if !path.is_file() {
            println!("  {} — MISSING", a.relative_path);
            continue;
        }

        print!("  {} — hashing ... ", a.relative_path);
        std::io::stdout().flush().ok();
        let digests = hash::redump_digests(&path)
            .with_context(|| format!("hashing {}", path.display()))?;
        print!("\r  {} — sha1 {}\n", a.relative_path, digests.sha1);

        match set.find(&digests) {
            Some(m) => {
                // A hash can only collide with a size mismatch through corruption or a
                // datfile error; either way it is not a clean identification.
                if m.rom.size != digests.size {
                    println!(
                        "    WARNING: hash matched but size differs ({} vs {} in datfile)",
                        digests.size, m.rom.size
                    );
                    println!("    Treating as unidentified; this needs investigation.");
                    continue;
                }

                println!("    MATCH: {}", m.game.name);
                println!("      platform:   {}", m.platform);
                println!("      matched on: {} (exact)", m.matched_on);
                println!(
                    "      category:   {}",
                    m.game.category.as_deref().unwrap_or("unspecified")
                );
                println!("      datfile:    {}", file_name(&m.source));
                println!(
                    "      confidence: {}{}",
                    m.confidence,
                    if m.confidence.is_auto_acceptable() {
                        "  (eligible for unattended acceptance)"
                    } else {
                        "  (needs confirmation)"
                    }
                );

                if !m.is_complete_set() {
                    println!(
                        "      NOTE: this title has {} files in the datfile; only this one\n            \
                         is verified so far, so the set is incomplete.",
                        m.game.roms.len()
                    );
                    for r in &m.game.roms {
                        println!("              - {}", r.name);
                    }
                }

                // Proposed destination: Redump filename inside the ES-DE platform dir.
                match es_de_slug(&m.platform) {
                    Some(slug) => {
                        println!("      proposed:   {slug}/{}", m.rom.name);
                        applicable.push((path.clone(), a.sha256.clone(), m));
                    }
                    None => {
                        println!(
                            "      proposed:   <unknown platform directory for {:?}>",
                            m.platform
                        );
                        println!(
                            "                  add a mapping before this can be filed."
                        );
                    }
                }
            }
            None => {
                println!("    NO MATCH in any loaded datfile.");
                println!("      This can mean: a disc Redump has not catalogued, a");
                println!("      different revision, a bad dump, or the wrong datfile set.");
                if let Some(g) = job.probe.as_ref().and_then(|p| p.game_serial.as_ref()) {
                    println!("      Disc reports serial {} ({})", g.serial, g.platform);
                }
                println!("      Confidence: {} — manual identification required.", Confidence::Unknown);
            }
        }
    }

    if apply {
        apply_matches(&mut job, job_dir, cfg, &applicable)?;
    } else if !applicable.is_empty() {
        println!("  (re-run with --apply to move these into ready/)");
    }

    Ok(())
}

/// Move exactly-matched files into `ready/<slug>/<canonical name>`.
///
/// Only exact matches are moved, and only when nothing already occupies the destination.
/// The move itself upholds the data-safety invariant: within one filesystem it is an
/// atomic rename that copies nothing and deletes nothing; across filesystems it copies,
/// verifies by re-reading the destination, and only then removes the source.
fn apply_matches(
    job: &mut Job,
    job_dir: &std::path::Path,
    cfg: &Config,
    matches: &[(PathBuf, String, dumo_identify::datfile::DatMatch)],
) -> Result<()> {
    if matches.is_empty() {
        return Ok(());
    }

    // Guard against filing an incomplete multi-file set as though it were whole.
    if let Some((_, _, first)) = matches.first() {
        if !first.is_complete_set() && matches.len() < first.game.roms.len() {
            println!(
                "  NOT APPLYING: {} needs {} files but only {} are present here.",
                first.game.name,
                first.game.roms.len(),
                matches.len()
            );
            println!("  Filing a partial set would misrepresent it as a complete dump.");
            return Ok(());
        }
    }

    // Disc images are the 'games' category; the category is the routing key
    // migrate uses to pick a destination.
    let ready_root = cfg.staging.ready_category_dir("games");
    let mut moved: Vec<dumo_core::ReadyFile> = Vec::new();
    let mut reference: Option<dumo_identify::datfile::DatMatch> = None;

    for (src, sha256, m) in matches {
        if !m.confidence.is_auto_acceptable() {
            println!("  skipping {} — confidence is not exact", m.rom.name);
            continue;
        }
        let Some(slug) = es_de_slug(&m.platform) else {
            continue;
        };
        let dest = ready_root.join(slug).join(&m.rom.name);

        print!("  -> {}/{} ... ", slug, m.rom.name);
        std::io::stdout().flush().ok();

        match dumo_core::fsops::move_verified(src, &dest, Some(sha256)) {
            Ok(outcome) => {
                let how = match outcome.method {
                    dumo_core::fsops::MoveMethod::Rename => "moved",
                    dumo_core::fsops::MoveMethod::CopyVerified => "copied and verified",
                };
                println!("{how}");
                moved.push(dumo_core::ReadyFile {
                    path: relative_to_staging(&dest, &cfg.staging.root),
                    bytes: outcome.bytes,
                    sha256: sha256.clone(),
                });
                reference = Some(m.clone());
            }
            Err(e) => {
                println!("FAILED");
                println!("     {e}");
                println!("     Nothing was moved or deleted for this file.");
            }
        }
    }

    if moved.is_empty() {
        return Ok(());
    }

    // Drop artifacts we relocated from the job's artifact list; they now live in ready/
    // and are tracked there, so the manifest keeps describing reality.
    let moved_names: Vec<String> = matches
        .iter()
        .map(|(p, _, _)| {
            p.strip_prefix(job_dir)
                .unwrap_or(p)
                .to_string_lossy()
                .to_string()
        })
        .collect();
    job.artifacts
        .retain(|a| !moved_names.contains(&a.relative_path));

    if let Some(m) = reference {
        job.identification = Some(dumo_core::Identification {
            title: m.game.name.clone(),
            platform: m.platform.clone(),
            platform_slug: es_de_slug(&m.platform).unwrap_or("unknown").to_string(),
            matched_on: m.matched_on.to_string(),
            confidence: m.confidence,
            source: file_name(&m.source),
            files: moved,
            identified_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
        job.stage = dumo_core::JobStage::Identified;
        job.save(job_dir)?;
        println!("  job stage: identified");
        println!("  provenance (logs, sector state) stays in the job directory");
    }

    Ok(())
}

/// Express a path relative to the staging root, for storage in the manifest.
fn relative_to_staging(path: &std::path::Path, staging_root: &std::path::Path) -> String {
    path.strip_prefix(staging_root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

/// Analyse a video job's structure: what is content, what is an extra, and whether the
/// disc looks like a film or a set of episodes.
///
/// This is offline inference from duration and chapter layout. It deliberately stops
/// short of naming anything — that needs an online lookup, and it is a fuzzy match that
/// must always be confirmed.
fn analyse_video(
    job: &Job,
    job_dir: &std::path::Path,
    cfg: &Config,
    show_override: Option<&str>,
) -> Result<()> {
    use dumo_identify::video::{self, AnalysisParams, TitleInput};

    let media: Vec<_> = job
        .artifacts
        .iter()
        .filter(|a| {
            let p = a.relative_path.to_ascii_lowercase();
            p.ends_with(".mkv") || p.ends_with(".mp4") || p.ends_with(".m2ts")
        })
        .collect();

    if media.is_empty() {
        println!("  no video files among the artifacts");
        return Ok(());
    }

    let mut inputs = Vec::new();
    for a in &media {
        let path = job_dir.join(&a.relative_path);
        if !path.is_file() {
            println!("  {} — MISSING", a.relative_path);
            continue;
        }
        let f = dumo_backends::ffprobe::probe(&path)
            .with_context(|| format!("probing {}", path.display()))?;
        inputs.push(TitleInput {
            name: a.relative_path.clone(),
            duration_secs: f.duration_secs,
            chapters: f.chapter_count(),
        });
    }

    // Present titles in disc order, which is the ordering an episode set follows.
    inputs.sort_by(|a, b| a.name.cmp(&b.name));

    let analysis = video::analyse(&inputs, AnalysisParams::default());

    println!("  Disc shape: {}", analysis.shape);
    if let Some(c) = analysis.category {
        println!("  Category:   {} (proposed)", c.slug());
    } else {
        println!("  Category:   undetermined");
    }
    println!(
        "  Confidence: {}  (structural inference — always requires confirmation)",
        analysis.confidence
    );

    println!("  Titles:");
    for t in &analysis.titles {
        println!(
            "    {:<16} {:>9}  {:<8} {}",
            t.name.trim_start_matches("raw/"),
            t.duration_hms(),
            t.role.to_string(),
            t.why
        );
    }

    println!("  Reasoning:");
    for e in &analysis.evidence {
        println!("    - {e}");
    }

    // --- Online lookup ---
    let label = job
        .probe
        .as_ref()
        .and_then(|p| p.content.title_guess.clone())
        .unwrap_or_default();
    let query = show_override
        .map(str::to_string)
        .unwrap_or_else(|| dumo_identify::matching::query_from_label(&label));

    if query.trim().is_empty() {
        println!("  No search term: pass --show to name this disc's content.");
        return Ok(());
    }

    let client = match dumo_identify::tmdb::TmdbClient::from_config(&cfg.api) {
        Ok(c) => c,
        Err(e) => {
            println!("  Naming unavailable: {e}");
            return Ok(());
        }
    };

    println!();
    println!("  Searching TMDB for {query:?} ...");
    let shows = client.search_tv(&query).context("searching TMDB")?;
    if shows.is_empty() {
        println!("  No series matched. Try --show \"<title>\".");
        return Ok(());
    }
    // The disc number in the volume label is an independent ordering signal, and
    // runtimes tie often enough that it earns its keep.
    let disc_hint = dumo_identify::matching::disc_number_from_label(&label);
    if let Some(d) = disc_hint {
        println!("  Disc {d} of a set, per the volume label");
    }

    let mut titles: Vec<dumo_identify::matching::DiscTitle> = analysis
        .main_titles()
        .map(|t| dumo_identify::matching::DiscTitle {
            name: t.name.trim_start_matches("raw/").to_string(),
            duration_secs: t.duration_secs,
        })
        .collect();
    if titles.is_empty() {
        println!("  No main titles to match.");
        return Ok(());
    }
    dumo_identify::matching::sort_by_title_index(&mut titles);

    // Judge candidates by how well their episodes actually fit these runtimes, not by
    // TMDB's search ranking. Searching "espn 30 for 30" puts a different, similarly
    // named series first; only the runtimes reveal which one is really on the disc.
    const MAX_CANDIDATES: usize = 4;
    let considered: Vec<_> = shows.iter().take(MAX_CANDIDATES).collect();
    println!(
        "  Considering {} candidate series by how well episode runtimes fit:",
        considered.len()
    );

    let mut evaluated: Vec<(&dumo_identify::tmdb::TvResult, Vec<dumo_identify::matching::SeasonMatch>)> =
        Vec::new();
    for c in &considered {
        let season_numbers = match client.tv_season_numbers(c.id) {
            Ok(n) => n,
            Err(e) => {
                println!("    {} — unavailable: {e}", c.name);
                continue;
            }
        };
        let mut seasons = Vec::new();
        for n in season_numbers.iter().filter(|n| **n > 0) {
            if let Ok(s) = client.season(c.id, *n) {
                seasons.push(s);
            }
        }
        let results = dumo_identify::matching::match_seasons(&titles, &seasons, disc_hint);
        match results.first() {
            Some(b) => {
                println!(
                    "    {:<34} {:>5.1} min/episode average difference",
                    truncate(&format!("{} ({})", c.name,
                        c.year().map(|y| y.to_string()).unwrap_or("?".into())), 34),
                    b.mean_delta
                );
                evaluated.push((c, results));
            }
            None => println!(
                "    {:<34} no season with {} or more episodes",
                truncate(&c.name, 34),
                titles.len()
            ),
        }
    }

    evaluated.sort_by(|a, b| {
        a.1[0]
            .mean_delta
            .partial_cmp(&b.1[0].mean_delta)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let Some((candidate, results)) = evaluated.first() else {
        println!("  No candidate series could be matched.");
        return Ok(());
    };
    let best = &results[0];

    println!();
    println!(
        "  Series: {} ({})  [tmdb:{}]",
        candidate.name,
        candidate.year().map(|y| y.to_string()).unwrap_or("?".into()),
        candidate.id
    );
    if let Some((runner, rres)) = evaluated.get(1) {
        let gap = rres[0].mean_delta - best.mean_delta;
        println!(
            "    chosen over {} by {:.1} min/episode",
            truncate(&runner.name, 40),
            gap
        );
    }

    println!();
    println!(
        "  Best match: season {}, episodes {}–{}",
        best.season,
        best.first_episode,
        best.first_episode + best.matches.len() as u32 - 1
    );
    for m in &best.matches {
        println!(
            "    {:<16} {:>5.0} min  ->  S{:02}E{:02} {:<44} (delta {:.0} min)",
            m.title_name,
            m.title_mins,
            m.episode.season,
            m.episode.number,
            truncate(&m.episode.name, 44),
            m.delta_mins
        );
    }
    println!(
        "  Confidence: {}  ({})",
        best.confidence,
        if best.confidence.is_auto_acceptable() {
            "eligible for unattended acceptance"
        } else {
            "needs confirmation"
        }
    );
    for e in &best.evidence {
        println!("    - {e}");
    }
    if best.disc_hint_used {
        println!("    - the disc number decided this, not the runtimes");
    }


    println!();
    println!("  Proposed names:");
    let year = candidate
        .year()
        .map(|y| format!(" ({y})"))
        .unwrap_or_default();
    for m in &best.matches {
        println!(
            "    tv/{}{}/Season {:02}/{}{} S{:02}E{:02} - {}.mkv",
            candidate.name,
            year,
            m.episode.season,
            candidate.name,
            year,
            m.episode.season,
            m.episode.number,
            sanitise(&m.episode.name)
        );
    }
    println!();
    println!("  Applying video names is not wired up yet — review the above first.");
    Ok(())
}

/// Strip characters that are awkward or illegal in filenames.
fn sanitise(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' => '-',
            ':' => ' ',
            '?' | '*' | '"' | '<' | '>' | '|' => ' ',
            other => other,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

/// Identify several video discs together, using the constraint that a box set holds
/// consecutive, non-overlapping episodes in disc order.
///
/// This is materially stronger than matching discs one at a time. Runtimes routinely
/// tie between adjacent windows; requiring the whole set to be consistent usually
/// leaves exactly one arrangement standing.
fn solve_video_set(dirs: &[PathBuf], cfg: &Config, show_override: Option<&str>) -> Result<()> {
    use dumo_identify::discset::{self, SetDisc};
    use dumo_identify::matching::{self, DiscTitle};
    use dumo_identify::video::{self as vid, AnalysisParams, TitleInput};

    let mut discs: Vec<SetDisc> = Vec::new();
    let mut labels: Vec<String> = Vec::new();

    for dir in dirs {
        let job = match Job::load(dir) {
            Ok(j) => j,
            Err(_) => continue,
        };
        let kind = job.probe.as_ref().map(|p| p.content.kind);
        if !matches!(kind, Some(MediaKind::DvdVideo) | Some(MediaKind::BluRayVideo)) {
            continue;
        }
        let label = job
            .probe
            .as_ref()
            .and_then(|p| p.content.title_guess.clone())
            .unwrap_or_default();

        let mut inputs = Vec::new();
        for a in job.artifacts.iter().filter(|a| {
            let p = a.relative_path.to_ascii_lowercase();
            p.ends_with(".mkv") || p.ends_with(".mp4") || p.ends_with(".m2ts")
        }) {
            let path = dir.join(&a.relative_path);
            if !path.is_file() {
                continue;
            }
            let f = dumo_backends::ffprobe::probe(&path)
                .with_context(|| format!("probing {}", path.display()))?;
            inputs.push(TitleInput {
                name: a.relative_path.clone(),
                duration_secs: f.duration_secs,
                chapters: f.chapter_count(),
            });
        }
        if inputs.is_empty() {
            continue;
        }

        let analysis = vid::analyse(&inputs, AnalysisParams::default());
        let mut titles: Vec<DiscTitle> = analysis
            .main_titles()
            .map(|t| DiscTitle {
                name: t.name.trim_start_matches("raw/").to_string(),
                duration_secs: t.duration_secs,
            })
            .collect();
        if titles.is_empty() {
            continue;
        }
        matching::sort_by_title_index(&mut titles);

        let disc_number = matching::disc_number_from_label(&label).unwrap_or(discs.len() as u32 + 1);
        println!(
            "  disc {disc_number}: {} main title(s) from {}",
            titles.len(),
            job.id
        );
        labels.push(label);
        discs.push(SetDisc {
            disc_number,
            job_id: job.id.clone(),
            titles,
        });
    }

    if discs.is_empty() {
        println!("No video jobs with main titles found.");
        return Ok(());
    }

    let query = show_override.map(str::to_string).unwrap_or_else(|| {
        labels
            .first()
            .map(|l| matching::query_from_label(l))
            .unwrap_or_default()
    });
    if query.trim().is_empty() {
        println!("No search term; pass --show.");
        return Ok(());
    }

    let client = match dumo_identify::tmdb::TmdbClient::from_config(&cfg.api) {
        Ok(c) => c,
        Err(e) => {
            println!("Naming unavailable: {e}");
            return Ok(());
        }
    };

    println!();
    println!("Searching TMDB for {query:?} ...");
    let shows = client.search_tv(&query).context("searching TMDB")?;
    if shows.is_empty() {
        println!("No series matched. Try --show \"<title>\".");
        return Ok(());
    }

    // Rank candidate series by how well the whole set fits, not by search position.
    let mut best: Option<(&dumo_identify::tmdb::TvResult, discset::SetSolution)> = None;
    for c in shows.iter().take(4) {
        let Ok(nums) = client.tv_season_numbers(c.id) else {
            continue;
        };
        let mut seasons = Vec::new();
        for n in nums.iter().filter(|n| **n > 0) {
            if let Ok(s) = client.season(c.id, *n) {
                seasons.push(s);
            }
        }
        if let Some(sol) = discset::solve(&discs, &seasons) {
            println!(
                "  {:<34} {:>5.1} min/title average difference",
                truncate(&format!("{} ({})", c.name,
                    c.year().map(|y| y.to_string()).unwrap_or("?".into())), 34),
                sol.mean_delta
            );
            if best.as_ref().map(|(_, b)| sol.mean_delta < b.mean_delta).unwrap_or(true) {
                best = Some((c, sol));
            }
        }
    }

    let Some((series, solution)) = best else {
        println!("No candidate series could hold this set.");
        return Ok(());
    };

    let year = series.year().map(|y| format!(" ({y})")).unwrap_or_default();
    println!();
    println!("Series: {}{}  [tmdb:{}]", series.name, year, series.id);
    println!("Season: {}", solution.season);
    println!(
        "Confidence: {}  ({})",
        solution.confidence,
        if solution.confidence.is_auto_acceptable() {
            "eligible for unattended acceptance"
        } else {
            "needs confirmation"
        }
    );
    for e in &solution.evidence {
        println!("  - {e}");
    }

    println!();
    for p in &solution.placements {
        println!("Disc {} ({})", p.disc_number, p.job_id);
        for m in &p.matches {
            println!(
                "  {:<16} {:>5.0} min  ->  S{:02}E{:02} {:<40} (delta {:.0} min)",
                m.title_name,
                m.title_mins,
                m.episode.season,
                m.episode.number,
                truncate(&m.episode.name, 40),
                m.delta_mins
            );
        }
        for m in &p.matches {
            println!(
                "    tv/{}{}/Season {:02}/{}{} S{:02}E{:02} - {}.mkv",
                series.name, year, m.episode.season,
                series.name, year, m.episode.season, m.episode.number,
                sanitise(&m.episode.name)
            );
        }
        println!();
    }
    println!("Applying video names is not wired up yet — review the above first.");
    Ok(())
}
