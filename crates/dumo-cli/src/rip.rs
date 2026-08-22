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
use dumo_backends::makemkv;
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

pub fn run(args: RipArgs) -> Result<()> {
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

    let device = match args.device {
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

    match probe.content.kind {
        MediaKind::DvdVideo | MediaKind::BluRayVideo => {}
        other => bail!(
            "this command currently handles DVD-Video and Blu-ray only; disc looks like {other}. \
             Audio CD and game disc backends are not implemented yet."
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
        return Ok(());
    }

    if !args.assume_yes && !confirm("Proceed with rip?")? {
        println!("Aborted; nothing was written.");
        return Ok(());
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
    println!("Next: identification (stage 3) is not implemented yet.");

    Ok(())
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
