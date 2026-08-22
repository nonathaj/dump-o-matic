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

    for dir in dirs {
        identify_job(&dir, &set, &cfg, args.apply)?;
        println!();
    }
    Ok(())
}

fn identify_job(
    job_dir: &std::path::Path,
    set: &DatfileSet,
    cfg: &Config,
    apply: bool,
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

    let ready_root = cfg.staging.ready_dir();
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
