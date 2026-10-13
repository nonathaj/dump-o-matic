//! `split`: cut one title of a job at its chapter marks, into a job of its own.
//!
//! For extras that are compilations of things filed separately, such as eleven
//! mini-anime shorts that are eleven specials. The pieces go into a **new** job, so the
//! job they came from, which may well be migrated already, is not touched; the title
//! stays there as it was. The new job carries the source disc's probe, so identify
//! searches for the same series, and its pieces are hashed like any rip.

use anyhow::{bail, Context, Result};
use dumo_core::job::{new_job_id, Artifact, Job, JobStage, TitleInfo};
use dumo_core::{hash, Config};
use std::io::Write;
use std::path::PathBuf;

pub struct SplitArgs {
    /// Job id or a fragment of one.
    pub job: String,
    /// The title to cut, by file name, as listed in the job.
    pub title: String,
    pub dry_run: bool,
    pub config_file: Option<PathBuf>,
}

pub fn run(args: SplitArgs) -> Result<()> {
    let cfg = Config::load(args.config_file.as_deref())?;
    let jobs_dir = cfg.staging.jobs_dir();

    let mut matches: Vec<PathBuf> = std::fs::read_dir(&jobs_dir)
        .with_context(|| format!("reading {}", jobs_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .map(|n| n.to_string_lossy().contains(args.job.as_str()))
                    .unwrap_or(false)
        })
        .collect();
    matches.sort();
    let job_dir = match matches.as_slice() {
        [one] => one.clone(),
        [] => bail!("no job matching {:?} in {}", args.job, jobs_dir.display()),
        many => bail!(
            "{:?} matches {} jobs; give more of the id:\n  {}",
            args.job,
            many.len(),
            many.iter()
                .map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("\n  ")
        ),
    };
    let source_job = Job::load(&job_dir)?;

    let artifact = source_job
        .artifacts
        .iter()
        .find(|a| {
            a.relative_path == args.title
                || std::path::Path::new(&a.relative_path)
                    .file_name()
                    .map(|n| n.to_string_lossy() == args.title)
                    .unwrap_or(false)
        })
        .with_context(|| {
            format!(
                "job {} has no title {:?}; its titles are:\n  {}",
                source_job.id,
                args.title,
                source_job
                    .artifacts
                    .iter()
                    .map(|a| a.relative_path.as_str())
                    .collect::<Vec<_>>()
                    .join("\n  ")
            )
        })?;
    let src = job_dir.join(&artifact.relative_path);

    let probe = dumo_backends::ffprobe::probe(&src)
        .with_context(|| format!("probing {}", src.display()))?;
    let chapters = probe.chapter_count();
    if chapters < 2 {
        bail!("{} has {chapters} chapter(s): nothing to cut", artifact.relative_path);
    }

    let stem = std::path::Path::new(&artifact.relative_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "title".into());
    println!(
        "{} ({}) has {chapters} chapters; each becomes a title of a new job.",
        artifact.relative_path, source_job.id
    );
    if args.dry_run {
        println!("Dry run: nothing was written.");
        return Ok(());
    }

    // The pieces are only as good as the source, so check it is still what was ripped.
    print!("Checking the source against its recorded hash ... ");
    std::io::stdout().flush().ok();
    let (sha256, _) = hash::sha256_file(&src).with_context(|| format!("hashing {}", src.display()))?;
    if sha256 != artifact.sha256 {
        println!("MISMATCH");
        bail!(
            "{} no longer matches the hash recorded when it was ripped; not cutting it",
            src.display()
        );
    }
    println!("ok");

    let label = source_job
        .probe
        .as_ref()
        .and_then(|p| p.content.title_guess.clone())
        .unwrap_or_else(|| source_job.id.clone());
    // MakeMKV names titles after the disc, so the stem usually carries the label already.
    let name = if stem.to_lowercase().contains(&label.to_lowercase()) {
        format!("{stem} split")
    } else {
        format!("{label} {stem} split")
    };
    let id = new_job_id(Some(&name));
    let new_dir = jobs_dir.join(&id);
    Job::create_dir(&new_dir)?;
    let raw = new_dir.join("raw");
    std::fs::create_dir_all(&raw)?;

    let mut job = Job::new(id.clone(), source_job.device.clone());
    job.probe = source_job.probe.clone();
    job.backend = Some(format!(
        "{}, splitting {}/{} at its chapters",
        dumo_backends::mkvmerge::version().unwrap_or_else(|_| "mkvmerge".into()),
        source_job.id,
        artifact.relative_path
    ));
    // Saved before cutting, so an interruption leaves a job that says what it was for.
    job.stage = JobStage::Ripping;
    job.save(&new_dir)?;

    print!("Cutting at the chapters ... ");
    std::io::stdout().flush().ok();
    let pieces = dumo_backends::mkvmerge::split_chapters(&src, &raw, &stem)?;
    println!("{} piece(s)", pieces.len());
    if pieces.len() != chapters {
        job.stage = JobStage::Failed;
        job.error = Some(format!("{} pieces from {chapters} chapters", pieces.len()));
        job.save(&new_dir)?;
        bail!("expected {chapters} pieces, got {}; the job is marked failed", pieces.len());
    }

    let mut total = 0.0;
    for (i, piece) in pieces.iter().enumerate() {
        let name = piece
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let (sha256, bytes) =
            hash::sha256_file(piece).with_context(|| format!("hashing {}", piece.display()))?;
        let p = dumo_backends::ffprobe::probe(piece)
            .with_context(|| format!("probing {}", piece.display()))?;
        total += p.duration_secs;
        println!("  {name}  {:>6.1} s  {}", p.duration_secs, &sha256[..16]);
        job.titles.push(TitleInfo {
            index: i as u32,
            duration_secs: p.duration_secs.round() as u64,
            estimated_bytes: bytes,
            chapters: 1,
            output_name: name.clone(),
        });
        job.selected_titles.push(i as u32);
        job.artifacts.push(Artifact {
            relative_path: format!("raw/{name}"),
            bytes,
            sha256,
            hashed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
    }
    // Cutting at keyframes cannot lose more than a frame or two per piece.
    let missing = (probe.duration_secs - total).abs();
    if missing > pieces.len() as f64 {
        job.stage = JobStage::Failed;
        job.error = Some(format!("pieces run {total:.1} s against {:.1} s", probe.duration_secs));
        job.save(&new_dir)?;
        bail!(
            "the pieces run {total:.1} s together, against {:.1} s for the title; the job \
             is marked failed",
            probe.duration_secs
        );
    }
    job.stage = JobStage::Ripped;
    job.save(&new_dir)?;

    println!();
    println!("Job:    {}", new_dir.display());
    println!(
        "The source title is unchanged and still in {}; nothing was deleted.",
        source_job.id
    );
    println!("Next:   dump-o-matic identify {id} --specials");
    Ok(())
}
