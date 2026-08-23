//! The `identify` command: Stage 3, work out what a staged job actually contains.
//!
//! Read-only. It hashes staged artifacts, matches them against datfiles, and prints the
//! proposed naming — it does not rename or move anything. Applying the result is a
//! separate, explicit step.

use anyhow::{bail, Context, Result};
use dumo_backends::chdman;
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
    /// Search term for a video disc, overriding the guess from the volume label.
    pub show: Option<String>,
    /// Solve all matching video discs together as one set.
    pub set: bool,
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

    if args.set {
        return solve_video_set(&dirs, &cfg, args.show.as_deref());
    }

    for dir in dirs {
        identify_job(&dir, &set, &cfg, args.apply, args.show.as_deref())?;
        println!();
    }
    Ok(())
}

/// Where the bytes of a file we are about to file come from.
enum Source {
    /// An artifact sitting in the job directory, to be moved into place.
    Artifact { path: PathBuf, relative: String },
    /// Content rebuilt into Redump's canonical form rather than copied — a cue sheet,
    /// which names its track files and so cannot survive the rename unchanged. These
    /// bytes are only ever used after their digests have been checked against the
    /// datfile, so a reconstruction we got wrong is refused, not written.
    Reconstructed(Vec<u8>),
}

/// A file that has been identified and can be filed under its archival name.
struct Applicable {
    source: Source,
    sha256: String,
    m: dumo_identify::datfile::DatMatch,
}

fn identify_job(
    job_dir: &std::path::Path,
    set: &DatfileSet,
    cfg: &Config,
    apply: bool,
    show: Option<&str>,
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
    if matches!(kind, MediaKind::DvdVideo | MediaKind::BluRayVideo) {
        return analyse_video(&job, job_dir, cfg, show.as_deref());
    }
    if !matches!(kind, MediaKind::GameDisc | MediaKind::Data) {
        println!("  {kind} — no identification path for this media type yet.");
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
    let mut applicable: Vec<Applicable> = Vec::new();

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
                        "      set:        {} files in the datfile; the rest are resolved below,\n                  \
                         and none are filed until every one is verified.",
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
                        applicable.push(Applicable {
                            source: Source::Artifact {
                                path: path.clone(),
                                relative: a.relative_path.clone(),
                            },
                            sha256: a.sha256.clone(),
                            m,
                        });
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
            // A cue sheet naming redumper's file names cannot match a datfile that
            // names Redump's; that is expected, and reconstruct_cues handles it below.
            None if a.relative_path.to_ascii_lowercase().ends_with(".cue") => {
                println!("    no direct match — cue sheets name their track files, so");
                println!("      this one is rebuilt from the identified tracks instead.");
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

    reconstruct_cues(&job, job_dir, &mut applicable);

    if apply {
        apply_matches(&mut job, job_dir, cfg, &applicable)?;
    } else if !applicable.is_empty() {
        println!("  (re-run with --apply to move these into ready/)");
    }

    Ok(())
}

/// How much of a title's subtitle track to read, in seconds.
///
/// The opening quarter-hour is ample to tell episodes apart, and stopping there keeps a
/// whole box set to seconds rather than minutes of ffmpeg time.
const DIALOGUE_WINDOW_SECS: u32 = 900;

/// Tokenised dialogue for one title, or `None` if it has no usable subtitle track.
fn read_dialogue(path: &std::path::Path) -> Option<std::collections::HashSet<String>> {
    let srt = dumo_backends::subtitles::extract_text(path, DIALOGUE_WINDOW_SECS).ok()?;
    let words = dumo_backends::subtitles::tokenize(&dumo_backends::subtitles::dialogue_text(&srt));
    // A handful of words is a stray forced-subtitle caption, not dialogue; scoring on it
    // would be worse than admitting we have no signal.
    (words.len() >= 20).then_some(words)
}

/// Rebuild cue sheets into Redump's canonical form, completing CD sets.
///
/// A CD dump's `.cue` is generated text that names its track files. redumper names them
/// after the image and writes LF endings; Redump names them after the game and writes
/// CRLF. So a bit-perfect CD dump arrives with a cue matching no datfile entry, and
/// without this step every CD title would stay permanently "incomplete" and unfilable —
/// the `.bin` alone is never the whole set.
///
/// The rebuilt cue is *proposed*, not asserted: it is hashed and compared against the
/// datfile's own entry for that game. Only a byte-exact match is added, so if the
/// reconstruction is wrong in any way the set stays incomplete and nothing is filed.
/// Nothing in the job directory is modified — the archival dump is left as it was read.
fn reconstruct_cues(job: &Job, job_dir: &std::path::Path, applicable: &mut Vec<Applicable>) {
    // The cue can only be rebuilt once the track files have archival names to point at.
    let renames: Vec<(String, String)> = applicable
        .iter()
        .filter_map(|i| match &i.source {
            Source::Artifact { path, .. } => Some((
                path.file_name()?.to_string_lossy().to_string(),
                i.m.rom.name.clone(),
            )),
            Source::Reconstructed(_) => None,
        })
        .collect();
    if renames.is_empty() {
        return;
    }

    // Every matched track must belong to one game before rebuilding anything: a cue
    // spanning two different titles is a situation we do not understand well enough to
    // generate a file for.
    let Some(first) = applicable.first() else {
        return;
    };
    if applicable.iter().any(|i| i.m.game.name != first.m.game.name) {
        return;
    }
    let Some(cue_rom) = first
        .m
        .game
        .roms
        .iter()
        .find(|r| r.name.to_ascii_lowercase().ends_with(".cue"))
    else {
        return;
    };
    if applicable
        .iter()
        .any(|i| i.m.rom.name.eq_ignore_ascii_case(&cue_rom.name))
    {
        return; // Already accounted for.
    }

    let Some(src) = job
        .artifacts
        .iter()
        .find(|a| a.relative_path.to_ascii_lowercase().ends_with(".cue"))
    else {
        return;
    };
    let Ok(raw) = std::fs::read(job_dir.join(&src.relative_path)) else {
        return;
    };

    let rebuilt = dumo_identify::cue::retarget(&raw, &renames);
    let digests = hash::redump_digests_of(&rebuilt);

    println!("  {} — rebuilding as {}", src.relative_path, cue_rom.name);
    if digests.sha1 != cue_rom.sha1 || digests.size != cue_rom.size {
        println!("    the rebuilt cue does not match Redump's entry:");
        println!("      ours:   {} bytes, sha1 {}", digests.size, digests.sha1);
        println!("      redump: {} bytes, sha1 {}", cue_rom.size, cue_rom.sha1);
        println!("    leaving the set incomplete rather than filing a cue we invented.");
        return;
    }

    println!("    matches Redump exactly — the set is now complete.");
    applicable.push(Applicable {
        sha256: hash::sha256_of(&rebuilt),
        source: Source::Reconstructed(rebuilt),
        m: dumo_identify::datfile::DatMatch {
            rom: cue_rom.clone(),
            ..first.m.clone()
        },
    });
}

/// Pack a verified CD set into a single CHD, or return `Ok(None)` to file it as-is.
///
/// Why this exists at all is in [`dumo_backends::chdman`]: a `.cue`/`.bin` pair is the
/// right archival form but the wrong library form, and for `ps2` in particular the cue is
/// not even scanned. CHD is one file, read natively, and losslessly reversible.
///
/// That last property is the only reason this is allowed to replace the Redump files, so
/// it is proven rather than assumed. The CHD is extracted straight back out and every
/// track compared against the Redump SHA-1s the dump already matched. If anything
/// differs — or chdman is missing, or the round-trip fails — nothing is filed as a CHD
/// and the caller falls back to filing the original files untouched.
///
/// The job's own `raw/` tree is never modified: track files are hard-linked into a work
/// area, so this costs no extra copy of the source and cannot damage the dump.
fn package_chd(
    job_dir: &std::path::Path,
    cfg: &Config,
    matches: &[Applicable],
) -> Result<Option<(PathBuf, dumo_identify::datfile::DatMatch)>> {
    let Some(first) = matches.first() else {
        return Ok(None);
    };
    let m = &first.m;

    let Some(slug) = es_de_slug(&m.platform) else {
        return Ok(None);
    };
    if !cfg.games.packs_chd(slug) {
        return Ok(None);
    }

    // The container follows the media, not the platform: a PS2 library holds both CD and
    // DVD titles and chdman needs a different subcommand for each.
    let is_cd = m
        .game
        .roms
        .iter()
        .any(|r| r.name.to_ascii_lowercase().ends_with(".cue"));
    let format = if is_cd {
        chdman::DiscFormat::Cd
    } else if m
        .game
        .roms
        .iter()
        .any(|r| r.name.to_ascii_lowercase().ends_with(".iso"))
    {
        chdman::DiscFormat::Dvd
    } else {
        return Ok(None);
    };

    match chdman::version() {
        Ok(v) => println!("  packaging as CHD ({v})"),
        Err(e) => {
            println!("  not packaging as CHD: {e}");
            println!("    filing the .cue/.bin set instead; install chdman (mame-tools) to pack it.");
            return Ok(None);
        }
    }

    let work = job_dir.join("package");
    if work.exists() {
        println!("  not packaging as CHD: {} already exists", work.display());
        return Ok(None);
    }
    std::fs::create_dir_all(&work).context("creating CHD work directory")?;

    // Assemble the set under its archival names. chdman resolves track files relative to
    // the cue, exactly as an emulator would, so the names have to be right here.
    let mut assembled = Ok(());
    for item in matches {
        let dest = work.join(&item.m.rom.name);
        let r = match &item.source {
            Source::Reconstructed(bytes) => {
                dumo_core::fsops::write_verified(&dest, bytes, &item.sha256)
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            }
            // A hard link costs nothing and shares the bytes with raw/, so packaging a
            // 400 MB track needs no second copy of it.
            Source::Artifact { path, .. } => std::fs::hard_link(path, &dest)
                .map_err(anyhow::Error::from)
                .with_context(|| format!("linking {} into the work area", item.m.rom.name)),
        };
        if let Err(e) = r {
            assembled = Err(e);
            break;
        }
    }
    let cleanup = |work: &std::path::Path| {
        // Only ever removes the work area we just created; raw/ holds the real files and
        // the hard links here are additional names for them, not the data itself.
        std::fs::remove_dir_all(work).ok();
    };
    if let Err(e) = assembled {
        cleanup(&work);
        println!("  not packaging as CHD: {e}");
        return Ok(None);
    }

    // chdman is pointed at the cue for a CD and the iso itself for a DVD.
    let wanted = if is_cd { ".cue" } else { ".iso" };
    let input = m
        .game
        .roms
        .iter()
        .find(|r| r.name.to_ascii_lowercase().ends_with(wanted))
        .map(|r| work.join(&r.name))
        .expect("checked above");
    let title = m.game.name.clone();
    let chd = work.join(format!("{title}.chd"));

    print!("  -> {slug}/{title}.chd ... ");
    std::io::stdout().flush().ok();
    if let Err(e) = chdman::create(&input, &chd, format) {
        println!("FAILED");
        println!("     {e}");
        cleanup(&work);
        return Ok(None);
    }

    // --- Prove the round-trip before trusting the CHD --------------------------------
    let verify_dir = work.join("verify");
    let round = match chdman::verify_roundtrip(&chd, &verify_dir, &title, format) {
        Ok(r) => r,
        Err(e) => {
            println!("FAILED");
            println!("     could not extract the CHD back out: {e}");
            println!("     Filing the original files instead; nothing was deleted.");
            cleanup(&work);
            return Ok(None);
        }
    };

    // Every track Redump lists must come back out with the same SHA-1. Compared as
    // multisets so track order cannot mask a swap.
    // The cue is chdman's own text on the way back out and is regenerated rather than
    // preserved, so only the data files are compared. For a DVD that is the .iso itself.
    let mut expected: Vec<String> = m
        .game
        .roms
        .iter()
        .filter(|r| !r.name.to_ascii_lowercase().ends_with(".cue"))
        .map(|r| r.sha1.to_ascii_lowercase())
        .collect();
    let mut actual: Vec<String> = Vec::new();
    for t in &round.tracks {
        match hash::redump_digests(t) {
            Ok(d) => actual.push(d.sha1.to_ascii_lowercase()),
            Err(e) => {
                println!("FAILED");
                println!("     could not hash the extracted track: {e}");
                cleanup(&work);
                return Ok(None);
            }
        }
    }
    expected.sort();
    actual.sort();

    if expected != actual {
        println!("FAILED");
        println!("     the CHD does not reproduce the dump byte for byte:");
        println!("       redump:    {expected:?}");
        println!("       extracted: {actual:?}");
        println!("     Filing the original files instead; nothing was deleted.");
        cleanup(&work);
        return Ok(None);
    }

    // The extracted copies have served their purpose and are pure duplicates of data we
    // still hold in raw/.
    std::fs::remove_dir_all(&verify_dir).ok();

    let dest = cfg
        .staging
        .ready_category_dir("games")
        .join(slug)
        .join(format!("{title}.chd"));
    match dumo_core::fsops::move_verified(&chd, &dest, None) {
        Ok(_) => {
            println!("packed and verified");
            println!(
                "     round-trip checked: {} track(s) extract back to Redump's SHA-1s",
                actual.len()
            );
            cleanup(&work);
            Ok(Some((dest, m.clone())))
        }
        Err(e) => {
            println!("FAILED");
            println!("     {e}");
            println!("     Nothing was moved or deleted.");
            cleanup(&work);
            Ok(None)
        }
    }
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
    matches: &[Applicable],
) -> Result<()> {
    if matches.is_empty() {
        return Ok(());
    }

    // Guard against filing an incomplete multi-file set as though it were whole.
    if let Some(first) = matches.first() {
        let first = &first.m;
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

    // Every file in the set must be exact before packaging is even considered; a CHD
    // built from a set we were unsure of would bury that uncertainty in a new file.
    let all_exact = matches.iter().all(|i| i.m.confidence.is_auto_acceptable());
    if all_exact {
        if let Some((dest, m)) = package_chd(job_dir, cfg, matches)? {
            let (sha256, bytes) = hash::sha256_file(&dest).context("hashing the packed CHD")?;
            record_identification(
                job,
                &m,
                vec![dumo_core::ReadyFile {
                    path: relative_to_staging(&dest, &cfg.staging.root),
                    bytes,
                    sha256,
                }],
            );
            job.save(job_dir)?;
            println!("  job stage: identified");
            println!("  the archival .cue/.bin stay in the job directory as provenance");
            return Ok(());
        }
    }

    // Disc images are the 'games' category; the category is the routing key
    // migrate uses to pick a destination.
    let ready_root = cfg.staging.ready_category_dir("games");
    let mut moved: Vec<dumo_core::ReadyFile> = Vec::new();
    let mut reference: Option<dumo_identify::datfile::DatMatch> = None;

    for item in matches {
        let (sha256, m) = (&item.sha256, &item.m);
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

        // Both paths end the same way: nothing exists at the destination until its
        // content has been read back and proven to match.
        let placed = match &item.source {
            Source::Artifact { path, .. } => {
                dumo_core::fsops::move_verified(path, &dest, Some(sha256)).map(|o| {
                    let how = match o.method {
                        dumo_core::fsops::MoveMethod::Rename => "moved",
                        dumo_core::fsops::MoveMethod::CopyVerified => "copied and verified",
                    };
                    (how, o.bytes)
                })
            }
            Source::Reconstructed(bytes) => dumo_core::fsops::write_verified(&dest, bytes, sha256)
                .map(|_| ("written and verified", bytes.len() as u64)),
        };

        match placed {
            Ok((how, bytes)) => {
                println!("{how}");
                moved.push(dumo_core::ReadyFile {
                    path: relative_to_staging(&dest, &cfg.staging.root),
                    bytes,
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
        .filter_map(|i| match &i.source {
            Source::Artifact { relative, .. } => Some(relative.clone()),
            // Reconstructed content was never an artifact, so there is nothing to drop.
            Source::Reconstructed(_) => None,
        })
        .collect();
    job.artifacts
        .retain(|a| !moved_names.contains(&a.relative_path));

    if let Some(m) = reference {
        record_identification(job, &m, moved);
        job.save(job_dir)?;
        println!("  job stage: identified");
        println!("  provenance (logs, sector state) stays in the job directory");
    }

    Ok(())
}

/// Record what a job was identified as, and which files now carry it.
///
/// Shared by both filing routes — the plain move and the CHD package — so the manifest
/// says the same things about a job however its content was packaged.
fn record_identification(
    job: &mut Job,
    m: &dumo_identify::datfile::DatMatch,
    files: Vec<dumo_core::ReadyFile>,
) {
    job.identification = Some(dumo_core::Identification {
        title: m.game.name.clone(),
        platform: m.platform.clone(),
        platform_slug: es_de_slug(&m.platform).unwrap_or("unknown").to_string(),
        matched_on: m.matched_on.to_string(),
        confidence: m.confidence,
        source: file_name(&m.source),
        files,
        superseded: Vec::new(),
        identified_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    job.stage = dumo_core::JobStage::Identified;
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

/// Analyse a video job's structure: what is content, what is an extra, and whether the
/// disc looks like a film or a set of episodes.
///
/// This is offline inference from duration and chapter layout. It deliberately stops
/// short of naming anything — that needs an online lookup, and it is a fuzzy match that
/// must always be confirmed.
fn analyse_video(
    job: &Job,
    job_dir: &std::path::Path,
    cfg: &Config,
    show_override: Option<&str>,
) -> Result<()> {
    use dumo_identify::video::{self, AnalysisParams, TitleInput};

    let media: Vec<_> = job
        .artifacts
        .iter()
        .filter(|a| {
            let p = a.relative_path.to_ascii_lowercase();
            p.ends_with(".mkv") || p.ends_with(".mp4") || p.ends_with(".m2ts")
        })
        .collect();

    if media.is_empty() {
        println!("  no video files among the artifacts");
        return Ok(());
    }

    let mut inputs = Vec::new();
    for a in &media {
        let path = job_dir.join(&a.relative_path);
        if !path.is_file() {
            println!("  {} — MISSING", a.relative_path);
            continue;
        }
        let f = dumo_backends::ffprobe::probe(&path)
            .with_context(|| format!("probing {}", path.display()))?;
        inputs.push(TitleInput {
            name: a.relative_path.clone(),
            duration_secs: f.duration_secs,
            chapters: f.chapter_count(),
        });
    }

    // Present titles in disc order, which is the ordering an episode set follows.
    inputs.sort_by(|a, b| a.name.cmp(&b.name));

    let analysis = video::analyse(&inputs, AnalysisParams::default());

    println!("  Disc shape: {}", analysis.shape);
    if let Some(c) = analysis.category {
        println!("  Category:   {} (proposed)", c.slug());
    } else {
        println!("  Category:   undetermined");
    }
    println!(
        "  Confidence: {}  (structural inference — always requires confirmation)",
        analysis.confidence
    );

    println!("  Titles:");
    for t in &analysis.titles {
        println!(
            "    {:<16} {:>9}  {:<8} {}",
            t.name.trim_start_matches("raw/"),
            t.duration_hms(),
            t.role.to_string(),
            t.why
        );
    }

    println!("  Reasoning:");
    for e in &analysis.evidence {
        println!("    - {e}");
    }

    // --- Online lookup ---
    let label = job
        .probe
        .as_ref()
        .and_then(|p| p.content.title_guess.clone())
        .unwrap_or_default();
    let query = show_override
        .map(str::to_string)
        .unwrap_or_else(|| dumo_identify::matching::query_from_label(&label));

    if query.trim().is_empty() {
        println!("  No search term: pass --show to name this disc's content.");
        return Ok(());
    }

    let client = match dumo_identify::tmdb::TmdbClient::from_config(&cfg.api) {
        Ok(c) => c,
        Err(e) => {
            println!("  Naming unavailable: {e}");
            return Ok(());
        }
    };

    println!();
    println!("  Searching TMDB for {query:?} ...");
    let shows = client.search_tv(&query).context("searching TMDB")?;
    if shows.is_empty() {
        println!("  No series matched. Try --show \"<title>\".");
        return Ok(());
    }
    // The disc number in the volume label is an independent ordering signal, and
    // runtimes tie often enough that it earns its keep.
    let disc_hint = dumo_identify::matching::disc_number_from_label(&label);
    if let Some(d) = disc_hint {
        println!("  Disc {d} of a set, per the volume label");
    }

    let mut titles: Vec<dumo_identify::matching::DiscTitle> = analysis
        .main_titles()
        .map(|t| dumo_identify::matching::DiscTitle {
            name: t.name.trim_start_matches("raw/").to_string(),
            duration_secs: t.duration_secs,
            dialogue: None,
        })
        .collect();
    if titles.is_empty() {
        println!("  No main titles to match.");
        return Ok(());
    }
    dumo_identify::matching::sort_by_title_index(&mut titles);

    // Judge candidates by how well their episodes actually fit these runtimes, not by
    // TMDB's search ranking. Searching "espn 30 for 30" puts a different, similarly
    // named series first; only the runtimes reveal which one is really on the disc.
    const MAX_CANDIDATES: usize = 4;
    let considered: Vec<_> = shows.iter().take(MAX_CANDIDATES).collect();
    println!(
        "  Considering {} candidate series by how well episode runtimes fit:",
        considered.len()
    );

    let mut evaluated: Vec<(&dumo_identify::tmdb::TvResult, Vec<dumo_identify::matching::SeasonMatch>)> =
        Vec::new();
    for c in &considered {
        let season_numbers = match client.tv_season_numbers(c.id) {
            Ok(n) => n,
            Err(e) => {
                println!("    {} — unavailable: {e}", c.name);
                continue;
            }
        };
        let mut seasons = Vec::new();
        for n in season_numbers.iter().filter(|n| **n > 0) {
            if let Ok(s) = client.season(c.id, *n) {
                seasons.push(s);
            }
        }
        let results = dumo_identify::matching::match_seasons(&titles, &seasons, disc_hint);
        match results.first() {
            Some(b) => {
                println!(
                    "    {:<34} {:>5.1} min/episode average difference",
                    truncate(&format!("{} ({})", c.name,
                        c.year().map(|y| y.to_string()).unwrap_or("?".into())), 34),
                    b.mean_delta
                );
                evaluated.push((c, results));
            }
            None => println!(
                "    {:<34} no season with {} or more episodes",
                truncate(&c.name, 34),
                titles.len()
            ),
        }
    }

    evaluated.sort_by(|a, b| {
        a.1[0]
            .mean_delta
            .partial_cmp(&b.1[0].mean_delta)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let Some((candidate, results)) = evaluated.first() else {
        println!("  No candidate series could be matched.");
        return Ok(());
    };
    let best = &results[0];

    println!();
    println!(
        "  Series: {} ({})  [tmdb:{}]",
        candidate.name,
        candidate.year().map(|y| y.to_string()).unwrap_or("?".into()),
        candidate.id
    );
    if let Some((runner, rres)) = evaluated.get(1) {
        let gap = rres[0].mean_delta - best.mean_delta;
        println!(
            "    chosen over {} by {:.1} min/episode",
            truncate(&runner.name, 40),
            gap
        );
    }

    println!();
    println!(
        "  Best match: season {}, episodes {}–{}",
        best.season,
        best.first_episode,
        best.first_episode + best.matches.len() as u32 - 1
    );
    for m in &best.matches {
        println!(
            "    {:<16} {:>5.0} min  ->  S{:02}E{:02} {:<44} (delta {:.0} min)",
            m.title_name,
            m.title_mins,
            m.episode.season,
            m.episode.number,
            truncate(&m.episode.name, 44),
            m.delta_mins
        );
    }
    println!(
        "  Confidence: {}  ({})",
        best.confidence,
        // "strong (needs confirmation)" reads as a contradiction without saying why. It
        // is not a hedge on the evidence: video identification is inference, and only
        // hash identity is ever filed unattended.
        if best.confidence.is_auto_acceptable() {
            "eligible for unattended acceptance"
        } else {
            "inferred — only exact hash matches are filed unattended"
        }
    );
    for e in &best.evidence {
        println!("    - {e}");
    }
    if best.disc_hint_used {
        println!("    - the disc number decided this, not the runtimes");
    }


    println!();
    println!("  Proposed names:");
    let year = candidate
        .year()
        .map(|y| format!(" ({y})"))
        .unwrap_or_default();
    for m in &best.matches {
        println!(
            "    tv/{}{}/Season {:02}/{}{} S{:02}E{:02} - {}.mkv",
            candidate.name,
            year,
            m.episode.season,
            candidate.name,
            year,
            m.episode.season,
            m.episode.number,
            sanitise(&m.episode.name)
        );
    }
    println!();
    println!("  Applying video names is not wired up yet — review the above first.");
    Ok(())
}

/// Strip characters that are awkward or illegal in filenames.
fn sanitise(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' => '-',
            ':' => ' ',
            '?' | '*' | '"' | '<' | '>' | '|' => ' ',
            other => other,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

/// Identify several video discs together, using the constraint that a box set holds
/// consecutive, non-overlapping episodes in disc order.
///
/// This is materially stronger than matching discs one at a time. Runtimes routinely
/// tie between adjacent windows; requiring the whole set to be consistent usually
/// leaves exactly one arrangement standing.
fn solve_video_set(dirs: &[PathBuf], cfg: &Config, show_override: Option<&str>) -> Result<()> {
    use dumo_identify::discset::{self, GroupInput};
    use dumo_identify::matching::{self, DiscTitle};
    use dumo_identify::video::{self as vid, AnalysisParams, TitleInput};

    let mut inputs: Vec<GroupInput> = Vec::new();

    for dir in dirs {
        let job = match Job::load(dir) {
            Ok(j) => j,
            Err(_) => continue,
        };
        let kind = job.probe.as_ref().map(|p| p.content.kind);
        if !matches!(kind, Some(MediaKind::DvdVideo) | Some(MediaKind::BluRayVideo)) {
            continue;
        }
        let label = job
            .probe
            .as_ref()
            .and_then(|p| p.content.title_guess.clone())
            .unwrap_or_default();

        let mut title_inputs = Vec::new();
        for a in job.artifacts.iter().filter(|a| {
            let p = a.relative_path.to_ascii_lowercase();
            p.ends_with(".mkv") || p.ends_with(".mp4") || p.ends_with(".m2ts")
        }) {
            let path = dir.join(&a.relative_path);
            if !path.is_file() {
                continue;
            }
            let f = dumo_backends::ffprobe::probe(&path)
                .with_context(|| format!("probing {}", path.display()))?;
            title_inputs.push(TitleInput {
                name: a.relative_path.clone(),
                duration_secs: f.duration_secs,
                chapters: f.chapter_count(),
            });
        }
        if title_inputs.is_empty() {
            continue;
        }

        let analysis = vid::analyse(&title_inputs, AnalysisParams::default());
        // Pull dialogue for each main title. This is what lets the solver do better
        // than runtime alone: two episodes of a series often run to the same minute, but
        // they do not say the same words. Extras have no subtitle track and score on
        // runtime as before.
        let mut titles: Vec<DiscTitle> = Vec::new();
        for t in analysis.main_titles() {
            let path = dir.join(&t.name);
            print!("  reading dialogue from {} ... ", t.name);
            std::io::stdout().flush().ok();
            let dialogue = read_dialogue(&path);
            match &dialogue {
                Some(w) => println!("{} words", w.len()),
                None => println!("no subtitle track"),
            }
            titles.push(DiscTitle {
                name: t.name.trim_start_matches("raw/").to_string(),
                duration_secs: t.duration_secs,
                dialogue,
            });
        }
        if titles.is_empty() {
            continue;
        }
        matching::sort_by_title_index(&mut titles);

        inputs.push(GroupInput {
            job_id: job.id.clone(),
            label,
            titles,
        });
    }

    if inputs.is_empty() {
        println!("No video jobs with main titles found.");
        return Ok(());
    }

    // Decide what a set is from the discs themselves, not from however the caller
    // happened to filter the job list. Grouping unrelated series together would turn a
    // selection accident into confident episode assignments.
    let groups = discset::group_into_sets(&inputs);
    println!("Grouped {} disc(s) into {} set(s):", inputs.len(), groups.len());
    for g in &groups {
        println!();
        println!("  Set {:?}", g.series_key);
        for d in &g.discs {
            println!(
                "    disc {:<3} {} main title(s)  {}",
                if d.disc_number == 0 {
                    "?".to_string()
                } else {
                    d.disc_number.to_string()
                },
                d.titles.len(),
                d.job_id
            );
        }
        for e in &g.evidence {
            println!("      - {e}");
        }
        for w in &g.warnings {
            println!("      ! {w}");
        }
        if !g.is_orderable() {
            println!(
                "      -> not solved as a set: {}",
                if g.discs.len() < 2 {
                    "a single disc gains nothing from the set constraint"
                } else {
                    "the ordering is not established, so the constraint would be a guess"
                }
            );
        }
    }

    let solvable: Vec<&discset::SetGroup> = groups.iter().filter(|g| g.is_orderable()).collect();
    if solvable.is_empty() {
        println!();
        println!("No set can be solved jointly. Identify these discs individually instead");
        println!("(drop --set), or correct the volume labels.");
        return Ok(());
    }
    if solvable.len() > 1 {
        println!();
        println!("Several independent sets found; solving each separately.");
    }

    for group in solvable {
        solve_one_set(group, cfg, show_override)?;
    }
    Ok(())
}

/// Solve a single, verified set against TMDB.
fn solve_one_set(
    group: &dumo_identify::discset::SetGroup,
    cfg: &Config,
    show_override: Option<&str>,
) -> Result<()> {
    use dumo_identify::discset;

    let discs = &group.discs;
    let query = show_override
        .map(str::to_string)
        .unwrap_or_else(|| group.series_key.clone());
    if query.trim().is_empty() {
        println!("No search term for this set; pass --show.");
        return Ok(());
    }

    let client = match dumo_identify::tmdb::TmdbClient::from_config(&cfg.api) {
        Ok(c) => c,
        Err(e) => {
            println!("Naming unavailable: {e}");
            return Ok(());
        }
    };

    println!();
    println!("Searching TMDB for {query:?} ...");
    let shows = client.search_tv(&query).context("searching TMDB")?;
    if shows.is_empty() {
        println!("No series matched. Try --show \"<title>\".");
        return Ok(());
    }

    // Rank candidate series by how well the whole set fits, not by search position.
    let mut best: Option<(&dumo_identify::tmdb::TvResult, discset::SetSolution)> = None;
    for c in shows.iter().take(4) {
        let Ok(nums) = client.tv_season_numbers(c.id) else {
            continue;
        };
        let mut seasons = Vec::new();
        for n in nums.iter().filter(|n| **n > 0) {
            if let Ok(s) = client.season(c.id, *n) {
                seasons.push(s);
            }
        }
        if let Some(sol) = discset::solve(&discs, &seasons) {
            println!(
                "  {:<34} score {:>5.3}   runtimes {:>4.1} min/title out",
                truncate(&format!("{} ({})", c.name,
                    c.year().map(|y| y.to_string()).unwrap_or("?".into())), 34),
                1.0 - sol.mean_cost,
                sol.mean_delta
            );
            // Rank on the combined signal, not runtime alone: a series whose episode
            // lengths happen to line up should not beat one whose dialogue matches.
            if best.as_ref().map(|(_, b)| sol.mean_cost < b.mean_cost).unwrap_or(true) {
                best = Some((c, sol));
            }
        }
    }

    let Some((series, solution)) = best else {
        println!("No candidate series could hold this set.");
        return Ok(());
    };

    let year = series.year().map(|y| format!(" ({y})")).unwrap_or_default();
    println!();
    println!("Series: {}{}  [tmdb:{}]", series.name, year, series.id);
    println!("Season: {}", solution.season);
    println!(
        "Confidence: {}  ({})",
        solution.confidence,
        if solution.confidence.is_auto_acceptable() {
            "eligible for unattended acceptance"
        } else {
            "inferred — only exact hash matches are filed unattended"
        }
    );
    for e in &solution.evidence {
        println!("  - {e}");
    }

    // Show the signals separately. Where they disagree, that disagreement is the most
    // useful thing on screen — it is what a reviewer should be looking at.
    println!();
    println!(
        "  {:<18} {:>8} {:>9}   {:<7} {:<8}",
        "title", "runtime", "dialogue", "rt pick", "dlg pick"
    );
    for v in &solution.verdicts {
        println!(
            "  {:<18} {:>8.2} {:>9}   E{:02}     {:<8} {}",
            truncate(&v.title_name, 18),
            v.scores.runtime,
            v.scores
                .subtitle
                .map(|s| format!("{s:.2}"))
                .unwrap_or_else(|| "-".into()),
            v.runtime_pick,
            v.subtitle_pick
                .map(|p| format!("E{p:02}"))
                .unwrap_or_else(|| "-".into()),
            match (v.subtitle_confirms(), v.runtime_confirms()) {
                (true, true) => "both confirm",
                (true, false) => "dialogue confirms",
                (false, true) => "runtime confirms",
                (false, false) => "",
            }
        );
    }

    println!();
    for p in &solution.placements {
        println!("Disc {} ({})", p.disc_number, p.job_id);
        for m in &p.matches {
            println!(
                "  {:<16} {:>5.0} min  ->  S{:02}E{:02} {:<40} (delta {:.0} min)",
                m.title_name,
                m.title_mins,
                m.episode.season,
                m.episode.number,
                truncate(&m.episode.name, 40),
                m.delta_mins
            );
        }
        for m in &p.matches {
            println!(
                "    tv/{}{}/Season {:02}/{}{} S{:02}E{:02} - {}.mkv",
                series.name, year, m.episode.season,
                series.name, year, m.episode.season, m.episode.number,
                sanitise(&m.episode.name)
            );
        }
        println!();
    }
    println!("Applying video names is not wired up yet — review the above first.");
    Ok(())
}
