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
//! What a job directory holds is not all the same kind of thing, and the three kinds want
//! different rules — see [`Tier`]. Age is deliberately not one of the rules: a two-year-old
//! dump whose destination copy has silently rotted is *less* safe to reclaim than
//! yesterday's, so "old enough" is never treated as a reason to delete. Neither is there a
//! force flag; skipping the proof would discard the only property that makes this command
//! trustworthy.

use anyhow::{bail, Result};
use dumo_core::config::Config;
use dumo_core::hash;
use dumo_core::job::{Job, JobStage};
use std::io::Write;
use std::path::PathBuf;

pub struct CleanArgs {
    /// Job id, or a fragment of one. Omit to consider every migrated job.
    pub job: Option<String>,
    /// Also drop the bulk provenance sidecars, which nothing can regenerate.
    pub provenance: bool,
    /// Also discard unfiled content — disc extras that were never identified.
    ///
    /// These exist nowhere but staging: nothing filed them, so there is no destination
    /// copy to verify against and nothing can bring them back. Discarding them is a
    /// decision about wanting the content, not a reclamation of something backed up,
    /// which is why it needs its own flag rather than riding along with the rest.
    pub discard_unfiled: bool,
    pub dry_run: bool,
    pub assume_yes: bool,
    pub config_file: Option<PathBuf>,
}

/// What a file in a job directory is, which determines whether it can be reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The dumped image itself. Redundant once packed and migrated: the CHD at the
    /// destination reproduces these bytes exactly, and that is re-proved before deletion.
    Redundant,
    /// Raw stream, per-sector state and subchannel data. Irreplaceable — nothing can
    /// regenerate them, least of all the packed image, which discards precisely this.
    /// Removed only when explicitly asked for.
    Provenance,
    /// Logs, TOCs and the cue sheet: the record of how the dump went. Kilobytes, and the
    /// only human-readable account of the read. Never removed; the space is not worth the
    /// loss of the audit trail.
    Audit,
}

/// Above this size, an Audit-tier file is content rather than a sidecar.
///
/// The tier is "things that are never removed", which is right for both, but they are
/// not the same thing and reporting them together is misleading: 5.9 GB of disc extras
/// described as "logs and TOCs" tells the operator nothing about where their space went.
const UNFILED_CONTENT_BYTES: u64 = 32 * 1024 * 1024;

/// Classify a job artifact by its extension.
pub fn tier_of(relative_path: &str) -> Tier {
    let ext = relative_path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "bin" | "iso" => Tier::Redundant,
        "scram" | "state" | "subcode" => Tier::Provenance,
        // Anything unrecognised is kept. A new sidecar should cost disk, not data.
        _ => Tier::Audit,
    }
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
    let mut redundant: Vec<RawFile> = Vec::new();
    let mut provenance: Vec<RawFile> = Vec::new();
    let mut audit_bytes = 0u64;
    let mut unfiled_bytes = 0u64;
    let mut unfiled: Vec<RawFile> = Vec::new();
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
        for a in &job.artifacts {
            if !dir.join(&a.relative_path).is_file() {
                continue;
            }
            match tier_of(&a.relative_path) {
                Tier::Redundant => redundant.push(RawFile {
                    job_dir: dir.clone(),
                    job_id: job.id.clone(),
                    relative: a.relative_path.clone(),
                    sha256: a.sha256.clone(),
                    bytes: a.bytes,
                }),
                Tier::Provenance => provenance.push(RawFile {
                    job_dir: dir.clone(),
                    job_id: job.id.clone(),
                    relative: a.relative_path.clone(),
                    sha256: a.sha256.clone(),
                    bytes: a.bytes,
                }),
                Tier::Audit => {
                    // Sidecars are kilobytes; anything substantial in this tier is
                    // content that was never filed — disc extras, most often — and it
                    // exists nowhere but here.
                    if a.bytes > UNFILED_CONTENT_BYTES {
                        unfiled_bytes += a.bytes;
                        unfiled.push(RawFile {
                            job_dir: dir.clone(),
                            job_id: job.id.clone(),
                            relative: a.relative_path.clone(),
                            sha256: a.sha256.clone(),
                            bytes: a.bytes,
                        });
                    } else {
                        audit_bytes += a.bytes;
                    }
                }
            }
        }

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

    let mut to_drop: Vec<&RawFile> = redundant.iter().collect();
    if args.provenance {
        to_drop.extend(provenance.iter());
    }
    if args.discard_unfiled {
        to_drop.extend(unfiled.iter());
    }

    if candidates.is_empty() && to_drop.is_empty() {
        println!("Nothing to reclaim.");
        if not_migrated > 0 {
            println!(
                "  {not_migrated} job(s) have not migrated yet; their staged content is the \
                 only copy and is left alone."
            );
        }
        report_retained(
            &provenance,
            audit_bytes,
            unfiled_bytes,
            args.provenance,
            args.discard_unfiled,
        );
        return Ok(());
    }

    let staged_total: u64 = candidates.iter().map(|c| c.bytes).sum();
    if !candidates.is_empty() {
        println!("Staged copies whose content has reached permanent storage:");
        for c in &candidates {
            println!("  {} ({})", c.relative, crate::migrate::human_size(c.bytes));
            println!("      keeping: {}", c.dest.display());
        }
        println!();
    }

    let raw_total: u64 = to_drop.iter().map(|f| f.bytes).sum();
    if !redundant.is_empty() {
        println!("Dumped images, reproducible from the packed copy at the destination:");
        for f in &redundant {
            println!("  {}/{} ({})", f.job_id, f.relative, crate::migrate::human_size(f.bytes));
        }
        println!("      each is proved by unpacking the destination copy and matching its");
        println!("      hash before the original is removed.");
        println!();
    }
    if args.provenance && !provenance.is_empty() {
        println!("Provenance, at your explicit request (NOTHING can regenerate these):");
        for f in &provenance {
            println!("  {}/{} ({})", f.job_id, f.relative, crate::migrate::human_size(f.bytes));
        }
        println!();
    }

    if args.discard_unfiled && !unfiled.is_empty() {
        println!("Unfiled content, at your explicit request (NOT backed up anywhere — this");
        println!("is a discard, not a reclamation, and it cannot be undone):");
        for f in &unfiled {
            println!(
                "  {}/{} ({})",
                f.job_id,
                f.relative,
                crate::migrate::human_size(f.bytes)
            );
        }
        println!();
    }

    println!(
        "Would reclaim {}.",
        crate::migrate::human_size(staged_total + raw_total)
    );
    if !candidates.is_empty() {
        println!("Each destination copy is re-hashed first; any that does not match means the");
        println!("staged file is the last good copy and it will be kept.");
    }
    report_retained(
        &provenance,
        audit_bytes,
        unfiled_bytes,
        args.provenance,
        args.discard_unfiled,
    );

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

    // --- Dumped images, proved reproducible before removal ---------------------------
    for f in &redundant {
        print!("  {}/{} ... ", f.job_id, f.relative);
        std::io::stdout().flush().ok();
        match prove_reproducible(f, &cfg) {
            Ok(true) => match std::fs::remove_file(f.job_dir.join(&f.relative)) {
                Ok(()) => {
                    freed += f.bytes;
                    record_reclaimed(&f.job_dir, &f.relative);
                    println!("reclaimed (destination copy unpacks to it exactly)");
                }
                Err(e) => {
                    println!("KEPT — could not remove: {e}");
                    kept += 1;
                }
            },
            Ok(false) => {
                println!("KEPT — the destination copy does not reproduce it");
                kept += 1;
            }
            Err(e) => {
                println!("KEPT — could not prove it: {e:#}");
                kept += 1;
            }
        }
    }

    // --- Unfiled content, only when explicitly asked for ------------------------------
    //
    // No proof step exists here and none is possible: nothing filed these, so there is
    // no destination copy to check them against. The flag is the whole safeguard.
    if args.discard_unfiled {
        for f in &unfiled {
            print!("  {}/{} ... ", f.job_id, f.relative);
            std::io::stdout().flush().ok();
            match std::fs::remove_file(f.job_dir.join(&f.relative)) {
                Ok(()) => {
                    freed += f.bytes;
                    record_reclaimed(&f.job_dir, &f.relative);
                    println!("discarded (unfiled; you asked)");
                }
                Err(e) => {
                    println!("KEPT — could not remove: {e}");
                    kept += 1;
                }
            }
        }
    }

    // --- Provenance, only when explicitly asked for -----------------------------------
    if args.provenance {
        for f in &provenance {
            print!("  {}/{} ... ", f.job_id, f.relative);
            std::io::stdout().flush().ok();
            match std::fs::remove_file(f.job_dir.join(&f.relative)) {
                Ok(()) => {
                    freed += f.bytes;
                    record_reclaimed(&f.job_dir, &f.relative);
                    println!("removed (irreplaceable; you asked)");
                }
                Err(e) => {
                    println!("KEPT — {e}");
                    kept += 1;
                }
            }
        }
    }

    println!();
    println!("Reclaimed {}.", crate::migrate::human_size(freed));
    if kept > 0 {
        println!("{kept} file(s) kept because they could not be proved safe to remove.");
    }
    Ok(())
}

/// Move an artifact from `artifacts` to `reclaimed` on the job.
///
/// The manifest must keep describing what is actually on disk, or `verify` reports a file
/// this command deliberately released as missing — and a job that is fine looks broken.
/// The path is retained under `reclaimed` so the record of what was here survives.
fn record_reclaimed(job_dir: &std::path::Path, relative: &str) {
    let Ok(mut job) = Job::load(job_dir) else {
        return;
    };
    job.artifacts.retain(|a| a.relative_path != relative);
    if !job.reclaimed.iter().any(|r| r == relative) {
        job.reclaimed.push(relative.to_string());
    }
    if let Err(e) = job.save(job_dir) {
        eprintln!("warning: could not record the reclaim in {}: {e}", job_dir.display());
    }
}

/// Say what is being kept and why, so the remaining space is explained.
fn report_retained(
    provenance: &[RawFile],
    audit_bytes: u64,
    unfiled_bytes: u64,
    dropping_provenance: bool,
    dropping_unfiled: bool,
) {
    if !dropping_provenance {
        let p: u64 = provenance.iter().map(|f| f.bytes).sum();
        if p > 0 {
            println!(
                "Provenance retained ({}): raw stream, sector state and subchannel data. \
                 Nothing can regenerate these — pass --provenance to drop them.",
                crate::migrate::human_size(p)
            );
        }
    }
    if audit_bytes > 0 {
        println!(
            "Audit trail retained ({}): logs and TOCs. Never removed.",
            crate::migrate::human_size(audit_bytes)
        );
    }
    if unfiled_bytes > 0 && !dropping_unfiled {
        println!(
            "Unfiled content retained ({}): titles that were never migrated — disc extras \
             and the like. This is their only copy. Pass --discard-unfiled to drop them.",
            crate::migrate::human_size(unfiled_bytes)
        );
    }
}

/// Prove the destination copy unpacks to exactly the bytes of this dumped image.
///
/// The round-trip was checked when the image was packed, but that was then. Rather than
/// trust a past record, the packed copy is unpacked again now and the result compared to
/// the hash recorded for the file about to be deleted. Costs one extraction; buys the
/// same guarantee the rest of the pipeline gives.
fn prove_reproducible(f: &RawFile, cfg: &Config) -> Result<bool> {
    use dumo_backends::chdman;

    let job = Job::load(&f.job_dir)?;
    let Some(id) = job.identification.as_ref() else {
        bail!("job has no identification");
    };
    let Some(packed) = id
        .files
        .iter()
        .find(|x| x.path.to_ascii_lowercase().ends_with(".chd"))
    else {
        bail!("no packed copy recorded for this job");
    };

    // Wherever it actually lives now.
    let staged = cfg.staging.root.join(&packed.path);
    let chd = if staged.is_file() {
        staged
    } else {
        let category = crate::migrate::category_of(&packed.path);
        cfg.destinations
            .iter()
            .find(|d| d.accepts(&category))
            .map(|d| crate::migrate::destination_path(&d.root, &packed.path))
            .filter(|p| p.is_file())
            .ok_or_else(|| anyhow::anyhow!("packed copy is nowhere to be found"))?
    };

    // A cue sheet in the job means the dump was a CD.
    let format = if job
        .artifacts
        .iter()
        .any(|a| a.relative_path.to_ascii_lowercase().ends_with(".cue"))
    {
        chdman::DiscFormat::Cd
    } else {
        chdman::DiscFormat::Dvd
    };

    let work = f.job_dir.join("clean-verify");
    if work.exists() {
        std::fs::remove_dir_all(&work).ok();
    }
    let round = chdman::verify_roundtrip(&chd, &work, "check", format);
    let result = (|| -> Result<bool> {
        let round = round?;
        for t in &round.tracks {
            let (sha256, _) = hash::sha256_file(t)?;
            if sha256 == f.sha256 {
                return Ok(true);
            }
        }
        Ok(false)
    })();
    std::fs::remove_dir_all(&work).ok();
    result
}

/// A file inside a job directory that a tier rule might release.
struct RawFile {
    job_dir: PathBuf,
    job_id: String,
    relative: String,
    sha256: String,
    bytes: u64,
}

struct Candidate {
    relative: String,
    staged: PathBuf,
    dest: PathBuf,
    sha256: String,
    bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole command turns on this classification, so it is worth pinning down.
    #[test]
    fn tiers_split_image_from_evidence_from_audit() {
        // The dumped image: reproducible from the packed copy at the destination.
        assert_eq!(tier_of("raw/SLUS-21038.bin"), Tier::Redundant);
        assert_eq!(tier_of("raw/GAME.iso"), Tier::Redundant);
        assert_eq!(tier_of("raw/GAME.ISO"), Tier::Redundant);

        // Evidence about the read itself. A CHD discards exactly this, so nothing can
        // regenerate it.
        assert_eq!(tier_of("raw/SLUS-21038.scram"), Tier::Provenance);
        assert_eq!(tier_of("raw/SLUS-21038.state"), Tier::Provenance);
        assert_eq!(tier_of("raw/SLUS-21038.subcode"), Tier::Provenance);

        // The account of how the dump went — kilobytes, never worth deleting.
        assert_eq!(tier_of("raw/SLUS-21038.log"), Tier::Audit);
        assert_eq!(tier_of("raw/SLUS-21038.toc"), Tier::Audit);
        assert_eq!(tier_of("raw/SLUS-21038.fulltoc"), Tier::Audit);
        // The cue is tiny and chdman does not reproduce its text byte for byte, so it is
        // kept rather than claimed reproducible.
        assert_eq!(tier_of("raw/SLUS-21038.cue"), Tier::Audit);
    }

    /// An unrecognised sidecar must cost disk space, not data. A future redumper version
    /// emitting a new file type should never have it silently deleted.
    #[test]
    fn unknown_extensions_are_kept() {
        assert_eq!(tier_of("raw/SLUS-21038.newthing"), Tier::Audit);
        assert_eq!(tier_of("raw/noextension"), Tier::Audit);
        assert_eq!(tier_of(""), Tier::Audit);
    }

    /// Video rips are not packed into anything, so their .mkv files must never be
    /// classified as reproducible.
    #[test]
    fn video_rips_are_not_treated_as_reproducible() {
        assert_eq!(tier_of("raw/B1_t00.mkv"), Tier::Audit);
        assert_eq!(tier_of("raw/title.m2ts"), Tier::Audit);
    }
}
