//! Re-package already-identified content into the configured container (stage 3b).
//!
//! Policy changes, and content filed under an older policy should be able to catch up
//! without being re-ripped from a disc that may no longer be to hand. The concrete case
//! this was written for: `.iso` images filed before CHD packaging existed.
//!
//! Repacking is deliberately *not* a delete-and-replace. The old file may already be on
//! permanent storage, so this stage only ever **adds**: it writes the new container into
//! `ready/` and records the path it supersedes on the job. Removing the old file is left
//! to `migrate`, which does it only after the replacement has landed at the destination
//! and been verified there. At no point does a valid copy stop existing.
//!
//! As everywhere else, the conversion is proven rather than assumed: the new container is
//! unpacked straight back out and compared against the datfile hashes the content was
//! originally identified by. A repack that cannot reproduce the original bytes is
//! discarded.

use anyhow::{bail, Context, Result};
use dumo_backends::chdman;
use dumo_core::config::Config;
use dumo_core::job::{Job, JobStage};
use dumo_core::{hash, ReadyFile};
use dumo_identify::{es_de_slug, DatfileSet};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct RepackArgs {
    /// Job id, or a fragment of one. Omit to consider every eligible job.
    pub job: Option<String>,
    pub dry_run: bool,
    pub assume_yes: bool,
    pub config_file: Option<PathBuf>,
}

pub fn run(args: RepackArgs) -> Result<()> {
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

    // Hashes to check the round-trip against. Without them there is nothing to prove the
    // repack correct, so this is a hard requirement rather than a nice-to-have.
    let dat_dir = cfg
        .datfiles
        .redump_dir
        .clone()
        .context("no datfiles.redump_dir configured; repacking needs the reference hashes")?;
    let set = DatfileSet::load_dir(&dat_dir)
        .with_context(|| format!("loading datfiles from {}", dat_dir.display()))?;
    if set.is_empty() {
        bail!("no datfiles loaded from {}", dat_dir.display());
    }
    println!(
        "Loaded {} games across {} datfile(s)",
        set.game_count(),
        set.datfiles().len()
    );

    let mut planned: Vec<Plan> = Vec::new();
    for dir in &dirs {
        match plan_job(dir, &cfg) {
            Ok(Some(p)) => planned.push(p),
            Ok(None) => {}
            Err(e) => println!("  {}: skipped — {e}", dir.display()),
        }
    }

    if planned.is_empty() {
        println!();
        println!("Nothing to repack: every identified file already matches the configured");
        println!("container policy (games.chd_platforms).");
        return Ok(());
    }

    println!();
    println!("Plan:");
    for p in &planned {
        println!(
            "  {}  ->  {}",
            p.old_relative,
            p.new_relative_display()
        );
        println!(
            "      source: {} ({})",
            p.source.display(),
            if p.from_destination {
                "from permanent storage; staging copy is gone"
            } else {
                "from staging"
            }
        );
    }
    println!();
    println!("The new file is written to staging only. Nothing is deleted here —");
    println!("`migrate` removes the superseded file after the replacement verifies.");

    if args.dry_run {
        println!();
        println!("Dry run: nothing was written.");
        return Ok(());
    }
    if !args.assume_yes && !crate::migrate::confirm("Proceed with repack?")? {
        println!("Aborted; nothing was written.");
        return Ok(());
    }

    println!();
    let mut failures = 0;
    for p in &planned {
        if let Err(e) = repack_one(p, &cfg, &set) {
            failures += 1;
            println!("     {e:#}");
            println!("     Nothing was changed for this job.");
        }
    }

    println!();
    if failures > 0 {
        println!("{failures} job(s) failed to repack; their existing files are untouched.");
        std::process::exit(1);
    }
    println!("Repack complete. Run `dump-o-matic migrate` to place the new files");
    println!("and retire the ones they replace.");
    Ok(())
}

struct Plan {
    job_dir: PathBuf,
    job_id: String,
    /// Staging-relative path of the file being replaced.
    old_relative: String,
    /// Where the bytes are read from: staging if it still has them, else the destination.
    source: PathBuf,
    from_destination: bool,
    slug: String,
    title: String,
    format: chdman::DiscFormat,
    sha1: String,
}

impl Plan {
    fn new_relative_display(&self) -> String {
        format!("ready/games/{}/{}.chd", self.slug, self.title)
    }
}

/// Decide whether a job has anything to repack, and where its bytes are.
fn plan_job(job_dir: &Path, cfg: &Config) -> Result<Option<Plan>> {
    let job = Job::load(job_dir)?;
    if !matches!(job.stage, JobStage::Identified | JobStage::Migrated) {
        return Ok(None);
    }
    let Some(id) = &job.identification else {
        return Ok(None);
    };
    // One image per job for now; a multi-track set is already packed at identify time.
    if id.files.len() != 1 {
        return Ok(None);
    }
    let f = &id.files[0];
    let slug = es_de_slug(&id.platform).unwrap_or(&id.platform_slug).to_string();
    if !cfg.games.packs_chd(&slug) {
        return Ok(None);
    }

    let lower = f.path.to_ascii_lowercase();
    if lower.ends_with(".chd") {
        return Ok(None); // Already in the configured container.
    }
    let format = if lower.ends_with(".iso") {
        chdman::DiscFormat::Dvd
    } else if lower.ends_with(".cue") || lower.ends_with(".bin") {
        // A cue/bin set filed before CHD packaging existed needs the whole set moved
        // together, which the single-file path here cannot express safely.
        bail!("{} is part of a multi-file set; repack that by re-running identify", f.path);
    } else {
        return Ok(None);
    };

    // Prefer the staging copy; fall back to permanent storage when staging was reclaimed.
    let staged = cfg.staging.root.join(&f.path);
    let (source, from_destination) = if staged.is_file() {
        (staged, false)
    } else {
        let category = crate::migrate::category_of(&f.path);
        let dest = cfg
            .destinations
            .iter()
            .find(|d| d.accepts(&category))
            .map(|d| crate::migrate::destination_path(&d.root, &f.path))
            .filter(|p| p.is_file())
            .with_context(|| {
                format!("{} is neither in staging nor at any configured destination", f.path)
            })?;
        (dest, true)
    };

    Ok(Some(Plan {
        job_dir: job_dir.to_path_buf(),
        job_id: job.id.clone(),
        old_relative: f.path.clone(),
        source,
        from_destination,
        slug,
        title: id.title.clone(),
        format,
        sha1: String::new(),
    }))
}

fn repack_one(p: &Plan, cfg: &Config, set: &DatfileSet) -> Result<()> {
    print!("  {} ... ", p.job_id);
    std::io::stdout().flush().ok();

    // Confirm the source is what the datfile says before converting it. Repacking a file
    // that no longer matches its identification would launder a corruption into a new
    // container with a fresh, valid-looking hash.
    let digests = hash::redump_digests(&p.source)
        .with_context(|| format!("hashing {}", p.source.display()))?;
    let Some(m) = set.find(&digests) else {
        bail!(
            "{} does not match any datfile entry any more; refusing to repack it",
            p.source.display()
        );
    };
    if m.game.name != p.title {
        bail!(
            "{} now identifies as {:?}, not {:?}",
            p.source.display(),
            m.game.name,
            p.title
        );
    }

    let work = p.job_dir.join("repack");
    if work.exists() {
        bail!("{} already exists", work.display());
    }
    std::fs::create_dir_all(&work)?;
    let cleanup = || {
        std::fs::remove_dir_all(&work).ok();
    };

    let chd = work.join(format!("{}.chd", p.title));
    if let Err(e) = chdman::create(&p.source, &chd, p.format) {
        cleanup();
        return Err(e).context("packing");
    }

    // Prove it reverses before anything downstream can act on it.
    let verify_dir = work.join("verify");
    let round = match chdman::verify_roundtrip(&chd, &verify_dir, &p.title, p.format) {
        Ok(r) => r,
        Err(e) => {
            cleanup();
            return Err(e).context("unpacking to verify");
        }
    };
    let mut expected: Vec<String> = m
        .game
        .roms
        .iter()
        .filter(|r| !r.name.to_ascii_lowercase().ends_with(".cue"))
        .map(|r| r.sha1.to_ascii_lowercase())
        .collect();
    let mut actual: Vec<String> = Vec::new();
    for t in &round.tracks {
        actual.push(hash::redump_digests(t)?.sha1.to_ascii_lowercase());
    }
    expected.sort();
    actual.sort();
    if expected != actual {
        cleanup();
        bail!(
            "the repacked file does not reproduce the original bytes\n       redump:    {expected:?}\n       extracted: {actual:?}"
        );
    }
    std::fs::remove_dir_all(&verify_dir).ok();

    let dest_rel = format!("ready/games/{}/{}.chd", p.slug, p.title);
    let dest = cfg.staging.root.join(&dest_rel);
    let outcome = match dumo_core::fsops::move_verified(&chd, &dest, None) {
        Ok(o) => o,
        Err(e) => {
            cleanup();
            return Err(e).context("placing the repacked file in staging");
        }
    };
    cleanup();

    let (sha256, bytes) = hash::sha256_file(&dest)?;

    // Record the swap. The old path is retained as `superseded`, not deleted: it may be
    // the only copy at the destination until migrate places this one.
    let mut job = Job::load(&p.job_dir)?;
    if let Some(id) = job.identification.as_mut() {
        id.superseded.push(p.old_relative.clone());
        id.files = vec![ReadyFile {
            path: dest_rel,
            bytes,
            sha256,
        }];
    }
    // Back to Identified: there is now content that has not reached permanent storage.
    job.stage = JobStage::Identified;
    job.save(&p.job_dir)?;

    let saved = outcome.bytes;
    println!(
        "packed and verified ({} -> {})",
        crate::migrate::human_size(digests.size),
        crate::migrate::human_size(saved)
    );
    println!(
        "     round-trip checked: extracts back to the datfile's SHA-1{}",
        if p.from_destination {
            "; source was read from permanent storage and left untouched"
        } else {
            ""
        }
    );
    Ok(())
}
