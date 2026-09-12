//! The `rip` command: Stage 2, disc to staging.
//!
//! Conservative by construction:
//!
//! - Pre-flight space check refuses to start rather than filling the disk.
//! - A fresh job directory per rip; never reuses or overwrites one.
//! - Every artifact is hashed after writing, and the hashes are recorded in the job
//!   manifest. Nothing later in the pipeline may delete a source without them.
//! - The disc is never modified, and nothing outside the staging root is touched.

use anyhow::{bail, Context, Result};
use dumo_backends::{makemkv, redumper};
use dumo_core::config::{self, Config};
use dumo_core::job::{self, Artifact, Job, JobStage};
use dumo_core::{hash, MediaKind};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Titles shorter than this are skipped by default: on a DVD they are menus, logos, and
/// transition stingers rather than content.
const DEFAULT_MIN_LENGTH_SECS: u32 = 300;

pub struct RipArgs {
    pub device: Option<String>,
    pub title: Option<u32>,
    pub min_length: u32,
    pub dry_run: bool,
    pub assume_yes: bool,
    pub config_file: Option<PathBuf>,
}

impl Default for RipArgs {
    fn default() -> Self {
        Self {
            device: None,
            title: None,
            min_length: DEFAULT_MIN_LENGTH_SECS,
            dry_run: false,
            assume_yes: false,
            config_file: None,
        }
    }
}

/// Rip the disc in a drive. Returns the job id, so a caller driving the whole pipeline
/// knows which job to carry forward.
pub fn run(args: RipArgs) -> Result<Option<String>> {
    let cfg = Config::load(args.config_file.as_deref())?;

    // Configuration must be sound before we touch a disc.
    let checks = config::check(&cfg);
    let errors: Vec<_> = checks
        .iter()
        .filter(|c| c.level == config::CheckLevel::Error)
        .collect();
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("config error: {} — {}", e.subject, e.detail);
        }
        bail!("configuration is not usable; run 'dump-o-matic config check'");
    }

    let device = match args.device.clone() {
        Some(d) => d,
        None => dumo_drives::enumerate_drives()
            .context("enumerating drives")?
            .first()
            .map(|d| d.path.clone())
            .context("no optical drives found")?,
    };

    // --- Stage 1: probe, so the job records what the disc looked like going in ---
    println!("Probing {device} ...");
    let probe = dumo_drives::probe_disc(&device).with_context(|| format!("probing {device}"))?;
    println!(
        "  {} — {}",
        probe.content.kind,
        probe.content.title_guess.as_deref().unwrap_or("no title")
    );

    // Route to the backend that suits the medium.
    match probe.content.kind {
        MediaKind::DvdVideo | MediaKind::BluRayVideo => {}
        MediaKind::GameDisc | MediaKind::Data => {
            return rip_game(&cfg, &device, &probe, &args);
        }
        // Refused rather than attempted: redumper supports Xbox discs, but only through
        // a drive whose firmware exposes the security sector. Ripping what this drive
        // can see would yield the warning clip under a game's name.
        MediaKind::Xbox360GameDisc => bail!(
            "this is an Xbox 360 disc and only its video partition is readable here. The \
             game is outside the addressable area, so any dump from this drive would \
             contain the warning clip, not the game. Dumping it needs a Kreon-firmware \
             drive (TSSTcorp SH-D162C/D163A/D163B)."
        ),
        MediaKind::XboxGameDisc => bail!(
            "this is an original Xbox disc and only its video partition is readable here. \
             The game is in an XDVDFS partition outside the addressable area, so any dump \
             from this drive would contain the warning clip, not the game. Dumping it needs \
             a Kreon-firmware drive (TSSTcorp SH-D162C/D163A/D163B) or a dump taken from a \
             softmodded console."
        ),
        other => bail!(
            "no backend for {other} yet; audio CD support is not implemented. \
             Video discs use MakeMKV and game/data discs use redumper."
        ),
    }

    // --- Scan titles ---
    let backend_version = makemkv::version().context("checking makemkvcon")?;
    println!("Backend: {backend_version}");

    let disc_index = makemkv::find_disc_index(&device)
        .with_context(|| format!("locating {device} in MakeMKV"))?;

    println!("Scanning titles (this reads the whole disc structure) ...");
    let scan = makemkv::scan(disc_index, args.min_length).context("scanning disc")?;

    if scan.titles.is_empty() {
        bail!(
            "no titles at least {}s long; lower --min-length to include shorter titles",
            args.min_length
        );
    }

    let selected: Vec<u32> = match args.title {
        Some(t) => {
            if !scan.titles.iter().any(|x| x.index == t) {
                bail!("no title with index {t}");
            }
            vec![t]
        }
        None => scan.titles.iter().map(|t| t.index).collect(),
    };

    println!();
    println!("Titles found:");
    for t in &scan.titles {
        let mark = if selected.contains(&t.index) { "*" } else { " " };
        println!(
            "  {mark} {:>2}. {:>9}  {:>8}  {} chapter(s)  -> {}",
            t.index,
            t.duration_hms(),
            human_size(t.estimated_bytes),
            t.chapters,
            t.output_name
        );
    }

    // --- Build the job ---
    let label = probe
        .content
        .title_guess
        .clone()
        .or_else(|| scan.disc_name.clone());
    let job_id = job::new_job_id(label.as_deref());
    let job_dir = cfg.staging.job_dir(&job_id);
    let raw_dir = cfg.staging.job_raw_dir(&job_id);

    let mut job = Job::new(job_id.clone(), device.clone());
    job.probe = Some(probe.clone());
    job.titles = scan.titles.clone();
    job.selected_titles = selected.clone();
    job.backend = Some(backend_version.clone());

    let estimated = job.estimated_bytes();

    // --- Pre-flight space check ---
    println!();
    println!("Job:       {job_id}");
    println!("Staging:   {}", raw_dir.display());
    println!("Estimated: {} for {} title(s)", human_size(estimated), selected.len());

    match config::filesystem_free_bytes(&cfg.staging.root) {
        Some((free, _total)) => {
            let headroom = cfg.staging.min_free_headroom_gb * 1_000_000_000;
            let required = estimated.saturating_add(headroom);
            println!(
                "Free:      {} (need {} incl. {} GB headroom)",
                human_size(free),
                human_size(required),
                cfg.staging.min_free_headroom_gb
            );
            if free < required {
                bail!(
                    "not enough space: {} free, need {}. Free space or lower \
                     staging.min_free_headroom_gb.",
                    human_size(free),
                    human_size(required)
                );
            }
        }
        None => {
            // Unknown is not the same as sufficient; require an explicit override.
            if !args.assume_yes {
                bail!(
                    "cannot determine free space on {}; re-run with --yes to proceed anyway",
                    cfg.staging.root.display()
                );
            }
            println!("Free:      unknown (proceeding because --yes was given)");
        }
    }

    if args.dry_run {
        println!();
        println!("Dry run: would create {} and rip {} title(s).", job_dir.display(), selected.len());
        println!("Nothing was written.");
        return Ok(None);
    }

    if !args.assume_yes && !confirm("Proceed with rip?")? {
        println!("Aborted; nothing was written.");
        return Ok(None);
    }

    // --- Rip ---
    Job::create_dir(&job_dir)?;
    std::fs::create_dir_all(&raw_dir)?;
    std::fs::create_dir_all(cfg.staging.logs_dir())?;

    job.stage = JobStage::Ripping;
    job.save(&job_dir)?;

    let log_path = cfg.staging.logs_dir().join(format!("{job_id}.log"));
    let mut log = std::fs::File::create(&log_path)?;

    let started = std::time::Instant::now();
    let mut last_render = std::time::Instant::now();

    let rip_result = (|| -> Result<()> {
        for (n, title) in selected.iter().enumerate() {
            println!();
            println!("Ripping title {title} ({} of {}) ...", n + 1, selected.len());
            makemkv::rip(
                disc_index,
                &[*title],
                &raw_dir,
                args.min_length,
                |p| {
                    // Throttle redraws; MakeMKV emits progress very frequently.
                    if last_render.elapsed().as_millis() < 250 {
                        return;
                    }
                    last_render = std::time::Instant::now();
                    match p.fraction {
                        Some(f) => {
                            print!("\r  {:<40} {:>5.1}%", truncate(&p.operation, 40), f * 100.0)
                        }
                        None => print!("\r  {:<40}       ", truncate(&p.operation, 40)),
                    }
                    let _ = std::io::stdout().flush();
                },
                |line| {
                    let _ = writeln!(log, "{line}");
                },
            )?;
            println!("\r  done{:<45}", "");
        }
        Ok(())
    })();

    if let Err(e) = rip_result {
        job.stage = JobStage::Failed;
        job.error = Some(e.to_string());
        job.save(&job_dir)?;
        eprintln!();
        eprintln!("Rip failed: {e}");
        eprintln!("Partial output left in {} for inspection.", raw_dir.display());
        eprintln!("Backend log: {}", log_path.display());
        eprintln!("Nothing was deleted.");
        return Err(e);
    }

    // --- Hash every artifact ---
    println!();
    println!("Hashing artifacts ...");
    let mut artifacts = Vec::new();
    for entry in std::fs::read_dir(&raw_dir)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        print!("  {name} ... ");
        let _ = std::io::stdout().flush();

        let (sha256, bytes) = hash::sha256_file(&path)
            .with_context(|| format!("hashing {}", path.display()))?;
        println!("{}  {}", human_size(bytes), &sha256[..16]);

        artifacts.push(Artifact {
            relative_path: relative_to(&path, &job_dir),
            bytes,
            sha256,
            hashed_at: unix_now(),
        });
    }

    if artifacts.is_empty() {
        job.stage = JobStage::Failed;
        job.error = Some("backend reported success but produced no files".into());
        job.save(&job_dir)?;
        bail!("backend reported success but produced no output files");
    }

    job.artifacts = artifacts;
    job.stage = JobStage::Ripped;
    job.save(&job_dir)?;

    println!();
    println!(
        "Ripped {} file(s), {} in {}.",
        job.artifacts.len(),
        human_size(job.total_artifact_bytes()),
        fmt_elapsed(started.elapsed())
    );
    println!("Job:      {}", job_dir.display());
    println!("Manifest: {}", job_dir.join(job::MANIFEST_NAME).display());
    println!();
    println!("The disc has not been modified and nothing has been deleted.");
    println!("Next: dump-o-matic identify {job_id}");
    println!("      or --set with the other discs of a box set, to solve them together.");

    Ok(Some(job_id))
}

/// Rip a game or data disc with redumper, producing an archival image.
fn rip_game(
    cfg: &Config,
    device: &str,
    probe: &dumo_core::DiscProbe,
    args: &RipArgs,
) -> Result<Option<String>> {
    let backend_version = dumo_backends::redumper::version()
        .context("checking redumper (install it from https://github.com/superg/redumper)")?;
    println!("Backend: {backend_version}");

    if let Some(g) = &probe.game_serial {
        println!("Serial:  {} ({})", g.serial, g.platform);
    } else {
        println!("Serial:  none found — this will be dumped as a generic data disc");
    }

    // The medium's own sector count is the size to plan for; a disc image is the whole
    // disc, unlike a video rip where only selected titles are copied.
    let estimated = probe.capacity_bytes.unwrap_or(0);

    let label = probe
        .game_serial
        .as_ref()
        .map(|g| format!("{}-{}", g.platform, g.serial))
        .or_else(|| probe.content.title_guess.clone());
    let job_id = job::new_job_id(label.as_deref());
    let job_dir = cfg.staging.job_dir(&job_id);
    let raw_dir = cfg.staging.job_raw_dir(&job_id);

    let mut job = Job::new(job_id.clone(), device.to_string());
    job.probe = Some(probe.clone());
    job.backend = Some(backend_version.clone());

    println!();
    println!("Job:       {job_id}");
    println!("Staging:   {}", raw_dir.display());
    println!("Estimated: {} (full disc image)", human_size(estimated));

    // redumper writes the image plus state/log sidecars; leave room for them.
    let overhead = estimated / 10;
    preflight_space(cfg, estimated.saturating_add(overhead), args.assume_yes)?;

    if args.dry_run {
        println!();
        println!(
            "Dry run: would create {} and dump the full disc.",
            job_dir.display()
        );
        println!("Nothing was written.");
        return Ok(None);
    }

    if !args.assume_yes && !confirm("Proceed with dump?")? {
        println!("Aborted; nothing was written.");
        return Ok(None);
    }

    Job::create_dir(&job_dir)?;
    std::fs::create_dir_all(&raw_dir)?;
    std::fs::create_dir_all(cfg.staging.logs_dir())?;

    job.stage = JobStage::Ripping;
    job.save(&job_dir)?;

    let log_path = cfg.staging.logs_dir().join(format!("{job_id}.log"));
    let mut log = std::fs::File::create(&log_path)?;

    // Name the image after the serial when we have one, so the raw output is already
    // meaningful; stage 3 still renames it to the full Redump convention.
    let image_name = probe
        .game_serial
        .as_ref()
        .map(|g| g.serial.clone())
        .unwrap_or_else(|| "disc".to_string());

    let started = std::time::Instant::now();
    let mut last_render = std::time::Instant::now();

    println!();
    println!("Dumping (this reads every sector; errors are retried) ...");

    let result = dumo_backends::redumper::dump(
        device,
        &raw_dir,
        &image_name,
        // No override configured: let redumper measure the sector order rather than
        // assume it. Assuming is what made every sector of a CD fail.
        None,
        |p: &dumo_backends::redumper::DumpProgress| {
            if last_render.elapsed().as_millis() < 250 {
                return;
            }
            last_render = std::time::Instant::now();
            print!(
                "\r  {:>3}%  LBA {}/{}  errors: SCSI {} EDC {}   ",
                p.percent, p.current_lba, p.total_lba, p.scsi_errors, p.edc_errors
            );
            let _ = std::io::stdout().flush();
        },
        |line: &str| {
            let _ = writeln!(log, "{line}");
        },
        // Warnings are shown as they happen. Held to the end they are useless: an
        // unknown drive decides whether the dump can work at all.
        |w: &str| {
            println!("\r  {w:<70}");
        },
    );

    let outcome = match result {
        Ok(o) => o,
        Err(e) => {
            job.stage = JobStage::Failed;
            job.error = Some(e.to_string());
            job.save(&job_dir)?;
            eprintln!();
            eprintln!("Dump failed: {e}");
            eprintln!("Partial output left in {} for inspection.", raw_dir.display());
            eprintln!("Log: {}", log_path.display());
            eprintln!("Nothing was deleted.");
            return Err(e.into());
        }
    };
    println!("\r  done{:<50}", "");

    // --- Integrity gates -------------------------------------------------------
    // redumper exits successfully even when sectors failed to read, so a clean exit is
    // not sufficient evidence of an archival dump.
    let mut problems: Vec<String> = Vec::new();

    // What the image *should* measure. The shape (one .iso versus tiled .bin tracks)
    // follows from what redumper produced; the expected length comes from the TOC this
    // tool read off the drive itself, so the two are independent of each other.
    let expected_geometry = if outcome
        .files
        .iter()
        .any(|p| p.extension().map(|e| e == "cue").unwrap_or(false))
    {
        redumper::ExpectedGeometry::CdTracks {
            leadout_lba: probe.toc.as_ref().map(|t| u64::from(t.leadout_lba)).unwrap_or(0),
        }
    } else {
        redumper::ExpectedGeometry::Iso
    };

    match &outcome.state {
        Some(s) if !s.is_complete() => {
            let runs = s.interior_runs();
            problems.push(format!(
                "{} unreadable {}(s) inside the data, in {} run(s); first at {}",
                s.interior_bad(),
                s.unit_noun(),
                runs.len(),
                runs.first().map(|r| r.0).unwrap_or(0)
            ));
        }
        Some(_) => {}
        None => problems.push("no .state file; cannot verify every sector was read".into()),
    }
    if let Err(e) = redumper::verify_image_size(&outcome, expected_geometry) {
        problems.push(e);
    }

    for w in &outcome.warnings {
        println!("  note: {w}");
    }

    // Corrections are recovered errors, not damage. Report them so a flaky disc is
    // visible, but never fail the dump on them.
    if outcome.corrections_scsi > 0 || outcome.corrections_edc > 0 {
        println!(
            "  note: redumper corrected {} SCSI and {} EDC read error(s) by re-reading; \
             all sectors were ultimately recovered",
            outcome.corrections_scsi, outcome.corrections_edc
        );
    }
    if let Some(s) = &outcome.state {
        println!(
            "  {} of {} {}s read successfully",
            s.total_sectors - s.bad_sectors(),
            s.total_sectors,
            s.unit_noun()
        );
        // On a CD the lead-in and lead-out sit outside the emitted tracks and are often
        // unreachable. Say so plainly, so a clean dump does not look alarming.
        if s.edge_bad() > 0 && s.interior_bad() == 0 {
            println!(
                "  {} unread {}(s) are all in the lead-in/lead-out, outside the tracks; \
                 the track data is complete",
                s.edge_bad(),
                s.unit_noun()
            );
        }
    }
    // Record how the drive was configured: on raw CD reads the dump's correctness
    // depends on it entirely.
    if let Some(order) = &outcome.drive.sector_order {
        println!(
            "  drive sector order: {order}{}{}",
            if outcome.drive.auto_detected {
                " (measured)"
            } else {
                " (assumed)"
            },
            if outcome.drive.generic {
                ", drive not in redumper's database"
            } else {
                ""
            }
        );
    }
    let m = &outcome.metadata;
    if m.serial.is_some() || m.region.is_some() {
        println!(
            "  disc metadata: serial {}, region {}, version {}",
            m.serial.as_deref().unwrap_or("?"),
            m.region.as_deref().unwrap_or("?"),
            m.version.as_deref().unwrap_or("?")
        );
    }

    // --- Hash every produced file ---
    println!();
    println!("Hashing artifacts ...");
    let mut artifacts = Vec::new();
    for path in &outcome.files {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        print!("  {name} ... ");
        let _ = std::io::stdout().flush();
        let (sha256, bytes) =
            hash::sha256_file(path).with_context(|| format!("hashing {}", path.display()))?;
        println!("{}  {}", human_size(bytes), &sha256[..16]);
        artifacts.push(Artifact {
            relative_path: relative_to(path, &job_dir),
            bytes,
            sha256,
            hashed_at: unix_now(),
        });
    }
    job.artifacts = artifacts;

    if !outcome.hashes.is_empty() {
        println!();
        println!("Redump-comparable hashes:");
        for h in &outcome.hashes {
            println!("  {}", h.name);
            println!("    size  {}", h.size);
            println!("    crc32 {}", h.crc32);
            println!("    md5   {}", h.md5);
            println!("    sha1  {}", h.sha1);
        }
    }

    if problems.is_empty() {
        job.stage = JobStage::Ripped;
        job.save(&job_dir)?;
        println!();
        println!(
            "Dumped {} file(s), {} in {}.",
            job.artifacts.len(),
            human_size(job.total_artifact_bytes()),
            fmt_elapsed(started.elapsed())
        );
        println!("Dump is clean: every sector read successfully and the image length");
        println!("matches the medium exactly.");
    } else {
        // Keep everything, but do not let an unclean dump pass as verified.
        job.stage = JobStage::Failed;
        job.error = Some(problems.join("; "));
        job.save(&job_dir)?;
        println!();
        println!("Dump completed but did NOT pass integrity checks:");
        for p in &problems {
            println!("  - {p}");
        }
        println!();
        println!("The image is kept at {} for inspection, and the job is", raw_dir.display());
        println!("marked failed so nothing downstream treats it as archival.");
        println!("Consider cleaning the disc and re-running, or 'redumper refine' to retry sectors.");
    }

    println!();
    println!("Job:      {}", job_dir.display());
    println!("Manifest: {}", job_dir.join(job::MANIFEST_NAME).display());
    println!("Log:      {}", log_path.display());
    println!("The disc has not been modified and nothing has been deleted.");

    if problems.is_empty() {
        println!();
        println!("Next: dump-o-matic identify {job_id}");
    }
    Ok(Some(job_id))
}

/// Shared pre-flight space check.
fn preflight_space(cfg: &Config, required_content: u64, assume_yes: bool) -> Result<()> {
    match config::filesystem_free_bytes(&cfg.staging.root) {
        Some((free, _)) => {
            let headroom = cfg.staging.min_free_headroom_gb * 1_000_000_000;
            let required = required_content.saturating_add(headroom);
            println!(
                "Free:      {} (need {} incl. {} GB headroom)",
                human_size(free),
                human_size(required),
                cfg.staging.min_free_headroom_gb
            );
            if free < required {
                bail!(
                    "not enough space: {} free, need {}",
                    human_size(free),
                    human_size(required)
                );
            }
            Ok(())
        }
        None => {
            if !assume_yes {
                bail!(
                    "cannot determine free space on {}; re-run with --yes to proceed anyway",
                    cfg.staging.root.display()
                );
            }
            println!("Free:      unknown (proceeding because --yes was given)");
            Ok(())
        }
    }
}

fn relative_to(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn fmt_elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

pub fn human_size(bytes: u64) -> String {
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
    Ok(matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}
