//! Drive the whole disc pipeline from one command.
//!
//! The stages exist separately because each is a real decision point, but running them by
//! hand makes the operator the state machine — and the design already says what should
//! happen without them: an exact hash match may proceed unattended, and only an uncertain
//! identification needs a person.
//!
//! So this runs rip → identify → repack → migrate and **stops at the first stage that
//! cannot proceed on its own**, saying why and what to run next. It is not an
//! insert-and-forget auto-ripper: it stops early by design and often, because most of
//! what it handles is not exactly matchable. A video disc, for instance, always stops
//! after ripping, since no amount of inference earns unattended filing.
//!
//! Every stage keeps its own safety behaviour unchanged; this only decides whether to
//! call the next one. The job's recorded stage is the signal — if a stage did not advance
//! it, the pipeline halts rather than guessing.

use anyhow::{Context, Result};
use dumo_core::config::Config;
use dumo_core::job::{Job, JobStage};
use std::path::PathBuf;

pub struct RunArgs {
    pub device: Option<String>,
    pub min_length: u32,
    pub dry_run: bool,
    pub assume_yes: bool,
    pub keep_play_all: bool,
    pub config_file: Option<PathBuf>,
}

pub fn run(args: RunArgs) -> Result<()> {
    // --- Stage 2: rip (probing happens inside) ---
    let job_id = crate::rip::run(crate::rip::RipArgs {
        device: args.device.clone(),
        title: None,
        min_length: args.min_length,
        dry_run: args.dry_run,
        assume_yes: args.assume_yes,
        keep_play_all: args.keep_play_all,
        config_file: args.config_file.clone(),
    })?;

    let Some(job_id) = job_id else {
        // A dry run, or a rip that declined to start. Either way there is nothing to
        // carry forward, and the rip has already explained itself.
        return Ok(());
    };

    let cfg = Config::load(args.config_file.as_deref())?;
    let job_dir = cfg.staging.job_dir(&job_id);
    let stage = |dir: &std::path::Path| -> Result<JobStage> {
        Ok(Job::load(dir).context("re-reading the job")?.stage)
    };

    if stage(&job_dir)? != JobStage::Ripped {
        return halt(&job_id, "the rip did not complete cleanly", "verify");
    }

    // --- Stage 3: identify, applying only what is exactly matched ---
    banner("identify");
    crate::identify::run(crate::identify::IdentifyArgs {
        job: Some(job_id.clone()),
        config_file: args.config_file.clone(),
        apply: true,
        show: None,
        set: false,
        // Never. An inferred match is a judgement call and this is the unattended path.
        accept_inferred: false,
        // Likewise never chosen for you: an audio disc ID shared by several releases
        // stops here until a person picks one.
        release: None,
    })?;

    if stage(&job_dir)? != JobStage::Identified {
        return halt(
            &job_id,
            "nothing was filed: the content was not identified by an exact hash match",
            "identify",
        );
    }

    // --- Stage 3b: repack into the configured container, if the policy calls for it ---
    banner("repack");
    crate::repack::run(crate::repack::RepackArgs {
        job: Some(job_id.clone()),
        dry_run: args.dry_run,
        assume_yes: args.assume_yes,
        config_file: args.config_file.clone(),
    })?;

    // --- Stage 4: migrate to permanent storage ---
    banner("migrate");
    crate::migrate::run(crate::migrate::MigrateArgs {
        job: Some(job_id.clone()),
        destination: None,
        dry_run: args.dry_run,
        assume_yes: args.assume_yes,
        config_file: args.config_file.clone(),
    })?;

    if stage(&job_dir)? != JobStage::Migrated {
        return halt(&job_id, "the content did not reach permanent storage", "migrate");
    }

    println!();
    println!("Done: {job_id} is ripped, identified, packaged and migrated.");
    println!("The disc has not been modified. Eject it and insert the next one.");
    Ok(())
}

fn banner(stage: &str) {
    println!();
    println!("── {stage} {}", "─".repeat(60usize.saturating_sub(stage.len())));
}

/// Stop the pipeline, saying plainly why and what to do next.
///
/// Not an error: stopping is the expected outcome whenever something needs a person, and
/// exiting non-zero would make a routine "this needs your eyes" indistinguishable from a
/// failure in a script.
fn halt(job_id: &str, why: &str, next: &str) -> Result<()> {
    println!();
    println!("Stopped: {why}.");
    println!("Nothing further was written, and nothing was deleted.");
    println!("Next: dump-o-matic {next} {job_id}");
    Ok(())
}
