//! Job state: what is being ripped, how far it got, and what was produced.
//!
//! State lives in a `job.json` manifest inside each job directory. Two properties matter:
//!
//! - **Crash-safe writes.** The manifest is written to a temp file and renamed into
//!   place, so an interrupted save leaves the previous manifest intact rather than a
//!   truncated one. A half-written manifest would be worse than none: it could make the
//!   tool believe artifacts were verified when they were not.
//! - **Self-describing on disk.** Everything needed to resume or audit a job is in the
//!   job directory, so a job survives the database, the tool, and the operator's memory.

use crate::disc::DiscProbe;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Manifest filename inside a job directory.
pub const MANIFEST_NAME: &str = "job.json";

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("job directory {path} already exists; refusing to reuse it")]
    AlreadyExists { path: PathBuf },

    #[error("no job manifest at {path}")]
    NoManifest { path: PathBuf },

    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("malformed manifest at {path}: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

type Result<T> = std::result::Result<T, JobError>;

/// How far a job has progressed through the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStage {
    /// Disc probed, nothing written yet.
    Probed,
    /// Rip in progress. A job left in this state was interrupted.
    Ripping,
    /// Rip finished and every artifact hashed.
    Ripped,
    /// Content identified and renamed in staging.
    Identified,
    /// Verified onto permanent storage.
    Migrated,
    /// Terminal failure; see `error`.
    Failed,
}

impl std::fmt::Display for JobStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            JobStage::Probed => "probed",
            JobStage::Ripping => "ripping",
            JobStage::Ripped => "ripped",
            JobStage::Identified => "identified",
            JobStage::Migrated => "migrated",
            JobStage::Failed => "failed",
        };
        f.write_str(s)
    }
}

/// A file produced by a rip, with the hash that proves its integrity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    /// Path relative to the job directory, so a job directory stays relocatable.
    pub relative_path: String,
    pub bytes: u64,
    /// Lowercase hex SHA-256 of the file as written to staging.
    pub sha256: String,
    /// When the hash was computed, as a unix timestamp.
    pub hashed_at: u64,
}

/// A title offered by the source disc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleInfo {
    /// Backend title index, used to select it for ripping.
    pub index: u32,
    pub duration_secs: u64,
    /// Size the backend estimates this title will occupy.
    pub estimated_bytes: u64,
    pub chapters: u32,
    /// Filename the backend intends to write.
    pub output_name: String,
}

impl TitleInfo {
    pub fn duration_hms(&self) -> String {
        let h = self.duration_secs / 3600;
        let m = (self.duration_secs % 3600) / 60;
        let s = self.duration_secs % 60;
        format!("{h}:{m:02}:{s:02}")
    }
}

/// A file placed in the staging `ready/` tree under its final name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyFile {
    /// Path relative to the **staging root**, e.g. `ready/ps2/Title (USA).iso`.
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// The outcome of identifying a job's content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identification {
    /// Canonical title, e.g. `Lord of the Rings, The - The Two Towers (USA)`.
    pub title: String,
    /// Source vocabulary platform name, e.g. `Sony - PlayStation 2`.
    pub platform: String,
    /// ES-DE / EmuDeck directory slug, e.g. `ps2`.
    pub platform_slug: String,
    /// Which digest produced the match.
    pub matched_on: String,
    pub confidence: crate::Confidence,
    /// Datfile the match came from, for auditability.
    pub source: String,
    /// Files now sitting in `ready/` under their final names.
    pub files: Vec<ReadyFile>,
    /// Staging-relative paths this identification *replaced*, still to be cleaned up.
    ///
    /// Written when content is repacked into a different container — an `.iso` becoming
    /// a `.chd`, say. The old file may already have been migrated to permanent storage,
    /// so it cannot simply be deleted here: it is recorded, and removed only once its
    /// replacement has been placed and verified at the destination. That ordering is the
    /// whole point of the field, so nothing is ever deleted before its successor exists.
    #[serde(default)]
    pub superseded: Vec<String>,
    /// Unix timestamp of identification.
    pub identified_at: u64,
}

/// Provenance for content taken off a storage device rather than ripped from a disc.
///
/// A disc rip is evidenced by the drive, the probe, redumper's per-sector state and its
/// logs. Content pulled off a USB drive has none of that, but it is not evidence-free
/// either: an Xbox 360 package carries a SHA-1 hash tree over its own data, so every block
/// can be checked as it is read, and the game's executable independently repeats the title
/// and media IDs the package claims. Recording all of it keeps the distinction visible —
/// this is a faithful, verified copy of a package, which is not the same thing as a
/// verified dump of a disc, and the manifest should never let the two blur.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceSource {
    /// Device or directory the content was read from.
    pub device: String,
    /// Layout that was detected, e.g. `xbox360_content`.
    pub layout: String,
    /// Console's identifier for the title, e.g. `545407E0`.
    pub title_id: String,
    /// Name as the package gives it.
    pub name: String,
    pub media_id: String,
    /// Content type, e.g. `Games on Demand`.
    pub content_kind: String,
    /// How the package was signed. A console-signed package was licensed to one console.
    pub signature: String,
    /// Path of the package within the device.
    pub package_path: String,
    pub package_bytes: u64,
    /// Blocks read and checked against the package's own hash tree during extraction.
    ///
    /// The core of the integrity claim: a nonzero count here means every byte written was
    /// confirmed against a hash the console recorded when it wrote the package.
    pub blocks_verified: u64,
    /// Root of the package's hash tree, as declared in its header.
    pub root_hash: String,
    /// Title ID read from the game's own executable, when it could be read.
    pub xex_title_id: Option<String>,
    /// Media ID read from the game's own executable, when it could be read.
    pub xex_media_id: Option<String>,
    /// SHA-256 of the image data alone, excluding the leading reserved region.
    ///
    /// Kept separately from the artifact's own hash because the reserved region names the
    /// tool that produced the file. Two tools converting the same package will differ
    /// there and agree here, so this is the field that can be compared against a
    /// conversion made by something else.
    pub image_sha256: Option<String>,
    /// Block base the package's filesystem addresses were relative to.
    pub base_blocks: u64,
    /// Constant subtracted from every sector reference to produce a standard image.
    pub shift_sectors: u32,
    /// Blocks of 0x1000 bytes in the converted image, zero if no image was written.
    ///
    /// An independent statement of how long the image should be. A disc rip checks its
    /// image against the sector count the drive reported; there is no drive here, so this
    /// plays the same role — it comes from the package's own geometry rather than from the
    /// file, so it still catches a truncated image long after the device is unplugged.
    #[serde(default)]
    pub image_blocks: u64,
    pub extracted_at: u64,
}

/// A single unit of work through the pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    /// Unix timestamp of creation.
    pub created_at: u64,
    pub stage: JobStage,
    /// Device the disc was read from.
    pub device: String,
    /// Stage 1 probe results, retained as provenance for later identification.
    pub probe: Option<DiscProbe>,
    /// Titles the backend reported.
    pub titles: Vec<TitleInfo>,
    /// Title indices selected for ripping.
    pub selected_titles: Vec<u32>,
    /// Files produced, with hashes.
    pub artifacts: Vec<Artifact>,
    /// Backend that produced the artifacts, with its version.
    pub backend: Option<String>,
    /// Failure detail when `stage` is `Failed`.
    pub error: Option<String>,
    /// Identification result, once stage 3 has run.
    #[serde(default)]
    pub identification: Option<Identification>,
    /// Set when this job describes a file that was already in the library rather than
    /// one this tool dumped.
    ///
    /// Recorded because it is a real difference in what is known. A ripped job carries
    /// the drive, the probe, redumper's per-sector `.state` and the logs — evidence about
    /// how the bytes came off the disc. An adopted job has none of that: all that is
    /// known is that the file's hash matches a datfile entry today. That is enough to
    /// name and repack it safely, and not enough to call it a verified dump, so the two
    /// must stay distinguishable.
    #[serde(default)]
    pub adopted_from: Option<String>,
    /// Set when this job's content came off a storage device rather than a disc.
    ///
    /// Mutually exclusive with a meaningful `probe` in practice: one describes a disc in a
    /// drive, the other a package in a filesystem.
    #[serde(default)]
    pub device_source: Option<DeviceSource>,

    /// Artifacts deliberately released by `clean`, kept as a record of what was here.
    ///
    /// They are removed from `artifacts` so the manifest keeps describing what is
    /// actually on disk, but the fact of their existence is not forgotten: a reclaimed
    /// file is not a lost one, and `verify` must not report it as missing.
    #[serde(default)]
    pub reclaimed: Vec<String>,
}

impl Job {
    pub fn new(id: String, device: String) -> Self {
        Self {
            id,
            created_at: now_unix(),
            stage: JobStage::Probed,
            device,
            probe: None,
            titles: Vec::new(),
            selected_titles: Vec::new(),
            artifacts: Vec::new(),
            backend: None,
            error: None,
            identification: None,
            adopted_from: None,
            device_source: None,
            reclaimed: Vec::new(),
        }
    }

    /// Total size of the selected titles, for pre-flight space checks.
    pub fn estimated_bytes(&self) -> u64 {
        self.titles
            .iter()
            .filter(|t| self.selected_titles.contains(&t.index))
            .map(|t| t.estimated_bytes)
            .sum()
    }

    pub fn total_artifact_bytes(&self) -> u64 {
        self.artifacts.iter().map(|a| a.bytes).sum()
    }

    /// Record that an artifact has been filed out of the job directory into `ready/`.
    ///
    /// The bytes are not gone — they are the file in `ready/`, recorded in
    /// [`Identification::files`] with the same hash — but they are no longer at the path
    /// the artifact list gives. Moving the entry to `reclaimed` keeps the manifest
    /// describing what is actually on disk, without forgetting that the file was produced.
    /// Skipping this makes `verify` report a file the tool filed itself as missing.
    pub fn mark_filed(&mut self, relative_path: &str) {
        self.artifacts.retain(|a| a.relative_path != relative_path);
        if !self.reclaimed.iter().any(|r| r == relative_path) {
            self.reclaimed.push(relative_path.to_string());
        }
    }

    /// Write the manifest into `job_dir`, atomically.
    pub fn save(&self, job_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(job_dir).map_err(|e| JobError::Io {
            path: job_dir.to_path_buf(),
            source: e,
        })?;

        let final_path = job_dir.join(MANIFEST_NAME);
        let tmp_path = job_dir.join(format!("{MANIFEST_NAME}.tmp"));

        let json = serde_json::to_string_pretty(self).map_err(|e| JobError::Malformed {
            path: final_path.clone(),
            source: e,
        })?;

        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp_path).map_err(|e| JobError::Io {
                path: tmp_path.clone(),
                source: e,
            })?;
            f.write_all(json.as_bytes()).map_err(|e| JobError::Io {
                path: tmp_path.clone(),
                source: e,
            })?;
            // Flush to disk before the rename, so a crash cannot leave a manifest that
            // exists but is empty.
            f.sync_all().map_err(|e| JobError::Io {
                path: tmp_path.clone(),
                source: e,
            })?;
        }

        std::fs::rename(&tmp_path, &final_path).map_err(|e| JobError::Io {
            path: final_path.clone(),
            source: e,
        })
    }

    /// Load a manifest from a job directory.
    pub fn load(job_dir: &Path) -> Result<Self> {
        let path = job_dir.join(MANIFEST_NAME);
        if !path.is_file() {
            return Err(JobError::NoManifest { path });
        }
        let text = std::fs::read_to_string(&path).map_err(|e| JobError::Io {
            path: path.clone(),
            source: e,
        })?;
        serde_json::from_str(&text).map_err(|e| JobError::Malformed { path, source: e })
    }

    /// Create a job directory, refusing to reuse an existing one.
    ///
    /// Reusing a directory risks mixing artifacts from two rips, which would make the
    /// manifest's hashes describe files that are no longer there.
    pub fn create_dir(job_dir: &Path) -> Result<()> {
        if job_dir.exists() {
            return Err(JobError::AlreadyExists {
                path: job_dir.to_path_buf(),
            });
        }
        std::fs::create_dir_all(job_dir).map_err(|e| JobError::Io {
            path: job_dir.to_path_buf(),
            source: e,
        })
    }
}

/// Generate a job id: a sortable timestamp plus a short disambiguator.
///
/// Sortable so `ls` orders jobs chronologically; disambiguated so two jobs started in the
/// same second cannot collide.
pub fn new_job_id(label: Option<&str>) -> String {
    let ts = now_unix();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);

    let base = format!("{}-{:04x}", format_timestamp(ts), (nanos >> 8) & 0xFFFF);
    match label.map(slugify).filter(|s| !s.is_empty()) {
        Some(l) => format!("{base}-{l}"),
        None => base,
    }
}

/// Reduce arbitrary text to a filesystem-safe slug.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').chars().take(48).collect()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format a unix timestamp as `YYYYMMDD-HHMMSS` in UTC, without pulling in a date crate.
fn format_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Howard Hinnant's days-from-civil algorithm, inverted.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {

    fn artifact(path: &str) -> Artifact {
        Artifact {
            relative_path: path.to_string(),
            bytes: 10,
            sha256: "abc".to_string(),
            hashed_at: 0,
        }
    }

    /// Filing moves the entry rather than dropping it: `verify` must not call a file it
    /// filed itself missing, and the manifest must still record that it existed.
    #[test]
    fn filing_an_artifact_moves_it_to_reclaimed() {
        let mut job = Job::new("j".to_string(), "/dev/sdd".to_string());
        job.artifacts = vec![artifact("raw/Prey.iso"), artifact("raw/other.iso")];

        job.mark_filed("raw/Prey.iso");

        assert_eq!(job.artifacts.len(), 1);
        assert_eq!(job.artifacts[0].relative_path, "raw/other.iso");
        assert_eq!(job.reclaimed, vec!["raw/Prey.iso".to_string()]);
    }

    #[test]
    fn filing_the_same_artifact_twice_records_it_once() {
        let mut job = Job::new("j".to_string(), "/dev/sdd".to_string());
        job.artifacts = vec![artifact("raw/Prey.iso")];
        job.mark_filed("raw/Prey.iso");
        job.mark_filed("raw/Prey.iso");
        assert_eq!(job.reclaimed.len(), 1);
        assert!(job.artifacts.is_empty());
    }

    #[test]
    fn filing_an_unknown_path_leaves_the_artifacts_alone() {
        let mut job = Job::new("j".to_string(), "/dev/sdd".to_string());
        job.artifacts = vec![artifact("raw/Prey.iso")];
        job.mark_filed("raw/nothing.iso");
        assert_eq!(job.artifacts.len(), 1);
    }

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("dumo-job-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn manifest_round_trips() {
        let dir = scratch("roundtrip");
        let mut job = Job::new("job-1".into(), "/dev/sr0".into());
        job.titles.push(TitleInfo {
            index: 3,
            duration_secs: 3066,
            estimated_bytes: 4_000_000_000,
            chapters: 4,
            output_name: "title_t03.mkv".into(),
        });
        job.selected_titles.push(3);
        job.save(&dir).unwrap();

        let loaded = Job::load(&dir).unwrap();
        assert_eq!(loaded.id, "job-1");
        assert_eq!(loaded.titles.len(), 1);
        assert_eq!(loaded.estimated_bytes(), 4_000_000_000);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = scratch("notmp");
        Job::new("j".into(), "/dev/sr0".into()).save(&dir).unwrap();
        assert!(dir.join(MANIFEST_NAME).is_file());
        assert!(!dir.join(format!("{MANIFEST_NAME}.tmp")).exists());
        std::fs::remove_dir_all(dir).ok();
    }

    /// A partially-written manifest must never replace a good one.
    #[test]
    fn resaving_preserves_a_readable_manifest() {
        let dir = scratch("resave");
        let mut job = Job::new("j".into(), "/dev/sr0".into());
        job.save(&dir).unwrap();
        job.stage = JobStage::Ripped;
        job.save(&dir).unwrap();
        assert_eq!(Job::load(&dir).unwrap().stage, JobStage::Ripped);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn create_dir_refuses_to_reuse() {
        let dir = scratch("reuse");
        Job::create_dir(&dir).unwrap();
        let err = Job::create_dir(&dir).unwrap_err();
        assert!(matches!(err, JobError::AlreadyExists { .. }));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn estimated_bytes_counts_only_selected_titles() {
        let mut job = Job::new("j".into(), "/dev/sr0".into());
        for i in 0..3u32 {
            job.titles.push(TitleInfo {
                index: i,
                duration_secs: 60,
                estimated_bytes: 1000,
                chapters: 1,
                output_name: format!("t{i}.mkv"),
            });
        }
        job.selected_titles = vec![0, 2];
        assert_eq!(job.estimated_bytes(), 2000);
    }

    #[test]
    fn job_ids_are_sortable_and_unique() {
        let a = new_job_id(None);
        let b = new_job_id(None);
        assert_ne!(a, b, "ids generated back to back must differ");
        assert!(a.len() >= 15, "got {a}");
    }

    #[test]
    fn job_id_includes_slugified_label() {
        let id = new_job_id(Some("ESPN_30_FOR_30_DISC_1"));
        assert!(id.ends_with("espn-30-for-30-disc-1"), "got {id}");
    }

    #[test]
    fn slugify_is_filesystem_safe() {
        assert_eq!(slugify("Hello, World! 2024"), "hello-world-2024");
        assert_eq!(slugify("../../etc/passwd"), "etc-passwd");
        assert_eq!(slugify("   "), "");
    }

    #[test]
    fn timestamp_formats_known_epoch_dates() {
        // 2024-01-01T00:00:00Z
        assert_eq!(format_timestamp(1_704_067_200), "20240101-000000");
        // 1970-01-01T00:00:01Z
        assert_eq!(format_timestamp(1), "19700101-000001");
    }

    #[test]
    fn duration_formats_as_hms() {
        let t = TitleInfo {
            index: 0,
            duration_secs: 3066,
            estimated_bytes: 0,
            chapters: 0,
            output_name: String::new(),
        };
        assert_eq!(t.duration_hms(), "0:51:06");
    }
}
