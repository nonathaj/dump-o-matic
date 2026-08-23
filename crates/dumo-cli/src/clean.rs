//! Reclaim staging space for content that has safely reached permanent storage.
//!
//! `staging.reclaim_after_migrate` decides whether a staged copy is released at the
//! moment it migrates. Leaving it off — keep until the space is needed — is the safer
//! default, but it only makes sense if the space can actually be reclaimed later. This is
//! that step.
//!
//! The rule is the same one the rest of the pipeline follows: **a copy is only released
//! once another copy has been proven to exist**. Being marked `migrated` is not
//! sufficient evidence on its own — the manifest records what happened at migrate time,
//! not what is true now. So every destination file is located and re-hashed here, and the
//! staging copy is removed only if that hash matches. A destination file that has since
//! been deleted, truncated or corrupted means the staging copy is the last good copy, and
//! it stays.
//!
//! Dump provenance — redumper's `.state`, `.scram`, `.subcode` and logs — is never
//! removed. It is the evidence a dump was clean, it exists nowhere else, and unlike the
//! image it cannot be regenerated from the copy at the destination. Its size is reported
//! so the space is visible rather than mysterious.

use anyhow::{bail, Result};
use dumo_core::config::Config;
use dumo_core::hash;
use dumo_core::job::{Job, JobStage};
use std::io::Write;
use std::path::PathBuf;

pub struct CleanArgs {
    /// Job id, or a fragment of one. Omit to consider every migrated job.
    pub job: Option<String>,
    pub dry_run: bool,
    pub assume_yes: bool,
    pub config_file: Option<PathBuf>,
}

pub fn run(args: CleanArgs) -> Result<()> {
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

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut provenance_bytes = 0u64;
    let mut not_migrated = 0usize;

    for dir in &dirs {
        let Ok(job) = Job::load(dir) else { continue };
        if job.stage != JobStage::Migrated {
            not_migrated += 1;
            continue;
        }
        let Some(id) = &job.identification else {
            continue;
        };
        // Provenance stays; count it so the remaining space is explained.
        provenance_bytes += job
            .artifacts
            .iter()
            .map(|a| dir.join(&a.relative_path))
            .filter(|p| p.is_file())
            .filter_map(|p| std::fs::metadata(&p).ok().map(|m| m.len()))
            .sum::<u64>();

        for f in &id.files {
            let staged = cfg.staging.root.join(&f.path);
            if !staged.is_file() {
                continue; // already reclaimed
            }
            let category = crate::migrate::category_of(&f.path);
            let Some(dest) = cfg
                .destinations
                .iter()
                .find(|d| d.accepts(&category))
                .map(|d| crate::migrate::destination_path(&d.root, &f.path))
            else {
                continue;
            };
            candidates.push(Candidate {
                relative: f.path.clone(),
                staged,
                dest,
                sha256: f.sha256.clone(),
                bytes: f.bytes,
            });
        }
    }

    if candidates.is_empty() {
        println!("Nothing to reclaim.");
        if not_migrated > 0 {
            println!(
                "  {not_migrated} job(s) have not migrated yet; their staged content is the \\
                 only copy and is left alone."
            );
        }
        if provenance_bytes > 0 {
            println!(
                "  {} of dump provenance is retained (sector state, logs); it exists nowhere else.",
                crate::migrate::human_size(provenance_bytes)
            );
        }
        return Ok(());
    }

    let total: u64 = candidates.iter().map(|c| c.bytes).sum();
    println!("Staged copies whose content has reached permanent storage:");
    for c in &candidates {
        println!("  {} ({})", c.relative, crate::migrate::human_size(c.bytes));
        println!("      keeping: {}", c.dest.display());
    }
    println!();
    println!("Would reclaim {}.", crate::migrate::human_size(total));
    println!("Each destination copy is re-hashed first; any that does not match means the");
    println!("staged file is the last good copy and it will be kept.");
    if provenance_bytes > 0 {
        println!(
            "Dump provenance ({}) is never removed — it exists nowhere else.",
            crate::migrate::human_size(provenance_bytes)
        );
    }

    if args.dry_run {
        println!();
        println!("Dry run: nothing was deleted.");
        return Ok(());
    }
    if !args.assume_yes && !crate::migrate::confirm("Reclaim these staged copies?")? {
        println!("Aborted; nothing was deleted.");
        return Ok(());
    }

    println!();
    let mut freed = 0u64;
    let mut kept = 0usize;
    for c in &candidates {
        print!("  {} ... ", c.relative);
        std::io::stdout().flush().ok();

        if !c.dest.is_file() {
            println!("KEPT — no copy at {}", c.dest.display());
            kept += 1;
            continue;
        }
        // Re-hash the destination now. "It migrated" is a record of the past; this is the
        // only thing that makes deleting the staged copy safe in the present.
        match hash::sha256_file(&c.dest) {
            Ok((sha256, _)) if sha256 == c.sha256 => match std::fs::remove_file(&c.staged) {
                Ok(()) => {
                    freed += c.bytes;
                    println!("reclaimed (destination verified)");
                }
                Err(e) => {
                    println!("KEPT — could not remove: {e}");
                    kept += 1;
                }
            },
            Ok(_) => {
                println!("KEPT — destination copy does not match the manifest");
                kept += 1;
            }
            Err(e) => {
                println!("KEPT — could not read the destination: {e}");
                kept += 1;
            }
        }
    }

    println!();
    println!("Reclaimed {}.", crate::migrate::human_size(freed));
    if kept > 0 {
        println!("{kept} file(s) kept because their destination copy could not be verified.");
    }
    Ok(())
}

struct Candidate {
    relative: String,
    staged: PathBuf,
    dest: PathBuf,
    sha256: String,
    bytes: u64,
}
