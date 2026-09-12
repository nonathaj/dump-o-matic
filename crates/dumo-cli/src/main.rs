//! `dump-o-matic` command-line interface.
//!
//! The four pipeline stages, plus the tools to inspect them: probe a disc (1), rip it
//! into staging (2), identify and package what came off it (3), and migrate it to
//! permanent storage (4).
//!
//! Help text is part of the safety story here, not decoration. Several commands write,
//! and `migrate` can remove files — so each command's help says plainly what it can
//! destroy, and the ones that cannot say that too. Keep those statements true when
//! changing behaviour.

mod adopt;
mod clean;
mod devices;
mod identify;
mod migrate;
mod repack;
mod rip;
mod run;
mod verify;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dumo_core::config::{self, CheckLevel};
use dumo_core::{Config, DiscProbe, MediaKind};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dump-o-matic",
    version,
    about = "Staged media ripping pipeline: probe, rip, identify, migrate",
    long_about = "\
Staged media ripping pipeline for discs: probe (1), rip to staging (2), identify and
package (3), migrate to permanent storage (4).

Most commands only read. Those that write say so in their own help. Only `migrate` ever
removes anything, and only after the replacement copy has been written to its destination
and verified by reading it back.

Content is never deleted to make room, and an existing file is never overwritten."
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
    /// List attached storage devices that may hold console content.
    ///
    /// The counterpart of `drives` for media that is a filesystem rather than a disc: USB
    /// drives and memory cards a console has written to. Read-only; devices are opened
    /// read-only and never mounted.
    Devices {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// List the content on a storage device.
    ///
    /// The counterpart of `probe`: says what a device holds and what of it can be pulled
    /// off. Strictly read-only — it reads package headers and directory tables, never the
    /// whole of anything.
    Catalog {
        /// Device node (e.g. /dev/sdd) or a directory holding an already-mounted device.
        /// Omit to use whichever attached device holds recognisable content.
        device: Option<String>,
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Pull content off a storage device into staging (stage 2).
    ///
    /// The counterpart of `rip`, producing an ordinary job that `identify`, `migrate` and
    /// `verify` then handle unchanged. Writes only inside the staging root and never
    /// modifies the device.
    ///
    /// Every block read is checked against the hash tree the console recorded in the
    /// package, and the written image is read back and re-hashed before the job is
    /// recorded. What this cannot do is match a dump against Redump: a package holds only
    /// the game partition, so its content is identified from the package's own metadata and
    /// filed as inference, never as a verified dump.
    Pull {
        /// Device node (e.g. /dev/sdd) or a directory holding an already-mounted device.
        /// Omit to use whichever attached device holds recognisable content.
        device: Option<String>,
        /// Title ID, or a unique fragment of a title's name. Omit when the device holds
        /// exactly one game.
        #[arg(long)]
        title: Option<String>,
        /// What to write: a converted single-file image, the package exactly as the console
        /// wrote it, or both.
        ///
        /// The `.iso` is what ordinary tools and emulators read. The package is the
        /// faithful archival form: its hash tree travels with it, so it stays verifiable
        /// long after extraction, which a converted image does not.
        #[arg(long, value_enum, default_value_t = devices::PullFormat::Iso)]
        format: devices::PullFormat,
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
        /// Rip a "play all" title too, instead of skipping it as redundant.
        ///
        /// TV discs often offer one title that plays the episodes back to back. It holds
        /// no unique content but costs as much disk as the rest of the disc, so it is
        /// skipped by default when detected.
        #[arg(long)]
        keep_play_all: bool,
        /// Config file to use instead of the search path.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Rip the disc in a drive and take it as far through the pipeline as it can go.
    ///
    /// Runs rip, identify, repack and migrate in sequence, stopping at the first stage
    /// that cannot proceed without you and saying why. Exact hash matches go all the way
    /// to permanent storage unattended; anything identified by inference — every video
    /// disc, for now — stops after ripping for you to confirm.
    ///
    /// Stopping early is a normal outcome, not a failure, and does not exit non-zero.
    Run {
        /// Device to rip. Defaults to the first drive found.
        device: Option<String>,
        /// Ignore titles shorter than this many seconds.
        #[arg(long, default_value_t = 300)]
        min_length: u32,
        /// Show what would happen without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Do not prompt for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Rip a "play all" title too, instead of skipping it as redundant.
        ///
        /// TV discs often offer one title that plays the episodes back to back. It holds
        /// no unique content but costs as much disk as the rest of the disc, so it is
        /// skipped by default when detected.
        #[arg(long)]
        keep_play_all: bool,
        /// Config file to use instead of the search path.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Inspect and validate configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Identify staged content against datfiles (stage 3).
    ///
    /// Reports what it found and stops, unless --apply is given. Nothing is ever
    /// deleted, and an existing file at a destination name is refused rather than
    /// overwritten.
    Identify {
        /// Job id or a fragment of one. Omit to identify every job.
        job: Option<String>,
        /// Move exactly-matched files into ready/ under their canonical names.
        ///
        /// Only exact hash matches are moved, and only when every file of a multi-file
        /// set has been verified. Where the platform's container policy calls for it
        /// (games.chd_platforms) the set is packed into a CHD, which is unpacked again
        /// and checked against the datfile before it is accepted.
        #[arg(long)]
        apply: bool,
        /// Search term for a video disc, overriding the guess from the volume label.
        #[arg(long)]
        show: Option<String>,
        /// Solve all matching video discs together as one box set.
        #[arg(long)]
        set: bool,
        /// Also file video identifications, which are always inference.
        ///
        /// Requires --apply. Video has no hash to check against, so a match is a
        /// judgement about runtimes and dialogue rather than proof. This flag is how you
        /// say you have reviewed the proposal and accept it; nothing else in the tool
        /// will file an inferred match, and `run` never passes it.
        #[arg(long, requires = "apply")]
        accept_inferred: bool,
        /// Config file to use instead of the search path.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Adopt existing library files that this tool did not rip (stage 0).
    ///
    /// Hashes loose disc images against the datfiles and, on an exact match, creates a
    /// job describing what each one is and where it already lives — after which the
    /// ordinary commands work on them.
    ///
    /// Nothing is moved, renamed or deleted: adopting only records what a file is. Files
    /// with no exact match are reported and left alone, since a guess here would turn
    /// into a wrong rename later. Adopted jobs are marked as such, because a matching
    /// hash is not the same evidence as a verified dump.
    Adopt {
        /// Directory to scan. Defaults to every configured destination root.
        #[arg(long)]
        path: Option<PathBuf>,
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
    /// Re-package identified content into the configured container (stage 3b).
    ///
    /// For content filed before a container policy changed — an `.iso` that should now
    /// be a `.chd`. Only ever adds: the replacement is written to staging and the old
    /// path recorded, and `migrate` retires it once the new file verifies at the
    /// destination.
    Repack {
        /// Job id or a fragment of one. Omit to consider every eligible job.
        job: Option<String>,
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
    /// Migrate identified content to permanent storage (stage 4).
    ///
    /// Copies each file to its destination and verifies it by reading the destination
    /// back and comparing hashes. An existing file at the destination is never
    /// overwritten.
    ///
    /// This is the only command that removes anything, and it does so only after the new
    /// copy is verified in place. It retires files that a `repack` replaced, from both
    /// the destination and staging, and it removes staging copies when
    /// staging.reclaim_after_migrate is set.
    ///
    /// Use --dry-run first to see exactly what would be written and removed.
    Migrate {
        /// Job id or a fragment of one. Omit to migrate every eligible job.
        job: Option<String>,
        /// Destination name, when more than one is configured.
        #[arg(long)]
        destination: Option<String>,
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
    /// Reclaim staging space for content that has reached permanent storage.
    ///
    /// Removes staged copies only after re-hashing the destination copy and confirming
    /// it matches. A destination file that is missing or altered means the staged copy is
    /// the last good one, and it is kept.
    ///
    /// Dump provenance — sector state, logs, subchannel data — is never removed: it
    /// exists nowhere else and cannot be regenerated from the destination copy.
    Clean {
        /// Job id or a fragment of one. Omit to consider every migrated job.
        job: Option<String>,
        /// Also drop the raw stream, sector state and subchannel data.
        ///
        /// These are irreplaceable — nothing can regenerate them, least of all the packed
        /// image, which discards exactly this information. Off by default.
        #[arg(long)]
        provenance: bool,
        /// Also discard unfiled content — disc extras that were never identified.
        ///
        /// Nothing filed these, so no destination copy exists to verify them against and
        /// nothing can bring them back. This is a decision about not wanting the content,
        /// not a reclamation of something already backed up. Off by default.
        #[arg(long)]
        discard_unfiled: bool,
        /// Show what would happen without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Do not prompt for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Config file to use instead of the search path.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Re-verify staged artifacts against their manifest hashes.
    ///
    /// Re-hashes every artifact a job recorded, re-runs the rip-time integrity checks,
    /// and re-hashes whatever was filed into ready/. Read-only unless --update is given.
    Verify {
        /// Job id or a fragment of one. Omit to verify every job.
        job: Option<String>,
        /// Update the recorded stage if the verdict changed.
        ///
        /// A job that fails verification is marked failed. A failed job that now passes
        /// every check is promoted back to ripped — which is how a job rejected by a
        /// since-corrected integrity check recovers without being re-dumped. No file is
        /// touched either way.
        #[arg(long)]
        update: bool,
        /// Config file to use instead of the search path.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List jobs in the staging area and the stage each has reached.
    Jobs {
        /// Config file to use instead of the search path.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Emit JSON instead of a table.
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
        Command::Devices { json } => devices::list(json),
        Command::Catalog { device, json } => devices::catalog(devices::CatalogArgs { device, json }),
        Command::Pull {
            device,
            title,
            format,
            dry_run,
            yes,
            config,
        } => devices::pull(devices::PullArgs {
            device,
            title,
            format,
            dry_run,
            assume_yes: yes,
            config_file: config,
        })
        .map(|_| ()),
        Command::Config { action } => cmd_config(action),
        Command::Run { device, min_length, dry_run, yes, keep_play_all, config } => {
            run::run(run::RunArgs {
                device,
                min_length,
                dry_run,
                assume_yes: yes,
                keep_play_all,
                config_file: config,
            })
        }
        Command::Rip {
            device,
            title,
            min_length,
            dry_run,
            yes,
            keep_play_all,
            config,
        } => rip::run(rip::RipArgs {
            device,
            title,
            min_length,
            dry_run,
            assume_yes: yes,
            keep_play_all,
            config_file: config,
        })
        .map(|_| ()),
        Command::Identify { job, apply, show, set, accept_inferred, config } => {
            identify::run(identify::IdentifyArgs {
                job,
                config_file: config,
                apply,
                show,
                set,
                accept_inferred,
            })
        }
        Command::Adopt { path, dry_run, yes, config } => {
            adopt::run(adopt::AdoptArgs {
                path,
                dry_run,
                assume_yes: yes,
                config_file: config,
            })
        }
        Command::Repack { job, dry_run, yes, config } => {
            repack::run(repack::RepackArgs {
                job,
                dry_run,
                assume_yes: yes,
                config_file: config,
            })
        }
        Command::Migrate { job, destination, dry_run, yes, config } => {
            migrate::run(migrate::MigrateArgs {
                job,
                destination,
                dry_run,
                assume_yes: yes,
                config_file: config,
            })
        }
        Command::Clean { job, provenance, discard_unfiled, dry_run, yes, config } => {
            clean::run(clean::CleanArgs {
                job,
                provenance,
                discard_unfiled,
                dry_run,
                assume_yes: yes,
                config_file: config,
            })
        }
        Command::Verify { job, update, config } => verify::run(verify::VerifyArgs {
            job,
            config_file: config,
            update,
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// clap's own consistency check: duplicate flags, bad defaults, malformed argument
    /// definitions. These are only caught at runtime otherwise, and only on the code path
    /// that happens to use the broken argument.
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// The top-level help is the one thing every user reads, and it is a safety
    /// statement: it is where someone learns that this tool can delete files. It said
    /// "read-only commands implemented so far" long after rip, migrate and repack landed.
    #[test]
    fn top_level_help_does_not_claim_to_be_read_only() {
        let about = Cli::command()
            .get_long_about()
            .map(|s| s.to_string())
            .expect("a long_about");
        assert!(
            !about.to_lowercase().contains("read-only"),
            "the tool writes and deletes; the summary must not say otherwise"
        );
        assert!(
            about.contains("verified"),
            "the summary should say removals happen only after verification"
        );
    }

    /// Every subcommand needs at least a one-line summary; an unexplained command in the
    /// list is worse than no command.
    #[test]
    fn every_subcommand_is_described() {
        for sub in Cli::command().get_subcommands() {
            let name = sub.get_name();
            if name == "help" {
                continue;
            }
            assert!(
                sub.get_about().is_some(),
                "subcommand {name} has no description"
            );
            for arg in sub.get_arguments() {
                assert!(
                    arg.get_help().is_some() || arg.get_long_help().is_some(),
                    "{name} --{} has no help text",
                    arg.get_id()
                );
            }
        }
    }
}
