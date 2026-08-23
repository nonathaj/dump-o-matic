//! Bring existing library files under management (stage 0).
//!
//! Everything else in this tool operates on jobs, and jobs come from ripping a disc. A
//! library that predates the tool — or files copied in from anywhere else — is therefore
//! invisible to it: it cannot be verified, renamed, or repacked.
//!
//! `adopt` closes that gap by hashing loose files against the datfiles. An exact match
//! creates a job describing what the file is and where it already lives, after which the
//! ordinary commands work on it.
//!
//! Two deliberate limits. Only exact hash matches are adopted — a file we cannot name
//! with certainty is reported and left alone, because a wrong identification here would
//! propagate into a rename. And **nothing is moved, renamed or deleted**: adopting is a
//! bookkeeping operation. The file is described where it sits, and any change to it is a
//! later, separate decision made with `repack` and `migrate`.
//!
//! The resulting job records [`Job::adopted_from`], because an adopted file is not a
//! verified dump: there is no probe, no sector state, no dump log. All that is known is
//! that its hash matches a datfile entry today.

use anyhow::{bail, Context, Result};
use dumo_core::config::Config;
use dumo_core::job::{self, Job, JobStage};
use dumo_core::{hash, Identification, ReadyFile};
use dumo_identify::{es_de_slug, DatfileSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Extensions worth hashing. Sidecars and BIOS images are not disc dumps.
const IMAGE_EXTS: &[&str] = &["iso", "chd", "cue", "bin"];

pub struct AdoptArgs {
    /// Directory to scan. Defaults to every configured destination root.
    pub path: Option<PathBuf>,
    pub dry_run: bool,
    pub assume_yes: bool,
    pub config_file: Option<PathBuf>,
}

pub fn run(args: AdoptArgs) -> Result<()> {
    let cfg = Config::load(args.config_file.as_deref())?;

    let roots: Vec<PathBuf> = match &args.path {
        Some(p) => vec![p.clone()],
        None => cfg.destinations.iter().map(|d| d.root.clone()).collect(),
    };
    if roots.is_empty() {
        bail!("no --path given and no destinations configured; nothing to scan");
    }

    let dat_dir = cfg
        .datfiles
        .redump_dir
        .clone()
        .context("no datfiles.redump_dir configured; adopting needs the reference hashes")?;
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

    // Files an existing job already accounts for must not be adopted twice.
    let known = known_paths(&cfg)?;

    // Only look where a match is possible. A destination root holds one directory per
    // platform, and hashing a file whose platform has no datfile loaded cannot produce an
    // identification — it can only cost a full read. On a network share that is the
    // difference between minutes and hours: measured on this library, scanning
    // everything meant reading ~300 GB of Xbox 360 images to prove nothing, against
    // 11 PlayStation 2 files that could actually match.
    let covered: Vec<String> = set
        .datfiles()
        .iter()
        .filter_map(|d| es_de_slug(&d.platform).map(str::to_string))
        .collect();
    println!(
        "Platforms with datfiles: {}",
        if covered.is_empty() {
            "none".to_string()
        } else {
            covered.join(", ")
        }
    );

    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for root in &roots {
        if !root.is_dir() {
            println!("  {} is not a directory; skipping", root.display());
            continue;
        }
        // A root that *is* a platform directory is scanned whole — that is what an
        // explicit --path at a platform means.
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if covered.contains(&name) {
            collect_images(root, &mut candidates);
            continue;
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let n = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if covered.contains(&n) {
                    collect_images(&p, &mut candidates);
                } else {
                    skipped.push(n);
                }
            }
        }
        // Loose files directly in the root have no platform to infer, so they are still
        // worth hashing; there are rarely many.
        for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_file() && is_image(&p) && !file_name(&p).starts_with('.') {
                candidates.push(p);
            }
        }
    }
    if !skipped.is_empty() {
        skipped.sort();
        skipped.dedup();
        println!(
            "Skipped {} platform director{} with no datfile loaded: {}",
            skipped.len(),
            if skipped.len() == 1 { "y" } else { "ies" },
            skipped.join(", ")
        );
    }
    candidates.sort();
    candidates.retain(|p| !known.contains(p));

    if candidates.is_empty() {
        println!();
        println!("Nothing to adopt: every disc image found is already tracked by a job.");
        return Ok(());
    }

    println!();
    println!("Hashing {} untracked file(s):", candidates.len());

    let mut found: Vec<Adoptable> = Vec::new();
    let mut unmatched: Vec<PathBuf> = Vec::new();
    for path in &candidates {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        print!("  {} ({}) ... ", file_name(path), crate::migrate::human_size(size));
        std::io::stdout().flush().ok();

        // One pass for both digest sets. A library scan reads every file end to end, and
        // over a network share a second pass costs as much again for nothing.
        let (digests, sha256) = match hash::all_digests(path) {
            Ok(d) => d,
            Err(e) => {
                println!("unreadable: {e}");
                continue;
            }
        };
        match set.find(&digests) {
            Some(m) if m.confidence.is_auto_acceptable() && m.rom.size == digests.size => {
                let Some(slug) = es_de_slug(&m.platform).map(str::to_string) else {
                    println!("matched {:?} but no directory mapping for {:?}", m.game.name, m.platform);
                    continue;
                };
                println!("{}", m.game.name);
                found.push(Adoptable {
                    path: path.clone(),
                    sha256,
                    bytes: digests.size,
                    slug,
                    m,
                });
            }
            _ => {
                println!("no datfile match");
                unmatched.push(path.clone());
            }
        }
    }

    if !unmatched.is_empty() {
        println!();
        println!("Not adopted — no exact datfile match, so these cannot be named with");
        println!("certainty and are left exactly as they are:");
        for p in &unmatched {
            println!("  {}", p.display());
        }
    }

    if found.is_empty() {
        println!();
        println!("Nothing to adopt.");
        return Ok(());
    }

    println!();
    println!("To adopt:");
    for a in &found {
        println!("  {}", a.m.game.name);
        println!("      {} ", a.path.display());
    }
    println!();
    println!("Adopting only records what these files are. Nothing is moved, renamed or");
    println!("deleted, and no dump provenance is invented — the jobs are marked adopted.");

    if args.dry_run {
        println!();
        println!("Dry run: nothing was written.");
        return Ok(());
    }
    if !args.assume_yes && !crate::migrate::confirm("Create jobs for these?")? {
        println!("Aborted; nothing was written.");
        return Ok(());
    }

    println!();
    for a in &found {
        match adopt_one(a, &cfg) {
            Ok(id) => println!("  {} — adopted as {}", id, a.m.game.name),
            Err(e) => println!("  {} — FAILED: {e:#}", a.path.display()),
        }
    }

    println!();
    println!("Adopted {} file(s). `repack` and `migrate` now work on them;", found.len());
    println!("`verify` will re-check them against the manifest.");
    Ok(())
}

struct Adoptable {
    path: PathBuf,
    sha256: String,
    bytes: u64,
    slug: String,
    m: dumo_identify::datfile::DatMatch,
}

fn adopt_one(a: &Adoptable, cfg: &Config) -> Result<String> {
    let label = format!("{}-{}", a.slug, a.m.game.name);
    let job_id = job::new_job_id(Some(&label));
    let job_dir = cfg.staging.job_dir(&job_id);

    // The staging-relative path the rest of the pipeline speaks in. The file is not in
    // staging — it is already at the destination — but this is the form `migrate` and
    // `repack` map onto a destination root, so an adopted file addresses the same way a
    // ripped one does.
    let relative = format!("ready/games/{}/{}", a.slug, file_name(&a.path));

    let mut job = Job::new(job_id.clone(), String::new());
    Job::create_dir(&job_dir)?;
    job.adopted_from = Some(a.path.display().to_string());
    // Already in permanent storage; there is nothing to migrate unless it is repacked.
    job.stage = JobStage::Migrated;
    job.identification = Some(Identification {
        title: a.m.game.name.clone(),
        platform: a.m.platform.clone(),
        platform_slug: a.slug.clone(),
        matched_on: a.m.matched_on.to_string(),
        confidence: a.m.confidence,
        source: file_name(&a.m.source),
        files: vec![ReadyFile {
            path: relative,
            bytes: a.bytes,
            sha256: a.sha256.clone(),
        }],
        superseded: Vec::new(),
        identified_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    job.save(&job_dir)?;
    Ok(job_id)
}

/// Every file path an existing job already claims, at a destination or in staging.
fn known_paths(cfg: &Config) -> Result<Vec<PathBuf>> {
    let mut known = Vec::new();
    let jobs_dir = cfg.staging.jobs_dir();
    if !jobs_dir.is_dir() {
        return Ok(known);
    }
    for entry in std::fs::read_dir(&jobs_dir)?.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(job) = Job::load(&dir) else { continue };
        let Some(id) = &job.identification else {
            continue;
        };
        for f in id.files.iter().map(|f| f.path.clone()).chain(id.superseded.iter().cloned()) {
            known.push(cfg.staging.root.join(&f));
            let category = crate::migrate::category_of(&f);
            for d in cfg.destinations.iter().filter(|d| d.accepts(&category)) {
                known.push(crate::migrate::destination_path(&d.root, &f));
            }
        }
    }
    Ok(known)
}

/// Walk a tree collecting plausible disc images.
fn collect_images(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        // Hidden directories are conventionally where multi-disc sets are tucked away to
        // keep them out of a front-end's game list; still worth adopting.
        if p.is_dir() {
            collect_images(&p, out);
            continue;
        }
        if name.starts_with('.') {
            continue;
        }
        if is_image(&p) {
            out.push(p);
        }
    }
}

/// Whether a path looks like a disc image worth hashing.
fn is_image(p: &Path) -> bool {
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    IMAGE_EXTS.contains(&ext.as_str())
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("dumo-adopt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn touch(p: &Path) {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn collects_disc_images_and_recurses() {
        let dir = scratch("collect");
        touch(&dir.join("Game (USA).iso"));
        touch(&dir.join("Game (USA).chd"));
        touch(&dir.join("sub/Other (USA).cue"));
        touch(&dir.join("sub/Other (USA).bin"));

        let mut out = Vec::new();
        collect_images(&dir, &mut out);
        out.sort();
        assert_eq!(out.len(), 4, "got {out:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Sidecars, saves and cover art are not disc dumps; hashing every file in a library
    /// over a network share would be slow and pointless.
    #[test]
    fn ignores_files_that_are_not_disc_images() {
        let dir = scratch("filter");
        touch(&dir.join("Game (USA).iso"));
        for name in ["notes.txt", "cover.png", "Game.sav", "gamelist.xml", "archive.7z"] {
            touch(&dir.join(name));
        }
        let mut out = Vec::new();
        collect_images(&dir, &mut out);
        assert_eq!(out.len(), 1, "got {out:?}");
        assert!(out[0].ends_with("Game (USA).iso"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A dot-prefixed *file* is a stray or a partial write, not content. A dot-prefixed
    /// *directory* is the usual place multi-disc sets are hidden from a front-end, so it
    /// is still walked.
    #[test]
    fn skips_hidden_files_but_walks_hidden_directories() {
        let dir = scratch("hidden");
        touch(&dir.join(".partial.iso"));
        touch(&dir.join(".hidden/Game (USA) (Disc 1).chd"));
        touch(&dir.join(".hidden/Game (USA) (Disc 2).chd"));

        let mut out = Vec::new();
        collect_images(&dir, &mut out);
        out.sort();
        assert_eq!(out.len(), 2, "got {out:?}");
        assert!(out.iter().all(|p| p.extension().unwrap() == "chd"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let mut out = Vec::new();
        collect_images(Path::new("/definitely/not/here"), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn is_image_accepts_disc_containers_case_insensitively() {
        for n in ["a.iso", "a.ISO", "a.chd", "a.cue", "a.bin"] {
            assert!(is_image(Path::new(n)), "{n} should be an image");
        }
        for n in ["a.txt", "a.sav", "a.png", "a.m3u", "a", "a.iso.part"] {
            assert!(!is_image(Path::new(n)), "{n} should not be an image");
        }
    }
}
