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

    for dir in dirs {
        identify_job(&dir, &set)?;
        println!();
    }
    Ok(())
}

fn identify_job(job_dir: &std::path::Path, set: &DatfileSet) -> Result<()> {
    let job = Job::load(job_dir)?;
    println!("Job {} ({})", job.id, job.stage);

    let kind = job
        .probe
        .as_ref()
        .map(|p| p.content.kind)
        .unwrap_or(MediaKind::Unknown);

    // Datfile matching only applies to disc images. Video rips need an entirely
    // different (and fuzzy) identification path, which is not built yet.
    if !matches!(kind, MediaKind::GameDisc | MediaKind::Data) {
        println!("  {kind} — datfile matching does not apply.");
        println!(
            "  Video identification (TMDB/TVDB) is not implemented yet, so this job\n  \
             cannot be identified automatically."
        );
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
    Ok(())
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}
