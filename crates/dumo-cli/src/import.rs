//! `import-rips`: turn video rips made outside this tool into jobs, one per disc.
//!
//! A library ripped with MakeMKV before this tool existed sits in folders of files named
//! after the disc, `Friends Season 6 Disc 1_t04.mkv`, unidentified. Identification works
//! on jobs, so those files are invisible to it. Importing groups them by the disc their
//! names carry and makes each group a job, with the disc's name as its label, so
//! `identify` can search, read dialogue and check against reference subtitles exactly as
//! for a fresh rip.
//!
//! The files are **hard-linked** into the job, not copied or moved: no space is used and
//! the originals stay exactly where they were. That needs them on the staging
//! filesystem; anything else is refused rather than copied. Each is hashed like a rip.
//! The job records [`Job::adopted_from`], since nothing is known of how these bytes came
//! off the disc — only what they are now.

use anyhow::{bail, Context, Result};
use dumo_core::disc::{ContentHint, DiscProbe};
use dumo_core::job::{new_job_id, Artifact, Job, JobStage, TitleInfo};
use dumo_core::{hash, Confidence, Config, MediaKind};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct ImportArgs {
    /// Directory of rips. Searched to any depth.
    pub path: PathBuf,
    pub dry_run: bool,
    pub config_file: Option<PathBuf>,
}

/// The disc and title index a MakeMKV file name carries: `Label_t04.mkv` -> ("Label", 4).
pub fn split_title_name(file_name: &str) -> Option<(String, u32)> {
    let stem = file_name.strip_suffix(".mkv")?;
    let (label, index) = stem.rsplit_once("_t")?;
    if label.trim().is_empty() || index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((label.to_string(), index.parse().ok()?))
}

/// Group rips by disc, each disc's titles in index order. Files that are not named as
/// MakeMKV names them are returned separately.
pub fn group_by_disc(files: &[PathBuf]) -> (BTreeMap<String, Vec<(u32, PathBuf)>>, Vec<PathBuf>) {
    let mut discs: BTreeMap<String, Vec<(u32, PathBuf)>> = BTreeMap::new();
    let mut other = Vec::new();
    for f in files {
        let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match split_title_name(&name) {
            Some((label, index)) => discs.entry(label).or_default().push((index, f.clone())),
            None => other.push(f.clone()),
        }
    }
    for titles in discs.values_mut() {
        titles.sort();
    }
    (discs, other)
}

fn collect_mkvs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let hidden = p
            .file_name()
            .map(|n| n.to_string_lossy().starts_with('.'))
            .unwrap_or(true);
        if hidden {
            continue;
        }
        if p.is_dir() {
            collect_mkvs(&p, out);
        } else if p.extension().map(|x| x.eq_ignore_ascii_case("mkv")).unwrap_or(false) {
            out.push(p);
        }
    }
}

/// Discs already imported from this directory, by label, with their job ids.
///
/// Only finished imports count. One interrupted partway is still `ripping` and holds
/// only some of its titles; that disc is imported again, into a new job.
fn imported(cfg: &Config, source: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(cfg.staging.jobs_dir()) else {
        return out;
    };
    for e in entries.flatten() {
        let Ok(job) = Job::load(&e.path()) else { continue };
        if job.adopted_from.as_deref() != Some(source)
            || matches!(job.stage, JobStage::Ripping | JobStage::Failed)
        {
            continue;
        }
        if let Some(label) = job.probe.as_ref().and_then(|p| p.content.title_guess.clone()) {
            out.insert(label, job.id.clone());
        }
    }
    out
}

pub fn run(args: ImportArgs) -> Result<()> {
    let cfg = Config::load(args.config_file.as_deref())?;
    let source = std::fs::canonicalize(&args.path)
        .with_context(|| format!("reading {}", args.path.display()))?;
    if !source.is_dir() {
        bail!("{} is not a directory", source.display());
    }
    let source_text = source.display().to_string();

    let mut files = Vec::new();
    collect_mkvs(&source, &mut files);
    let (discs, other) = group_by_disc(&files);
    if discs.is_empty() {
        bail!("no MakeMKV-named rips (Label_tNN.mkv) under {}", source.display());
    }
    let done = imported(&cfg, &source_text);

    println!("{} disc(s) under {}:", discs.len(), source.display());
    for (label, titles) in &discs {
        let state = match done.get(label) {
            Some(id) => format!("already imported as {id}"),
            None => "to import".into(),
        };
        println!("  {label:<40} {:>3} title(s)  {state}", titles.len());
    }
    if !other.is_empty() {
        println!("Not named as MakeMKV names rips, so left alone: {} file(s)", other.len());
        for f in other.iter().take(5) {
            println!("  {}", f.display());
        }
    }
    if args.dry_run {
        println!("Dry run: nothing was written.");
        return Ok(());
    }

    let jobs_dir = cfg.staging.jobs_dir();
    let mut created = Vec::new();
    for (label, titles) in &discs {
        if done.contains_key(label) {
            continue;
        }
        println!();
        println!("{label}");
        let id = new_job_id(Some(label));
        let job_dir = jobs_dir.join(&id);
        Job::create_dir(&job_dir)?;
        let raw = job_dir.join("raw");
        std::fs::create_dir_all(&raw)?;

        let mut job = Job::new(id.clone(), "imported".into());
        job.adopted_from = Some(source_text.clone());
        job.backend = Some(format!("imported from {source_text}, hard-linked"));
        // Saved first, so an interruption leaves a job that says what it was for.
        job.stage = JobStage::Ripping;
        job.save(&job_dir)?;

        let mut tallest = 0u32;
        for (index, src) in titles {
            let name = src
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let linked = raw.join(&name);
            if let Err(e) = std::fs::hard_link(src, &linked) {
                job.stage = JobStage::Failed;
                job.error = Some(format!("hard-linking {}: {e}", src.display()));
                job.save(&job_dir)?;
                bail!(
                    "could not hard-link {} into the job ({e}); it must be on the staging \
                     filesystem. The job is marked failed and the original is untouched.",
                    src.display()
                );
            }
            print!("  {name} ... ");
            std::io::stdout().flush().ok();
            let p = dumo_backends::ffprobe::probe(&linked)
                .with_context(|| format!("probing {}", linked.display()))?;
            tallest = tallest.max(
                p.streams
                    .iter()
                    .filter(|s| s.kind == "video")
                    .filter_map(|s| s.height)
                    .max()
                    .unwrap_or(0),
            );
            let (sha256, bytes) =
                hash::sha256_file(&linked).with_context(|| format!("hashing {}", linked.display()))?;
            println!("{:>6.0} s  {}", p.duration_secs, &sha256[..16]);
            job.titles.push(TitleInfo {
                index: *index,
                duration_secs: p.duration_secs.round() as u64,
                estimated_bytes: bytes,
                chapters: p.chapter_count() as u32,
                output_name: name.clone(),
            });
            job.selected_titles.push(*index);
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
        // Identification takes the video path for a disc of video; the resolution says
        // which kind it was.
        let kind = if tallest >= 720 {
            MediaKind::BluRayVideo
        } else {
            MediaKind::DvdVideo
        };
        job.probe = Some(DiscProbe {
            device: "imported".into(),
            profile: None,
            content: ContentHint::new(kind, Confidence::Weak)
                .with_title(label.clone())
                .with_evidence(format!("imported from {source_text}; label from the file names")),
            volume: None,
            toc: None,
            game_serial: None,
            capacity_bytes: None,
            root_entries: Vec::new(),
            probe_millis: 0,
        });
        job.stage = JobStage::Ripped;
        job.save(&job_dir)?;
        created.push(id);
    }

    println!();
    println!("Imported {} disc(s). The originals were not moved or changed.", created.len());
    for id in &created {
        println!("  dump-o-matic identify {id}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_disc_and_index_from_a_makemkv_name() {
        assert_eq!(
            split_title_name("Friends Season 6 Disc 1_t04.mkv"),
            Some(("Friends Season 6 Disc 1".to_string(), 4))
        );
        assert_eq!(split_title_name("B1_t00.mkv"), Some(("B1".to_string(), 0)));
        // A label may itself hold "_t".
        assert_eq!(split_title_name("My_title_t12.mkv"), Some(("My_title".to_string(), 12)));
        assert_eq!(split_title_name("Friends (1994) S01E01 - Pilot.mkv"), None);
        assert_eq!(split_title_name("Disc_tx.mkv"), None);
        assert_eq!(split_title_name("_t03.mkv"), None);
        assert_eq!(split_title_name("Disc_t03.m2ts"), None);
    }

    #[test]
    fn groups_rips_by_disc_in_title_order() {
        let files: Vec<PathBuf> = [
            "a/Show Disc 2_t01.mkv",
            "a/Show Disc 1_t10.mkv",
            "a/Show Disc 1_t02.mkv",
            "a/Show (2001) S01E01 - Pilot.mkv",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let (discs, other) = group_by_disc(&files);
        assert_eq!(discs.len(), 2);
        let one: Vec<u32> = discs["Show Disc 1"].iter().map(|(i, _)| *i).collect();
        assert_eq!(one, vec![2, 10]);
        assert_eq!(other, vec![PathBuf::from("a/Show (2001) S01E01 - Pilot.mkv")]);
    }
}
