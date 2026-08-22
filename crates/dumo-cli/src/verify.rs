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
        if !verify_one(&dir, args.update)? {
            any_failed = true;
        }
        println!();
    }

    if any_failed {
        std::process::exit(1);
    }
    Ok(())
}

fn verify_one(job_dir: &std::path::Path, update: bool) -> Result<bool> {
    let mut job = Job::load(job_dir)?;
    println!("Job {} ({})", job.id, job.stage);

    if job.artifacts.is_empty() {
        println!("  no artifacts recorded — nothing to verify");
        return Ok(false);
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
        match dumo_backends::redumper::analyze_state_file(&path) {
            Ok(s) => {
                if s.is_complete() {
                    println!(
                        "  sector state: all {} sectors read successfully",
                        s.total_sectors
                    );
                } else {
                    println!(
                        "  sector state: {} unreadable of {} in {} run(s)",
                        s.bad_sectors(),
                        s.total_sectors,
                        s.bad_runs.len()
                    );
                    for (a, b) in s.bad_runs.iter().take(10) {
                        println!("    LBA {a}..{b} ({} sectors)", b - a + 1);
                    }
                    problems.push(format!("{} unreadable sector(s)", s.bad_sectors()));
                }
            }
            Err(e) => problems.push(format!("could not read state file: {e}")),
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
        // Never promote a job that failed verification; downgrading is safe though.
        if update && job.stage != JobStage::Failed {
            job.stage = JobStage::Failed;
            job.error = Some(problems.join("; "));
            job.save(job_dir)?;
            println!("  stage updated: -> failed");
        }
        Ok(false)
    }
}
