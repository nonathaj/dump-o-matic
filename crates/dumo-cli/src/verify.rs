//! The `verify` command: re-check a staged job's integrity from its artifacts.
//!
//! Read-only with respect to media: it re-hashes what is on disk and compares against
//! the manifest. Two uses:
//!
//! - Confirm staged content is still intact (bit rot, a bad cable, an interrupted copy).
//! - Re-judge a job whose verdict was recorded by older, wronger logic, without
//!   re-reading the disc.
//!
//! It will promote a job from `failed` to `ripped` only when every check passes, and it
//! never deletes anything.

use anyhow::{bail, Context, Result};
use dumo_backends::redumper;
use crate::migrate::{category_of, destination_path};
use dumo_core::config::Config;
use dumo_core::job::{Job, JobStage};
use dumo_core::hash;
use std::path::PathBuf;

pub struct VerifyArgs {
    /// Job id, or a prefix of one. Omit to verify every job.
    pub job: Option<String>,
    pub config_file: Option<PathBuf>,
    /// Update the recorded stage when the verdict changes.
    pub update: bool,
}

pub fn run(args: VerifyArgs) -> Result<()> {
    let cfg = Config::load(args.config_file.as_deref())?;
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
            bail!("no job matching {filter:?} in {}", jobs_dir.display());
        }
    }

    let mut any_failed = false;
    for dir in dirs {
        if !verify_one(&dir, &cfg, args.update)? {
            any_failed = true;
        }
        println!();
    }

    if any_failed {
        std::process::exit(1);
    }
    Ok(())
}

/// Where a staging-relative path would live at its configured destination.
fn cfg_destination(cfg: &Config, category: &str, relative: &str) -> Option<PathBuf> {
    cfg.destinations
        .iter()
        .find(|d| d.accepts(category))
        .map(|d| destination_path(&d.root, relative))
}

fn verify_one(
    job_dir: &std::path::Path,
    cfg: &Config,
    update: bool,
) -> Result<bool> {
    let staging_root = &cfg.staging.root;
    let mut job = Job::load(job_dir)?;
    println!("Job {} ({})", job.id, job.stage);

    // A job with no artifacts is not necessarily a job with nothing to check. An adopted
    // job never had artifacts — it was never ripped — but it does name files in the
    // library, and those are exactly what wants verifying. Bail only when there is
    // genuinely nothing on either side.
    let ready_count = job
        .identification
        .as_ref()
        .map(|id| id.files.len())
        .unwrap_or(0);
    if job.artifacts.is_empty() && ready_count == 0 {
        println!("  nothing recorded to verify");
        // Not a failure: an interrupted or empty job is a state, not a corruption.
        return Ok(true);
    }

    let mut problems: Vec<String> = Vec::new();

    // --- Re-hash every artifact against the manifest ---
    for a in &job.artifacts {
        let path = job_dir.join(&a.relative_path);
        print!("  {:<28} ", a.relative_path);

        if !path.is_file() {
            println!("MISSING");
            problems.push(format!("{} is missing", a.relative_path));
            continue;
        }

        let (sha256, bytes) = hash::sha256_file(&path)
            .with_context(|| format!("hashing {}", path.display()))?;

        if bytes != a.bytes {
            println!("SIZE MISMATCH ({bytes} vs {} recorded)", a.bytes);
            problems.push(format!("{} changed size", a.relative_path));
        } else if sha256 != a.sha256 {
            println!("HASH MISMATCH");
            problems.push(format!("{} failed hash verification", a.relative_path));
        } else {
            println!("ok  {}", &sha256[..16]);
        }
    }

    // --- Re-run backend-specific integrity checks ---
    if let Some(state_file) = job
        .artifacts
        .iter()
        .find(|a| a.relative_path.ends_with(".state"))
    {
        let path = job_dir.join(&state_file.relative_path);
        // Same reasoning as the rip-time gate: on a CD each byte is a 4-byte sample and
        // the file spans the unreachable lead-in/lead-out, so the unit has to be
        // measured against the .scram sidecar rather than assumed.
        let scram = job
            .artifacts
            .iter()
            .find(|a| a.relative_path.ends_with(".scram"))
            .map(|a| job_dir.join(&a.relative_path));
        let unit = dumo_backends::redumper::detect_state_unit(&path, scram.as_deref());
        match dumo_backends::redumper::analyze_state_file_as(&path, unit) {
            Ok(s) => {
                let noun = s.unit_noun();
                if s.is_complete() {
                    if s.edge_bad() > 0 {
                        println!(
                            "  {noun} state: all {} track {noun}s read successfully \
                             ({} unread in the lead-in/lead-out, outside the tracks)",
                            s.total_sectors - s.edge_bad(),
                            s.edge_bad()
                        );
                    } else {
                        println!(
                            "  {noun} state: all {} {noun}s read successfully",
                            s.total_sectors
                        );
                    }
                } else {
                    let runs = s.interior_runs();
                    println!(
                        "  {noun} state: {} unreadable of {} in {} run(s) inside the data",
                        s.interior_bad(),
                        s.total_sectors,
                        runs.len()
                    );
                    for (a, b) in runs.iter().take(10) {
                        println!("    {a}..{b} ({} {noun}s)", b - a + 1);
                    }
                    problems.push(format!("{} unreadable {noun}(s)", s.interior_bad()));
                }
            }
            Err(e) => problems.push(format!("could not read state file: {e}")),
        }
    }

    // --- Re-run the rip-time geometry gate ---
    // The expected length comes from the TOC/capacity recorded at probe time, which is
    // independent of the image, so this still catches a truncated artifact long after
    // the disc is gone.
    //
    // Only while the image is still in the job, though: once identify has filed the
    // tracks into ready/ they are no longer here to measure, and the leftover cue and
    // sidecars are provenance, not a truncated dump.
    let files: Vec<PathBuf> = job
        .artifacts
        .iter()
        .map(|a| job_dir.join(&a.relative_path))
        .collect();
    let has = |ext: &str| {
        files
            .iter()
            .any(|p| p.extension().map(|e| e == ext).unwrap_or(false))
    };
    // A device job has no drive and no capacity to compare against, so the disc gates
    // below do not apply to it. Its independent measure is the package's own geometry.
    if let Some(src) = &job.device_source {
        if src.image_blocks > 0 {
            let expected = dumo_devices::iso::RESERVED + src.image_blocks * dumo_devices::god::BLOCK;
            match files
                .iter()
                .find(|p| p.extension().map(|e| e == "iso").unwrap_or(false))
            {
                Some(iso) => {
                    let actual = std::fs::metadata(iso).map(|m| m.len()).unwrap_or(0);
                    if actual == expected {
                        println!(
                            "  image length: matches the package's {} block(s)",
                            src.image_blocks
                        );
                    } else {
                        problems.push(format!(
                            "{} is {actual} bytes but the package's {} blocks give {expected}",
                            iso.display(),
                            src.image_blocks
                        ));
                    }
                }
                // Filed into ready/ already, so there is nothing here to measure.
                None => {}
            }
        }
        println!(
            "  provenance: {} block(s) were verified against the package's hash tree when pulled",
            src.blocks_verified
        );
    } else if has("cue") && has("bin") {
        let leadout = job
            .probe
            .as_ref()
            .and_then(|p| p.toc.as_ref())
            .map(|t| u64::from(t.leadout_lba))
            .unwrap_or(0);
        let geom = redumper::ExpectedGeometry::CdTracks {
            leadout_lba: leadout,
        };
        match redumper::verify_files_size(&files, None, geom) {
            Ok(()) => println!("  track lengths: match the TOC lead-out at LBA {leadout}"),
            Err(e) => problems.push(e),
        }
    } else if has("iso") {
        let sectors = job
            .probe
            .as_ref()
            .and_then(|p| p.capacity_bytes)
            .map(|b| b / 2048);
        match redumper::verify_files_size(&files, sectors, redumper::ExpectedGeometry::Iso) {
            Ok(()) => println!("  image length: matches the drive-reported capacity"),
            Err(e) => problems.push(e),
        }
    }

    // --- Re-hash whatever identify filed into ready/ ---
    // These are the files that will actually be migrated, so they matter more than the
    // provenance left behind in the job directory. Without this, verifying an identified
    // job would silently check only the leftovers.
    if let Some(id) = &job.identification {
        let migrated = job.stage == JobStage::Migrated;
        for f in &id.files {
            let staged = staging_root.join(&f.path);
            print!("  {} ... ", f.path);
            std::io::Write::flush(&mut std::io::stdout()).ok();

            // Once a job has migrated, its staging copy is expendable and may have been
            // reclaimed on purpose. The content still exists — at the destination — so
            // check there rather than calling a deliberately released copy "missing".
            let path = if staged.is_file() {
                staged
            } else if migrated {
                let category = category_of(&f.path);
                match cfg_destination(cfg, &category, &f.path).filter(|p| p.is_file()) {
                    Some(p) => {
                        print!("(from permanent storage) ");
                        p
                    }
                    None => {
                        println!("MISSING");
                        problems.push(format!(
                            "{} is in neither staging nor permanent storage",
                            f.path
                        ));
                        continue;
                    }
                }
            } else {
                println!("MISSING");
                problems.push(format!("{} is missing from ready/", f.path));
                continue;
            };
            match hash::sha256_file(&path) {
                Ok((sha256, _)) if sha256 == f.sha256 => println!("ok  {}", &sha256[..16]),
                Ok(_) => {
                    println!("HASH MISMATCH");
                    problems.push(format!("{} failed hash verification", f.path));
                }
                Err(e) => {
                    println!("ERROR");
                    problems.push(format!("could not hash {}: {e}", f.path));
                }
            }
        }
    }

    if problems.is_empty() {
        println!("  VERIFIED — all artifacts match the manifest");
        if update && job.stage == JobStage::Failed {
            job.stage = JobStage::Ripped;
            job.error = None;
            job.save(job_dir)?;
            println!("  stage updated: failed -> ripped");
        }
        Ok(true)
    } else {
        println!("  PROBLEMS:");
        for p in &problems {
            println!("    - {p}");
        }
        // Never promote a job that failed verification; downgrading is always safe.
        if update && job.stage != JobStage::Failed {
            job.stage = JobStage::Failed;
            job.error = Some(problems.join("; "));
            job.save(job_dir)?;
            println!("  stage updated: -> failed");
        }
        Ok(false)
    }
}
