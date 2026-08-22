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

    // With several shares, each file is routed by its media category rather than the
    // whole run going to one place. --destination restricts the run to a single target.
    let candidates: Vec<&DestinationConfig> = match &args.destination {
        Some(name) => vec![cfg
            .find_destination(name)
            .with_context(|| format!("no destination named {name:?}"))?],
        None => cfg.destinations.iter().collect(),
    };

    println!("Destinations:");
    for d in &candidates {
        println!(
            "  {:<16} {}{}{}",
            d.name,
            d.root.display(),
            if d.media.is_empty() {
                "  (any media)".to_string()
            } else {
                format!("  ({})", d.media.join(", "))
            },
            if d.network { "  [network]" } else { "" }
        );
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
            let category = category_of(&f.path);

            let Some(target) = candidates.iter().find(|d| d.accepts(&category)) else {
                println!(
                    "  {} -> no destination accepts category {:?}; skipping",
                    f.path, category
                );
                continue;
            };

            let dest = destination_path(&target.root, &f.path);
            let exists = dest.exists();
            println!(
                "  {} -> [{}] {}{}",
                f.path,
                target.name,
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
                dest_root: target.root.clone(),
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

    // Validate only the destinations this run will actually write to. Checking every
    // configured destination up front would let an unrelated offline or read-only share
    // block a migration that never touches it.
    let mut used_roots: Vec<PathBuf> = Vec::new();
    for p in planned.iter().filter(|p| !p.blocked) {
        if !used_roots.contains(&p.dest_root) {
            used_roots.push(p.dest_root.clone());
        }
    }
    for root in &used_roots {
        let d = candidates
            .iter()
            .find(|c| &c.root == root)
            .expect("planned roots come from candidates");

        if !d.root.is_dir() {
            bail!(
                "{} ({}) is not present.{}\n\
                 Nothing has been changed; staged content simply waits.",
                d.name,
                d.root.display(),
                if d.network { " The share may not be mounted." } else { "" }
            );
        }
        // Touch the path first: with systemd automount the mount is lazy, and a bare
        // stat of an untriggered mountpoint would look like an unmounted share.
        let _ = std::fs::read_dir(&d.root);

        if d.network && !is_network_filesystem(&d.root) {
            println!(
                "  WARNING: {} is declared as a network destination, but {} is not on a",
                d.name,
                d.root.display()
            );
            println!("           network filesystem — the share is probably not mounted.");
            println!("           Writing would land on the local disk and fill it silently.");
            if !args.assume_yes && !confirm("Continue anyway?")? {
                println!("Aborted; nothing was written.");
                return Ok(());
            }
        }
        if !is_writable_dir(&d.root) {
            bail!(
                "{} ({}) is not writable.\n\
                 Some NAS shares mark the share root read-only while subdirectories are\n\
                 writable; point the destination at a writable subdirectory, or fix the\n\
                 permission on the server.",
                d.name,
                d.root.display()
            );
        }
        if let Some((free, total)) = config::filesystem_free_bytes(&d.root) {
            println!("  {} — {} free of {}", d.name, human_size(free), human_size(total));
        }
    }

    // Space is checked per destination root, since files may be routed to different
    // shares with independent free space.
    let mut needed: std::collections::BTreeMap<PathBuf, u64> = Default::default();
    for p in planned.iter().filter(|p| !p.blocked) {
        *needed.entry(p.dest_root.clone()).or_insert(0) += p.bytes;
    }
    for (root, bytes) in &needed {
        match config::filesystem_free_bytes(root) {
            Some((free, _)) if free < *bytes => bail!(
                "{} has {} free but {} is needed",
                root.display(),
                human_size(free),
                human_size(*bytes)
            ),
            Some(_) => {}
            None if !args.assume_yes => bail!(
                "free space on {} is unknown; re-run with --yes to proceed anyway",
                root.display()
            ),
            None => {}
        }
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
                id.files.iter().all(|f| {
                    let category = category_of(&f.path);
                    candidates
                        .iter()
                        .find(|d| d.accepts(&category))
                        .map(|d| destination_path(&d.root, &f.path).exists())
                        .unwrap_or(false)
                })
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
    dest_root: PathBuf,
}

/// Map a staging-relative path onto the destination.
///
/// Staged paths look like `ready/ps2/Title.iso`; the `ready/` prefix is a staging
/// concept and is dropped so permanent storage sees `ps2/Title.iso`.
fn destination_path(dest_root: &Path, staging_relative: &str) -> PathBuf {
    let rel = staging_relative
        .strip_prefix("ready/")
        .unwrap_or(staging_relative);
    // Drop the category component too: it selected the destination, and the destination
    // root already represents it (e.g. .../emulation/roms for games).
    let below = rel.split_once('/').map(|(_, rest)| rest).unwrap_or(rel);
    dest_root.join(below)
}

/// The media category of a staging-relative path: the component after `ready/`.
///
/// `ready/games/ps2/Title.iso` -> `games`. This is the key that routes a file to a
/// destination, which matters once movies, shows and games live on separate shares.
fn category_of(staging_relative: &str) -> String {
    staging_relative
        .strip_prefix("ready/")
        .unwrap_or(staging_relative)
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Whether a directory can actually be written to, tested rather than inferred.
///
/// Some NAS shares mark the share root read-only while its subdirectories are writable,
/// so permission bits alone are not a reliable answer.
fn is_writable_dir(dir: &Path) -> bool {
    let probe = dir.join(".dumo-write-test");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Whether `path` currently sits on a network filesystem.
///
/// This is the question that matters for a network destination, and it is not the same
/// as "is this a mount point": destinations are usually a *subdirectory* of the mount
/// (e.g. the share is at /mnt/nas/emulation but games belong in .../emulation/roms),
/// so a mount-point test gives a false negative on a perfectly good path.
///
/// Checking the filesystem type answers it directly. If the share is not mounted, the
/// path resolves to a local disk and this returns false — which is exactly the silent
/// failure worth catching, because writes would otherwise fill the root filesystem while
/// appearing to succeed.
fn is_network_filesystem(path: &Path) -> bool {
    // Magic numbers from statfs(2). Note SMB2 and the older CIFS value differ by a
    // single bit and are easy to confuse: a modern `vers=3.1.1` mount reports
    // 0xFE534D42, verified against a live share on this machine.
    const SMB2_MAGIC: i64 = 0xFE53_4D42;
    const CIFS_MAGIC: i64 = 0xFF53_4D42;
    const SMB_MAGIC: i64 = 0x0000_517B;
    const NFS_MAGIC: i64 = 0x0000_6969;
    const FUSE_MAGIC: i64 = 0x6573_5546; // sshfs and friends

    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return false;
    }
    let t = st.f_type as i64;
    matches!(t, SMB2_MAGIC | CIFS_MAGIC | SMB_MAGIC | NFS_MAGIC | FUSE_MAGIC)
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
    fn destination_path_drops_ready_and_category() {
        let d = destination_path(Path::new("/mnt/x"), "ready/games/ps2/Title (USA).iso");
        assert_eq!(d, PathBuf::from("/mnt/x/ps2/Title (USA).iso"));
    }

    #[test]
    fn category_is_the_component_after_ready() {
        assert_eq!(category_of("ready/games/ps2/Title.iso"), "games");
        assert_eq!(category_of("ready/movies/Title (2001).mkv"), "movies");
        assert_eq!(category_of("ready/tv/Show (1994)/Season 01/e.mkv"), "tv");
    }

    #[test]
    fn destination_path_preserves_the_tree_below_the_category() {
        // The category itself is consumed by routing; the rest of the path is kept.
        let d = destination_path(Path::new("/mnt/nas/emulation/roms"), "ready/games/ps2/T.iso");
        assert_eq!(d, PathBuf::from("/mnt/nas/emulation/roms/ps2/T.iso"));
    }

    /// Local paths must never be mistaken for a mounted share.
    #[test]
    fn local_paths_are_not_network_filesystems() {
        assert!(!is_network_filesystem(Path::new("/")));
        assert!(!is_network_filesystem(Path::new("/etc")));
    }

    #[test]
    fn a_missing_path_is_not_a_network_filesystem() {
        assert!(!is_network_filesystem(Path::new("/nonexistent/dumo/path")));
    }
}
