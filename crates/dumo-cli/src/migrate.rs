//! The `migrate` command: Stage 4, staging to permanent storage.
//!
//! This is the stage that can lose data if it is wrong, so it is the most cautious:
//!
//! - Only jobs that reached `identified` are eligible; nothing unidentified is filed.
//! - The destination is checked for reachability and space before anything is written.
//! - Existing destination files are never overwritten.
//! - Every file is verified by re-reading the destination after writing.
//! - Whether the staging copy is then removed is governed by
//!   `staging.reclaim_after_migrate`, and removal only ever happens after verification.

use anyhow::{bail, Context, Result};
use dumo_core::config::{self, Config, DestinationConfig};
use dumo_core::fsops;
use dumo_core::job::{Job, JobStage};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct MigrateArgs {
    /// Job id or fragment. Omit to migrate every eligible job.
    pub job: Option<String>,
    /// Destination name, when more than one is configured.
    pub destination: Option<String>,
    pub dry_run: bool,
    pub assume_yes: bool,
    pub config_file: Option<PathBuf>,
}

pub fn run(args: MigrateArgs) -> Result<()> {
    let cfg = Config::load(args.config_file.as_deref())?;

    // --- Pick a destination ---
    if cfg.destinations.is_empty() {
        bail!(
            "no destination configured.\n\
             Add a [[destination]] block to {} with the mount point of your permanent\n\
             storage. For a network share, mount it with the OS first — dump-o-matic\n\
             takes a filesystem path, not an smb:// URL.",
            cfg.source_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "your config".into())
        );
    }

    let dest_cfg: &DestinationConfig = match &args.destination {
        Some(name) => cfg
            .find_destination(name)
            .with_context(|| format!("no destination named {name:?}"))?,
        None if cfg.destinations.len() == 1 => &cfg.destinations[0],
        None => bail!(
            "several destinations configured ({}); choose one with --destination",
            cfg.destinations
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    println!("Destination: {} -> {}", dest_cfg.name, dest_cfg.root.display());

    // --- Destination must actually be there ---
    if !dest_cfg.root.is_dir() {
        bail!(
            "{} is not present.{}\n\
             Nothing has been changed; staged content simply waits.",
            dest_cfg.root.display(),
            if dest_cfg.network {
                " The share may not be mounted."
            } else {
                ""
            }
        );
    }
    // A mounted-but-empty mountpoint is a common failure: the share dropped and writes
    // would silently land on the local root filesystem instead.
    if dest_cfg.network && !is_mountpoint(&dest_cfg.root) {
        println!(
            "  WARNING: {} is configured as a network destination but is not a mount\n\
             \x20         point. If the share is unmounted, writes would land on the local\n\
             \x20         disk under that path instead.",
            dest_cfg.root.display()
        );
        if !args.assume_yes && !confirm("Continue anyway?")? {
            println!("Aborted; nothing was written.");
            return Ok(());
        }
    }

    match config::filesystem_free_bytes(&dest_cfg.root) {
        Some((free, total)) => println!(
            "  {} free of {}",
            human_size(free),
            human_size(total)
        ),
        None => println!("  free space: unknown (the filesystem does not report it)"),
    }

    if dest_cfg.network {
        println!("  network destination: writes are verified by re-reading after eviction");
    }
    println!();

    // --- Gather eligible jobs ---
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

    let mut eligible: Vec<(PathBuf, Job)> = Vec::new();
    for d in dirs {
        let job = match Job::load(&d) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("warning: skipping {}: {e}", d.display());
                continue;
            }
        };
        match job.stage {
            JobStage::Identified => eligible.push((d, job)),
            JobStage::Migrated => println!("{} — already migrated, skipping", job.id),
            other => println!(
                "{} — {other}, not eligible (identify it first)",
                job.id
            ),
        }
    }

    if eligible.is_empty() {
        println!();
        println!("Nothing to migrate.");
        return Ok(());
    }

    // --- Plan, and check it fits ---
    println!();
    println!("Plan:");
    let mut total_bytes = 0u64;
    let mut planned: Vec<Plan> = Vec::new();

    for (dir, job) in &eligible {
        let Some(id) = &job.identification else {
            continue;
        };
        for f in &id.files {
            let src = cfg.staging.root.join(&f.path);
            let dest = destination_path(&dest_cfg.root, &f.path);
            let exists = dest.exists();
            println!(
                "  {} -> {}{}",
                f.path,
                dest.display(),
                if exists { "   [BLOCKED: exists]" } else { "" }
            );
            if !exists {
                total_bytes += f.bytes;
            }
            planned.push(Plan {
                job_dir: dir.clone(),
                src,
                dest,
                sha256: f.sha256.clone(),
                bytes: f.bytes,
                blocked: exists,
                staging_relative: f.path.clone(),
            });
        }
    }

    let blocked = planned.iter().filter(|p| p.blocked).count();
    if blocked > 0 {
        println!();
        println!(
            "{blocked} file(s) already exist at the destination and will be skipped.\n\
             They are never overwritten. If the destination copy is stale, remove it\n\
             yourself and re-run."
        );
    }

    let to_move: Vec<&Plan> = planned.iter().filter(|p| !p.blocked).collect();
    if to_move.is_empty() {
        println!();
        println!("Nothing left to do.");
        return Ok(());
    }

    println!();
    println!("Total to transfer: {}", human_size(total_bytes));

    if let Some((free, _)) = config::filesystem_free_bytes(&dest_cfg.root) {
        if free < total_bytes {
            bail!(
                "destination has {} free but {} is needed",
                human_size(free),
                human_size(total_bytes)
            );
        }
    } else if !args.assume_yes {
        bail!("destination free space is unknown; re-run with --yes to proceed anyway");
    }

    let reclaim = cfg.staging.reclaim_after_migrate;
    println!(
        "Staging copies will be {} after verification.",
        if reclaim {
            "REMOVED (staging.reclaim_after_migrate = true)"
        } else {
            "KEPT (staging.reclaim_after_migrate = false)"
        }
    );

    if args.dry_run {
        println!();
        println!("Dry run: nothing was written.");
        return Ok(());
    }
    if !args.assume_yes && !confirm("Proceed with migration?")? {
        println!("Aborted; nothing was written.");
        return Ok(());
    }

    // --- Transfer ---
    println!();
    let mut migrated_jobs: Vec<PathBuf> = Vec::new();
    let mut failures = 0;

    for p in &to_move {
        print!("  {} ... ", p.staging_relative);
        std::io::stdout().flush().ok();
        let started = std::time::Instant::now();

        let result = if reclaim {
            fsops::move_verified(&p.src, &p.dest, Some(&p.sha256)).map(|o| o.verified_sha256)
        } else {
            // Keep the staging copy: copy and verify, and do not touch the source.
            fsops::copy_verified(&p.src, &p.dest, &p.sha256).map(Some)
        };

        match result {
            Ok(verified) => {
                let secs = started.elapsed().as_secs_f64().max(0.001);
                let rate = p.bytes as f64 / secs / 1_000_000.0;
                println!(
                    "ok ({:.0} MB/s){}",
                    rate,
                    match verified {
                        Some(_) => ", verified by re-read",
                        // A same-filesystem rename needs no re-read; see fsops.
                        None => ", atomic rename",
                    }
                );
                if !migrated_jobs.contains(&p.job_dir) {
                    migrated_jobs.push(p.job_dir.clone());
                }
            }
            Err(e) => {
                failures += 1;
                println!("FAILED");
                println!("     {e}");
                println!("     Source is untouched and no partial file was left behind.");
            }
        }
    }

    // --- Record outcomes ---
    for dir in &migrated_jobs {
        let mut job = Job::load(dir)?;
        // Only claim migration when every file of this job landed.
        let all_done = job
            .identification
            .as_ref()
            .map(|id| {
                id.files
                    .iter()
                    .all(|f| destination_path(&dest_cfg.root, &f.path).exists())
            })
            .unwrap_or(false);
        if all_done {
            job.stage = JobStage::Migrated;
            job.save(dir)?;
            println!();
            println!("{} — migrated", job.id);
        }
    }

    println!();
    if failures > 0 {
        println!("{failures} file(s) failed; nothing was deleted for those.");
        std::process::exit(1);
    }
    println!("Migration complete.");
    if !reclaim {
        println!(
            "Staging copies remain in {} — they are safe to delete once you are happy.",
            cfg.staging.ready_dir().display()
        );
    }
    Ok(())
}

struct Plan {
    job_dir: PathBuf,
    src: PathBuf,
    dest: PathBuf,
    sha256: String,
    bytes: u64,
    blocked: bool,
    staging_relative: String,
}

/// Map a staging-relative path onto the destination.
///
/// Staged paths look like `ready/ps2/Title.iso`; the `ready/` prefix is a staging
/// concept and is dropped so permanent storage sees `ps2/Title.iso`.
fn destination_path(dest_root: &Path, staging_relative: &str) -> PathBuf {
    let rel = staging_relative
        .strip_prefix("ready/")
        .unwrap_or(staging_relative);
    dest_root.join(rel)
}

/// Whether `path` is a mount point, by comparing device ids with its parent.
fn is_mountpoint(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(here) = std::fs::metadata(path) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return true; // "/" is a mount point
    };
    match std::fs::metadata(parent) {
        Ok(up) => here.dev() != up.dev(),
        Err(_) => false,
    }
}

fn human_size(bytes: u64) -> String {
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else {
        format!("{:.0} MB", b / MB)
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_path_drops_the_ready_prefix() {
        let d = destination_path(Path::new("/mnt/nas"), "ready/ps2/Title (USA).iso");
        assert_eq!(d, PathBuf::from("/mnt/nas/ps2/Title (USA).iso"));
    }

    #[test]
    fn destination_path_passes_through_other_paths() {
        let d = destination_path(Path::new("/mnt/nas"), "ps2/Title.iso");
        assert_eq!(d, PathBuf::from("/mnt/nas/ps2/Title.iso"));
    }

    #[test]
    fn root_is_a_mountpoint() {
        assert!(is_mountpoint(Path::new("/")));
    }

    #[test]
    fn an_ordinary_directory_is_not_a_mountpoint() {
        assert!(!is_mountpoint(Path::new("/etc")));
    }
}
