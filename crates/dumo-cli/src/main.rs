//! `dump-o-matic` command-line interface.
//!
//! Currently implements the read-only half of the pipeline: drive discovery and Stage 1
//! disc probing. No command in this binary writes to a disc or to your filesystem.

mod rip;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dumo_core::config::{self, CheckLevel};
use dumo_core::{Config, DiscProbe, MediaKind};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dump-o-matic",
    version,
    about = "Staged media ripping pipeline (read-only commands implemented so far)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List optical drives and their current state.
    Drives {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Probe the disc in a drive and report what it appears to be.
    ///
    /// Fast and strictly read-only: reads a handful of sectors, never the whole disc.
    Probe {
        /// Device to probe. Defaults to the first drive found.
        device: Option<String>,
        /// Emit JSON instead of a report.
        #[arg(long)]
        json: bool,
    },
    /// Rip a disc into the staging area (stage 2).
    ///
    /// Writes only inside the configured staging root, never modifies the disc, and
    /// never deletes anything.
    Rip {
        /// Device to rip. Defaults to the first drive found.
        device: Option<String>,
        /// Rip only this title index. Default: every title long enough to qualify.
        #[arg(long)]
        title: Option<u32>,
        /// Ignore titles shorter than this many seconds.
        #[arg(long, default_value_t = 300)]
        min_length: u32,
        /// Show what would happen without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Do not prompt for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Config file to use instead of the search path.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Inspect and validate configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// List jobs in the staging area.
    Jobs {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Validate the configuration against the filesystem.
    Check {
        /// Config file to use instead of the search path.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Print the effective configuration, with secrets redacted.
    Show {
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Print a commented starter config to stdout.
    Example,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Drives { json } => cmd_drives(json),
        Command::Probe { device, json } => cmd_probe(device, json),
        Command::Config { action } => cmd_config(action),
        Command::Rip {
            device,
            title,
            min_length,
            dry_run,
            yes,
            config,
        } => rip::run(rip::RipArgs {
            device,
            title,
            min_length,
            dry_run,
            assume_yes: yes,
            config_file: config,
        }),
        Command::Jobs { config, json } => cmd_jobs(config, json),
    }
}

fn cmd_jobs(config_file: Option<PathBuf>, json: bool) -> Result<()> {
    let cfg = Config::load(config_file.as_deref())?;
    let jobs_dir = cfg.staging.jobs_dir();

    let mut jobs = Vec::new();
    if jobs_dir.is_dir() {
        let mut dirs: Vec<_> = std::fs::read_dir(&jobs_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for d in dirs {
            match dumo_core::Job::load(&d) {
                Ok(j) => jobs.push(j),
                Err(e) => eprintln!("warning: skipping {}: {e}", d.display()),
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&jobs)?);
        return Ok(());
    }

    if jobs.is_empty() {
        println!("No jobs in {}", jobs_dir.display());
        return Ok(());
    }

    for j in &jobs {
        println!(
            "{:<40} {:<10} {:>3} file(s)  {}",
            j.id,
            j.stage.to_string(),
            j.artifacts.len(),
            rip::human_size(j.total_artifact_bytes())
        );
        if let Some(e) = &j.error {
            println!("    error: {e}");
        }
    }
    Ok(())
}

const EXAMPLE_CONFIG: &str = include_str!("../../../docs/config.example.toml");

fn cmd_config(action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Example => {
            print!("{EXAMPLE_CONFIG}");
            Ok(())
        }
        ConfigAction::Show { file } => {
            let cfg = Config::load(file.as_deref())?;
            if let Some(p) = &cfg.source_path {
                println!("# loaded from {}", p.display());
            }
            // Always print the redacted form; secrets must never reach stdout or a log.
            println!("{}", toml::to_string_pretty(&cfg.redacted())?);
            Ok(())
        }
        ConfigAction::Check { file } => {
            let cfg = Config::load(file.as_deref())?;
            if let Some(p) = &cfg.source_path {
                println!("Config: {}", p.display());
                println!();
            }

            let results = config::check(&cfg);
            let mut errors = 0;
            let mut warnings = 0;
            for r in &results {
                let tag = match r.level {
                    CheckLevel::Ok => "ok   ",
                    CheckLevel::Warning => {
                        warnings += 1;
                        "warn "
                    }
                    CheckLevel::Error => {
                        errors += 1;
                        "ERROR"
                    }
                };
                println!("[{tag}] {:<24} {}", r.subject, r.detail);
            }

            println!();
            if errors > 0 {
                println!("{errors} error(s), {warnings} warning(s) — not ready to run.");
                std::process::exit(1);
            }
            println!("No errors, {warnings} warning(s).");
            Ok(())
        }
    }
}

fn cmd_drives(json: bool) -> Result<()> {
    let drives = dumo_drives::enumerate_drives().context("enumerating optical drives")?;

    if drives.is_empty() {
        if json {
            println!("[]");
        } else {
            eprintln!("No optical drives found.");
            eprintln!("If running in a container, pass the device through: --device /dev/sr0");
        }
        return Ok(());
    }

    let mut rows = Vec::new();
    for d in &drives {
        let tray = dumo_drives::open_drive_status(d)
            .map(|s| s.tray.to_string())
            .unwrap_or_else(|e| format!("error: {e}"));
        rows.push((d.clone(), tray));
    }

    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|(d, tray)| {
                serde_json::json!({
                    "path": d.path,
                    "name": d.name,
                    "description": d.description(),
                    "revision": d.revision,
                    "capabilities": d.capabilities,
                    "tray": tray,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    for (d, tray) in &rows {
        let c = &d.capabilities;
        let mut reads = Vec::new();
        if c.reads_cd {
            reads.push("CD");
        }
        if c.reads_dvd {
            reads.push("DVD");
        }
        if c.reads_bluray {
            reads.push("BD");
        }
        println!("{}  {}", d.path, d.description());
        println!("    state:  {tray}");
        println!("    reads:  {}", reads.join(", "));
        if let Some(rev) = &d.revision {
            println!("    firmware: {rev}");
        }
    }
    Ok(())
}

fn cmd_probe(device: Option<String>, json: bool) -> Result<()> {
    let device = match device {
        Some(d) => d,
        None => {
            let drives = dumo_drives::enumerate_drives().context("enumerating optical drives")?;
            drives
                .first()
                .map(|d| d.path.clone())
                .context("no optical drives found; pass a device path explicitly")?
        }
    };

    let probe = dumo_drives::probe_disc(&device).with_context(|| format!("probing {device}"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&probe)?);
    } else {
        print_probe(&probe);
    }
    Ok(())
}

fn human_size(bytes: u64) -> String {
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else {
        format!("{:.1} MB", b / MB)
    }
}

fn fmt_duration(secs: f64) -> String {
    let total = secs.round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

fn print_probe(p: &DiscProbe) {
    println!("Device:     {}", p.device);
    if let Some(profile) = p.profile {
        println!("Medium:     {} ({})", profile, profile.family());
    }
    if let Some(cap) = p.capacity_bytes {
        println!("Capacity:   {}", human_size(cap));
    }

    println!();
    println!("Detected:   {}", p.content.kind);
    if let Some(t) = &p.content.title_guess {
        println!("Title guess: {t}");
    }
    println!(
        "Confidence: {}{}",
        p.content.confidence,
        if p.content.confidence.is_auto_acceptable() {
            ""
        } else {
            "  (needs confirmation)"
        }
    );
    if !p.content.evidence.is_empty() {
        println!("Evidence:");
        for e in &p.content.evidence {
            println!("  - {e}");
        }
    }

    if let Some(v) = &p.volume {
        println!();
        println!("Volume:");
        if let Some(x) = &v.volume_id {
            println!("  label:       {x}");
        }
        if let Some(x) = &v.publisher_id {
            println!("  publisher:   {x}");
        }
        if let Some(x) = &v.application_id {
            println!("  application: {x}");
        }
        if let Some(x) = &v.created {
            println!("  created:     {x}");
        }
        if let (Some(size), Some(bs)) = (v.volume_space_size, v.logical_block_size) {
            println!(
                "  size:        {} sectors x {} bytes = {}",
                size,
                bs,
                human_size(u64::from(size) * u64::from(bs))
            );
        }
    }

    if let Some(g) = &p.game_serial {
        println!();
        println!("Game serial: {}  (platform: {})", g.serial, g.platform);
        println!("  evidence:  {}", g.evidence);
    }

    if let Some(t) = &p.toc {
        println!();
        println!(
            "Table of contents: tracks {}-{}, total {}",
            t.first_track,
            t.last_track,
            fmt_duration(t.total_duration_secs())
        );
        for tr in &t.tracks {
            let dur = tr
                .duration_secs()
                .map(fmt_duration)
                .unwrap_or_else(|| "?".into());
            println!(
                "  {:>2}. {:>8}  {}",
                tr.number,
                dur,
                if tr.is_data { "data" } else { "audio" }
            );
        }
        if let Some(id) = &t.musicbrainz_discid {
            println!("  MusicBrainz disc ID: {id}");
        }
        if let Some(id) = &t.freedb_discid {
            println!("  FreeDB disc ID:      {id}");
        }
    }

    if !p.root_entries.is_empty() {
        println!();
        println!("Root directory ({} entries):", p.root_entries.len());
        for e in p.root_entries.iter().take(24) {
            println!("  {e}");
        }
        if p.root_entries.len() > 24 {
            println!("  ... {} more", p.root_entries.len() - 24);
        }
    }

    if p.content.kind == MediaKind::Unknown {
        println!();
        println!("Nothing recognised. The disc may be blank, damaged, or use a filesystem");
        println!("this probe does not read yet.");
    }

    println!();
    println!("Probed in {} ms", p.probe_millis);
}
