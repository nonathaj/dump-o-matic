//! redumper adapter for game discs and other bit-accurate dumps.
//!
//! redumper is the reference tool for archival disc dumping: it handles PS1/PS2
//! subchannel and protection quirks correctly and produces Redump-comparable hashes.
//! This module drives it as a subprocess and parses its output.
//!
//! The safety-critical part is **error accounting**. redumper reports running SCSI and
//! EDC error counts, and a dump with non-zero errors is not archival-quality even though
//! the process exits successfully. Treating such a dump as good would put a corrupt image
//! into the pipeline with a valid-looking hash — so errors are surfaced explicitly and
//! the dump is marked unclean rather than silently accepted.

use crate::{BackendError, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const TOOL: &str = "redumper";

/// Progress during a dump.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DumpProgress {
    pub percent: u32,
    pub current_lba: u64,
    pub total_lba: u64,
    pub scsi_errors: u64,
    pub edc_errors: u64,
}

/// A hash entry from redumper's DAT output, in Redump's vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub struct RomHash {
    pub name: String,
    pub size: u64,
    pub crc32: String,
    pub md5: String,
    pub sha1: String,
}

/// Per-sector outcome, from redumper's `.state` sidecar.
///
/// Values come from redumper's `State` enum, verified against its source:
/// `0 = ERROR_SKIP`, `1 = ERROR_C2`, `2 = SUCCESS_C2_OFF`, `3 = SUCCESS_SCSI_OFF`,
/// `4 = SUCCESS`. Only 0 and 1 are failures; the rest are successful reads that merely
/// record how the sector was obtained.
pub const STATE_ERROR_SKIP: u8 = 0;
pub const STATE_ERROR_C2: u8 = 1;
/// Lowest state value that still means "this sector was read successfully".
pub const STATE_FIRST_SUCCESS: u8 = 2;

/// What a single byte of a `.state` file describes.
///
/// This is not cosmetic: it changes what "complete" means. Getting it wrong made an
/// otherwise flawless CD dump fail its integrity gate, because 26 million lead-in
/// *samples* were counted as unreadable *sectors*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StateUnit {
    /// DVD/BD: one byte per 2048-byte sector, covering exactly the emitted image.
    /// Every byte must be a success — there is no region here that is not the image.
    #[default]
    Sector,
    /// CD: one byte per 4-byte sample of the raw scrambled stream, covering the lead-in,
    /// pre-gap, track data *and* lead-out. The disc's outer regions are routinely
    /// unreachable on drives redumper does not have calibration data for, and they are
    /// not part of a Redump-conformant dump, so only the interior must be clean.
    Sample,
}

/// Summary of the `.state` file: the authoritative record of which units are good.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateSummary {
    pub unit: StateUnit,
    /// Number of units the file covers (bytes in the file).
    pub total_sectors: u64,
    pub error_skip: u64,
    pub error_c2: u64,
    /// Contiguous runs of unreadable units, as inclusive `(first, last)` indices.
    pub bad_runs: Vec<(u64, u64)>,
}

impl StateSummary {
    pub fn bad_sectors(&self) -> u64 {
        self.error_skip + self.error_c2
    }

    /// Runs that touch the start or end of the covered range.
    ///
    /// On a CD these are the lead-in and lead-out: regions outside the emitted tracks
    /// that many drives simply cannot reach. Worth reporting, never a reason to reject
    /// an image whose tracks are intact.
    pub fn edge_runs(&self) -> Vec<(u64, u64)> {
        let last = self.total_sectors.saturating_sub(1);
        self.bad_runs
            .iter()
            .copied()
            .filter(|&(a, b)| a == 0 || b == last)
            .collect()
    }

    /// Runs strictly inside the covered range — damage within the data itself.
    pub fn interior_runs(&self) -> Vec<(u64, u64)> {
        let last = self.total_sectors.saturating_sub(1);
        self.bad_runs
            .iter()
            .copied()
            .filter(|&(a, b)| a != 0 && b != last)
            .collect()
    }

    fn run_len((a, b): (u64, u64)) -> u64 {
        b - a + 1
    }

    /// Unreadable units that fall inside the data, excluding the unreachable edges.
    pub fn interior_bad(&self) -> u64 {
        self.interior_runs().into_iter().map(Self::run_len).sum()
    }

    /// Unreadable units in the lead-in/lead-out.
    pub fn edge_bad(&self) -> u64 {
        self.edge_runs().into_iter().map(Self::run_len).sum()
    }

    /// Word for one unit, for messages that would otherwise say "sector" about samples.
    pub fn unit_noun(&self) -> &'static str {
        match self.unit {
            StateUnit::Sector => "sector",
            StateUnit::Sample => "sample",
        }
    }

    /// Whether the dump is archival, judged by the rules that apply to this unit.
    pub fn is_complete(&self) -> bool {
        match self.unit {
            StateUnit::Sector => self.bad_sectors() == 0,
            StateUnit::Sample => self.interior_bad() == 0,
        }
    }
}

/// Read and summarise a `.state` file.
///
/// `unit` must come from [`detect_state_unit`] rather than a guess: the file is an
/// undifferentiated byte array and carries no indication of its own scale.
pub fn analyze_state_file_as(path: &Path, unit: StateUnit) -> std::io::Result<StateSummary> {
    let data = std::fs::read(path)?;
    let mut s = StateSummary {
        unit,
        total_sectors: data.len() as u64,
        ..Default::default()
    };

    let mut run_start: Option<u64> = None;
    for (i, &b) in data.iter().enumerate() {
        let lba = i as u64;
        let bad = b < STATE_FIRST_SUCCESS;
        match b {
            STATE_ERROR_SKIP => s.error_skip += 1,
            STATE_ERROR_C2 => s.error_c2 += 1,
            _ => {}
        }
        match (bad, run_start) {
            (true, None) => run_start = Some(lba),
            (false, Some(start)) => {
                s.bad_runs.push((start, lba - 1));
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run_start {
        s.bad_runs.push((start, s.total_sectors.saturating_sub(1)));
    }
    Ok(s)
}

/// Summarise a `.state` file, treating each byte as a sector.
///
/// Correct for DVD/BD only. Prefer [`analyze_state_file_as`] with a unit from
/// [`detect_state_unit`].
pub fn analyze_state_file(path: &Path) -> std::io::Result<StateSummary> {
    analyze_state_file_as(path, StateUnit::Sector)
}

/// Bytes of raw scrambled CD data described by one `.state` byte.
pub const SAMPLES_PER_STATE_BYTE: u64 = 4;

/// Work out what one `.state` byte means for this dump, by measurement.
///
/// A raw CD dump writes a `.scram` sidecar holding the whole scrambled stream, and the
/// `.state` file runs alongside it at one byte per 4-byte sample. That relationship is
/// checkable, so it is checked rather than inferred from the media type: if the sizes do
/// not line up exactly, the assumption does not hold and we fall back to per-sector,
/// which is the stricter reading.
pub fn detect_state_unit(state: &Path, scram: Option<&Path>) -> StateUnit {
    let (Some(scram), Ok(st)) = (scram, std::fs::metadata(state)) else {
        return StateUnit::Sector;
    };
    let Ok(sc) = std::fs::metadata(scram) else {
        return StateUnit::Sector;
    };
    if st.len() > 0 && sc.len() == st.len() * SAMPLES_PER_STATE_BYTE {
        StateUnit::Sample
    } else {
        StateUnit::Sector
    }
}

/// Drive parameters redumper used for this dump.
///
/// Worth recording: on raw CD reads the dump's correctness depends entirely on these,
/// and a wrong sector order makes every sector fail. Keeping them with the job means a
/// dump can be explained, reproduced, or distrusted later on evidence rather than memory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DriveConfig {
    /// Raw `configuration:` line as reported.
    pub summary: Option<String>,
    /// Sector order actually used, after any auto-detection.
    pub sector_order: Option<String>,
    /// True when the drive was absent from redumper's database and parameters were
    /// therefore assumed rather than known.
    pub generic: bool,
    /// True when the sector order was measured rather than assumed.
    pub auto_detected: bool,
}

/// Identification metadata redumper's INFO stage recovers from the image.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiscMetadata {
    pub serial: Option<String>,
    pub region: Option<String>,
    pub version: Option<String>,
    pub exe_date: Option<String>,
}

/// Result of a completed dump.
#[derive(Debug, Clone, Default)]
pub struct DumpOutcome {
    /// Files redumper produced, absolute paths.
    pub files: Vec<PathBuf>,
    /// Read errors redumper **encountered and corrected** during the run.
    ///
    /// These are corrections, not remaining damage: redumper's own log labels them
    /// "correction statistics". A non-zero value means the drive stumbled and redumper
    /// recovered by re-reading — the dump can still be perfect. The authoritative
    /// record of remaining damage is [`DumpOutcome::state`].
    pub corrections_scsi: u64,
    pub corrections_edc: u64,
    /// Sector count as reported by the drive.
    pub sectors: Option<u64>,
    /// Redump-style hashes, when the hash stage ran.
    pub hashes: Vec<RomHash>,
    /// Per-sector state, the authoritative integrity record.
    pub state: Option<StateSummary>,
    /// Metadata recovered by the INFO stage.
    pub metadata: DiscMetadata,
    /// Warnings worth showing the operator.
    pub warnings: Vec<String>,
    /// Drive parameters used, recorded as provenance.
    pub drive: DriveConfig,
    /// Samples at the very end of the last track filled with silence because the drive
    /// cannot overread into the lead-out (see [`is_leadout_overread_only`]).
    pub leadout_fill_samples: Option<u64>,
}

impl DumpOutcome {
    /// Whether every sector was read successfully.
    ///
    /// Judged from the `.state` file, never from the correction counters — an earlier
    /// version of this check used the counters and wrongly failed a flawless dump whose
    /// transient errors redumper had already recovered.
    pub fn is_clean(&self) -> bool {
        match &self.state {
            Some(s) => s.is_complete(),
            // Without a state file we cannot prove completeness, so do not claim it.
            None => false,
        }
    }
}

/// Parse a progress line such as:
/// `- [ 42%] LBA:  845632/2009888, errors: { SCSI: 0, EDC: 0 }`
///
/// Returns `None` for any other line.
pub fn parse_progress(line: &str) -> Option<DumpProgress> {
    // Strip the leading spinner character and whitespace.
    let line = line.trim().trim_start_matches(['-', '\\', '|', '/']).trim();
    if !line.starts_with('[') {
        return None;
    }

    let percent_end = line.find("%]")?;
    let percent: u32 = line[1..percent_end].trim().parse().ok()?;

    let lba_part = line.find("LBA:")? + 4;
    let rest = &line[lba_part..];
    let comma = rest.find(',')?;
    let (cur, total) = rest[..comma].split_once('/')?;

    let field = |key: &str| -> u64 {
        line.find(key)
            .and_then(|i| {
                let tail = &line[i + key.len()..];
                let end = tail.find(|c: char| !c.is_ascii_digit() && !c.is_whitespace())?;
                tail[..end].trim().parse().ok()
            })
            .unwrap_or(0)
    };

    Some(DumpProgress {
        percent,
        current_lba: cur.trim().parse().ok()?,
        total_lba: total.trim().parse().ok()?,
        scsi_errors: field("SCSI:"),
        edc_errors: field("EDC:"),
    })
}

/// Parse a DAT `<rom .../>` element emitted by the hash stage.
pub fn parse_rom_hash(line: &str) -> Option<RomHash> {
    let line = line.trim();
    if !line.starts_with("<rom ") {
        return None;
    }
    // Attributes are `key="value"`, so pull each by name rather than by position.
    let attr = |key: &str| -> Option<String> {
        let needle = format!("{key}=\"");
        let start = line.find(&needle)? + needle.len();
        let end = line[start..].find('"')? + start;
        Some(line[start..end].to_string())
    };

    Some(RomHash {
        name: attr("name")?,
        size: attr("size")?.parse().ok()?,
        crc32: attr("crc").unwrap_or_default(),
        md5: attr("md5").unwrap_or_default(),
        sha1: attr("sha1").unwrap_or_default(),
    })
}

/// Unreadable audio in one track, from a line like
/// `errors detected, track: 20, sectors: {SKIP: 2, C2: 0}, samples: {SKIP: 667, C2: 0}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackErrors {
    pub track: u32,
    pub skip_samples: u64,
    pub c2_samples: u64,
}

fn parse_track_errors(line: &str) -> Option<TrackErrors> {
    let rest = line.trim().strip_prefix("errors detected, track:")?;
    let track = rest.split(',').next()?.trim().parse().ok()?;
    let samples = &rest[rest.find("samples:")?..];
    let num = |key: &str| -> Option<u64> {
        let tail = &samples[samples.find(key)? + key.len()..];
        tail.trim()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    };
    Some(TrackErrors {
        track,
        skip_samples: num("SKIP:")?,
        c2_samples: num("C2:")?,
    })
}

/// A track number from redumper's TOC listing (`  track 20 { audio }`).
fn parse_toc_track(line: &str) -> Option<u32> {
    let rest = line.trim().strip_prefix("track ")?;
    rest.split_whitespace().next()?.parse().ok()
}

/// Whether redumper's refusal is only the lead-out overread a positive read offset
/// needs and this drive cannot do.
///
/// Correcting a +N offset means the final N samples of the last track must be read from
/// beyond the end of the audio, in the lead-out. Many drives cannot read there, so those
/// samples come back unreadable — always exactly at the very end, never more than N, and
/// with no C2 errors because nothing was misread. EAC and whipper fill them with silence;
/// AccurateRip excludes the last five sectors of a disc from its checksums for exactly
/// this reason. Anything else — errors on another track, any C2 error, more missing
/// samples than the offset accounts for — is real damage and is not excused.
pub fn is_leadout_overread_only(errors: &[TrackErrors], last_track: u32, read_offset: i32) -> bool {
    read_offset > 0
        && !errors.is_empty()
        && errors.iter().all(|e| {
            e.track == last_track
                && e.c2_samples == 0
                && e.skip_samples <= u64::from(read_offset.unsigned_abs())
        })
}

/// Extract the drive-reported sector count from a log line.
fn parse_sector_count(line: &str) -> Option<u64> {
    let line = line.trim();
    let prefix = "sectors count (READ_CAPACITY):";
    line.strip_prefix(prefix)?.trim().parse().ok()
}

/// Note drive configuration and any auto-detection result.
fn absorb_drive_config(line: &str, d: &mut DriveConfig) {
    let t = line.trim();
    if let Some(rest) = t.strip_prefix("configuration:") {
        d.summary = Some(rest.trim().to_string());
        // Extract the sector order from within the parenthesised summary.
        if let Some(i) = rest.find("sector order:") {
            let tail = &rest[i + "sector order:".len()..];
            let v: String = tail
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !v.is_empty() {
                d.sector_order = Some(v);
            }
        }
    }
    if t.contains("using generic drive") || t.contains("drive not found in the database") {
        d.generic = true;
    }
    // e.g. "GENERIC: auto-detected sector order: DATA_SUB"
    if let Some(i) = t.find("auto-detected sector order:") {
        let v = t[i + "auto-detected sector order:".len()..].trim().to_string();
        if !v.is_empty() {
            d.sector_order = Some(v);
            d.auto_detected = true;
        }
    }
}

/// Pull `key: value` metadata emitted by the INFO stage into `meta`.
fn absorb_metadata(line: &str, meta: &mut DiscMetadata) {
    let t = line.trim();
    let Some((k, v)) = t.split_once(':') else {
        return;
    };
    let v = v.trim().to_string();
    if v.is_empty() {
        return;
    }
    match k.trim() {
        "serial" => meta.serial = Some(v),
        "region" => meta.region = Some(v),
        "version" => meta.version = Some(v),
        "EXE date" => meta.exe_date = Some(v),
        _ => {}
    }
}

fn spawn(args: &[String]) -> Result<std::process::Child> {
    Command::new(TOOL)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                BackendError::NotInstalled { tool: TOOL }
            } else {
                BackendError::Io {
                    tool: TOOL,
                    source: e,
                }
            }
        })
}

/// Return redumper's version/build string.
pub fn version() -> Result<String> {
    let out = Command::new(TOOL).arg("--version").output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            BackendError::NotInstalled { tool: TOOL }
        } else {
            BackendError::Io {
                tool: TOOL,
                source: e,
            }
        }
    })?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: "no version output".into(),
        });
    }
    Ok(s)
}

/// Split a reader into lines, treating both `\n` and `\r` as terminators.
///
/// redumper redraws progress with carriage returns, so a plain line iterator would
/// buffer the entire dump's progress into one enormous "line".
fn read_cr_lines<R: Read>(r: R, mut on_line: impl FnMut(&str)) {
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Read up to the next carriage return...
        let n = match reader.read_until(b'\r', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let _ = n;
        // ...then split any embedded newlines within it.
        let chunk = String::from_utf8_lossy(&buf);
        for piece in chunk.split('\n') {
            let piece = piece.trim_end_matches('\r');
            if !piece.trim().is_empty() {
                on_line(piece);
            }
        }
    }
}

/// Dump a disc with redumper's aggregate `disc` command.
///
/// `image_name` is the basename for the produced files. Progress is reported through
/// `on_progress`; every output line is passed to `on_log`.
#[allow(clippy::too_many_arguments)]
pub fn dump(
    device: &str,
    image_path: &Path,
    image_name: &str,
    sector_order: Option<&str>,
    read_offset: Option<i32>,
    mut on_progress: impl FnMut(&DumpProgress),
    mut on_log: impl FnMut(&str),
    mut on_warning: impl FnMut(&str),
) -> Result<DumpOutcome> {
    let mut args = vec![
        "disc".to_string(),
        format!("--drive={device}"),
        format!("--image-path={}", image_path.display()),
        format!("--image-name={image_name}"),
        // Retry sectors that error, rather than accepting the first failure. Archival
        // dumps are worth the extra time.
        "--retries=8".to_string(),
    ];
    match sector_order {
        // An explicit override wins: used when detection is known to be wrong.
        Some(o) => args.push(format!("--drive-sector-order={o}")),
        // Otherwise measure rather than assume. Raw CD reads depend on this being
        // right, and redumper's fallback for a drive missing from its database is a
        // guess that silently fails every sector when wrong. Detection costs ~0s.
        None => args.push("--auto-detect".to_string()),
    }
    if let Some(o) = read_offset {
        args.push(format!("--drive-read-offset={o}"));
    }

    let mut child = spawn(&args)?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    let mut outcome = DumpOutcome::default();
    let mut last = DumpProgress::default();
    let mut track_errors: Vec<TrackErrors> = Vec::new();
    let mut last_track: u32 = 0;

    read_cr_lines(stdout, |line| {
        on_log(line);
        if let Some(e) = parse_track_errors(line) {
            track_errors.push(e);
        }
        if let Some(t) = parse_toc_track(line) {
            last_track = last_track.max(t);
        }
        if let Some(p) = parse_progress(line) {
            last = p.clone();
            on_progress(&p);
            return;
        }
        if let Some(h) = parse_rom_hash(line) {
            outcome.hashes.push(h);
            return;
        }
        if let Some(n) = parse_sector_count(line) {
            outcome.sectors = Some(n);
            return;
        }
        absorb_metadata(line, &mut outcome.metadata);
        absorb_drive_config(line, &mut outcome.drive);
        let t = line.trim();
        if t.starts_with("warning:") || t.starts_with("error:") {
            outcome.warnings.push(t.to_string());
            // Surface immediately. A warning that the drive is unknown decides whether
            // the dump can work at all, and holding it until the end is useless when the
            // run would otherwise grind for hours producing nothing.
            on_warning(t);
        }
    });

    // Drain stderr so a failure message is not lost.
    let mut err_text = String::new();
    let _ = BufReader::new(stderr).read_to_string(&mut err_text);
    for l in err_text.lines().filter(|l| !l.trim().is_empty()) {
        on_log(l);
        outcome.warnings.push(l.trim().to_string());
    }

    let status = child.wait().map_err(|e| BackendError::Io {
        tool: TOOL,
        source: e,
    })?;

    let overread_only = read_offset
        .map(|o| is_leadout_overread_only(&track_errors, last_track, o))
        .unwrap_or(false);
    if !status.success() && overread_only {
        // The read itself is complete; only the split refused. Redo just the split,
        // letting redumper fill the unreachable lead-out samples with silence.
        let filled: u64 = track_errors.iter().map(|e| e.skip_samples).sum();
        let mut split_args = vec![
            "split".to_string(),
            format!("--image-path={}", image_path.display()),
            format!("--image-name={image_name}"),
            "--force-split".to_string(),
        ];
        if let Some(o) = read_offset {
            split_args.push(format!("--drive-read-offset={o}"));
        }
        let split = spawn(&split_args)?
            .wait_with_output()
            .map_err(|e| BackendError::Io { tool: TOOL, source: e })?;
        for l in String::from_utf8_lossy(&split.stdout).lines() {
            on_log(l);
        }
        if !split.status.success() {
            return Err(BackendError::Failed {
                tool: TOOL,
                detail: format!("split after a lead-out overread failed: {}", split.status),
            });
        }
        let note = format!(
            "filled the last {filled} sample(s) of track {last_track} with silence: \
             the drive cannot read the lead-out that its +{} read offset reaches into \
             (AccurateRip excludes these samples from its checksums)",
            read_offset.unwrap_or(0)
        );
        on_warning(&note);
        // redumper's refusal has been dealt with; repeating it in the summary would read
        // as damage that is not there.
        outcome
            .warnings
            .retain(|w| !w.contains("data errors detected"));
        outcome.warnings.push(note);
        outcome.leadout_fill_samples = Some(filled);
    } else if !status.success() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: format!(
                "exited with {status}{}",
                if err_text.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", err_text.trim())
                }
            ),
        });
    }

    // These are corrections applied, not damage remaining (see field docs).
    outcome.corrections_scsi = last.scsi_errors;
    outcome.corrections_edc = last.edc_errors;

    // Collect what was produced.
    if let Ok(entries) = std::fs::read_dir(image_path) {
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(image_name))
                    .unwrap_or(false)
            })
            .collect();
        files.sort();
        outcome.files = files;
    }

    if outcome.files.is_empty() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: "redumper reported success but produced no files".into(),
        });
    }

    // The .state sidecar is the authoritative integrity record.
    if let Some(state_path) = outcome
        .files
        .iter()
        .find(|p| p.extension().map(|e| e == "state").unwrap_or(false))
        .cloned()
    {
        let scram = outcome
            .files
            .iter()
            .find(|p| p.extension().map(|e| e == "scram").unwrap_or(false))
            .cloned();
        let unit = detect_state_unit(&state_path, scram.as_deref());
        match analyze_state_file_as(&state_path, unit) {
            Ok(s) => outcome.state = Some(s),
            Err(e) => outcome
                .warnings
                .push(format!("could not read {}: {e}", state_path.display())),
        }
    }

    Ok(outcome)
}

/// Bytes per sector in a raw CD track image.
pub const CD_RAW_SECTOR_BYTES: u64 = 2352;

/// What the emitted image should measure, according to a source other than the image.
///
/// A truncated image that otherwise looks fine is the failure mode worth catching: it
/// hashes cleanly and passes every later check while missing data. Catching it needs an
/// expected length from somewhere independent of the file itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedGeometry {
    /// DVD/BD: one `.iso` of `sectors * 2048` bytes, per the drive's READ CAPACITY.
    Iso,
    /// CD: `.bin` tracks that tile the disc from LBA 0 to the lead-out, so their sizes
    /// sum to `leadout_lba * 2352`. The lead-out comes from the TOC we read ourselves,
    /// which makes this a genuine cross-check of redumper's output rather than a
    /// restatement of it.
    CdTracks { leadout_lba: u64 },
}

/// Verify the dumped image is the size an independent source says it should be.
pub fn verify_image_size(
    outcome: &DumpOutcome,
    expected: ExpectedGeometry,
) -> std::result::Result<(), String> {
    verify_files_size(&outcome.files, outcome.sectors, expected)
}

/// As [`verify_image_size`], but over a bare file list.
///
/// Lets the standalone verifier re-run exactly the gate the rip ran, months later, from
/// the manifest alone — the gate is only worth anything if it is the same gate.
pub fn verify_files_size(
    files: &[PathBuf],
    sectors: Option<u64>,
    expected: ExpectedGeometry,
) -> std::result::Result<(), String> {
    match expected {
        ExpectedGeometry::Iso => verify_iso_size(files, sectors),
        ExpectedGeometry::CdTracks { leadout_lba } => verify_cd_tracks(files, leadout_lba),
    }
}

fn verify_iso_size(files: &[PathBuf], sectors: Option<u64>) -> std::result::Result<(), String> {
    let Some(sectors) = sectors else {
        return Err("drive did not report a sector count; cannot verify image length".into());
    };
    let expected = sectors * 2048;

    let Some(iso) = files
        .iter()
        .find(|p| p.extension().map(|e| e == "iso").unwrap_or(false))
    else {
        return Err("no .iso produced".into());
    };

    let actual = std::fs::metadata(iso).map(|m| m.len()).unwrap_or(0);
    if actual != expected {
        return Err(format!(
            "{} is {actual} bytes but the drive reported {sectors} sectors ({expected} bytes)",
            iso.display()
        ));
    }
    Ok(())
}

fn verify_cd_tracks(files: &[PathBuf], leadout_lba: u64) -> std::result::Result<(), String> {
    if leadout_lba == 0 {
        return Err("no TOC lead-out was read; cannot verify track lengths".into());
    }
    let bins: Vec<&PathBuf> = files
        .iter()
        .filter(|p| p.extension().map(|e| e == "bin").unwrap_or(false))
        .collect();
    if bins.is_empty() {
        return Err("no .bin track produced".into());
    }

    let mut total = 0u64;
    for b in &bins {
        let len = std::fs::metadata(b).map(|m| m.len()).unwrap_or(0);
        if len % CD_RAW_SECTOR_BYTES != 0 {
            return Err(format!(
                "{} is {len} bytes, not a whole number of {CD_RAW_SECTOR_BYTES}-byte sectors",
                b.display()
            ));
        }
        total += len;
    }

    let expected = leadout_lba * CD_RAW_SECTOR_BYTES;
    if total != expected {
        return Err(format!(
            "{} track(s) total {total} bytes ({} sectors) but the TOC puts the lead-out at \
             LBA {leadout_lba} ({expected} bytes)",
            bins.len(),
            total / CD_RAW_SECTOR_BYTES
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_redumpers_per_track_error_line() {
        let e = parse_track_errors(
            "errors detected, track: 20, sectors: {SKIP: 2, C2: 0}, samples: {SKIP: 667, C2: 0}",
        )
        .unwrap();
        assert_eq!(e, TrackErrors { track: 20, skip_samples: 667, c2_samples: 0 });
        assert_eq!(parse_toc_track("  track 20 { audio }"), Some(20));
        assert_eq!(parse_toc_track("  track AA { audio }"), None);
    }

    /// The real case: a +667 drive that cannot overread, on a 20-track disc.
    #[test]
    fn only_a_lead_out_overread_is_excused() {
        let e = |track, skip, c2| TrackErrors { track, skip_samples: skip, c2_samples: c2 };
        assert!(is_leadout_overread_only(&[e(20, 667, 0)], 20, 667));
        // Damage elsewhere, misreads, or more missing than the offset explains: refused.
        assert!(!is_leadout_overread_only(&[e(19, 667, 0)], 20, 667));
        assert!(!is_leadout_overread_only(&[e(20, 667, 1)], 20, 667));
        assert!(!is_leadout_overread_only(&[e(20, 668, 0)], 20, 667));
        assert!(!is_leadout_overread_only(&[e(20, 667, 0), e(3, 1, 0)], 20, 667));
        // A negative offset reads into the lead-in, not the lead-out; no errors, no excuse.
        assert!(!is_leadout_overread_only(&[e(20, 600, 0)], 20, -600));
        assert!(!is_leadout_overread_only(&[], 20, 667));
    }

    /// Captured verbatim from a real redumper run on a PS2 disc.
    #[test]
    fn parses_real_progress_line() {
        let p = parse_progress("- [  0%] LBA:       0/2009888, errors: { SCSI: 0, EDC: 0 }")
            .expect("parsed");
        assert_eq!(p.percent, 0);
        assert_eq!(p.current_lba, 0);
        assert_eq!(p.total_lba, 2_009_888);
        assert_eq!(p.scsi_errors, 0);
        assert_eq!(p.edc_errors, 0);
    }

    #[test]
    fn parses_progress_with_errors_and_spinners() {
        for spinner in ["-", "\\", "|", "/"] {
            let line =
                format!("{spinner} [ 42%] LBA:  845632/2009888, errors: {{ SCSI: 3, EDC: 17 }}");
            let p = parse_progress(&line).expect("parsed");
            assert_eq!(p.percent, 42);
            assert_eq!(p.current_lba, 845_632);
            assert_eq!(p.scsi_errors, 3, "line: {line}");
            assert_eq!(p.edc_errors, 17);
        }
    }

    #[test]
    fn ignores_non_progress_lines() {
        assert!(parse_progress("redumper (build: b744)").is_none());
        assert!(parse_progress("  book type: DVD-ROM").is_none());
        assert!(parse_progress("").is_none());
        assert!(parse_progress("*** DUMP (time check: 0s)").is_none());
    }

    #[test]
    fn parses_rom_hash_entry() {
        let line = r#"<rom name="game.iso" size="4116250624" crc="1a2b3c4d" md5="0123456789abcdef0123456789abcdef" sha1="da39a3ee5e6b4b0d3255bfef95601890afd80709" />"#;
        let h = parse_rom_hash(line).expect("parsed");
        assert_eq!(h.name, "game.iso");
        assert_eq!(h.size, 4_116_250_624);
        assert_eq!(h.crc32, "1a2b3c4d");
        assert_eq!(h.sha1, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn ignores_non_rom_lines() {
        assert!(parse_rom_hash("<game name=\"x\">").is_none());
        assert!(parse_rom_hash("not xml at all").is_none());
    }

    #[test]
    fn parses_sector_count() {
        assert_eq!(
            parse_sector_count("sectors count (READ_CAPACITY): 2009888"),
            Some(2_009_888)
        );
        assert_eq!(parse_sector_count("sectors count (PHYSICAL): 2009888"), None);
    }

    /// Corrections are not damage.
    ///
    /// Regression test for a real false positive: a flawless PS2 dump reported
    /// "correction statistics: SCSI 96" — transient errors redumper encountered and
    /// recovered — and an earlier version of this check failed the dump on that basis
    /// even though every sector in the .state file was SUCCESS.
    #[test]
    fn corrections_do_not_make_a_dump_unclean() {
        let o = DumpOutcome {
            corrections_scsi: 96,
            corrections_edc: 0,
            state: Some(StateSummary {
                unit: StateUnit::Sector,
                total_sectors: 2_009_888,
                error_skip: 0,
                error_c2: 0,
                bad_runs: vec![],
            }),
            ..Default::default()
        };
        assert!(o.is_clean(), "recovered errors must not fail a complete dump");
    }

    #[test]
    fn unreadable_sectors_make_a_dump_unclean() {
        let o = DumpOutcome {
            state: Some(StateSummary {
                unit: StateUnit::Sector,
                total_sectors: 1000,
                error_skip: 3,
                error_c2: 0,
                bad_runs: vec![(500, 502)],
            }),
            ..Default::default()
        };
        assert!(!o.is_clean());
    }

    /// Without a state file we cannot prove completeness, so we must not claim it.
    #[test]
    fn missing_state_file_is_not_clean() {
        let o = DumpOutcome::default();
        assert!(!o.is_clean());
    }

    #[test]
    fn state_file_analysis_counts_and_groups_bad_sectors() {
        use std::io::Write;
        let mut p = std::env::temp_dir();
        p.push(format!("dumo-state-test-{}", std::process::id()));

        // 10 sectors: two separate bad runs at 2..3 and 7..7.
        let states: Vec<u8> = vec![4, 4, 0, 1, 4, 4, 4, 0, 4, 4];
        std::fs::File::create(&p).unwrap().write_all(&states).unwrap();

        let s = analyze_state_file(&p).unwrap();
        assert_eq!(s.total_sectors, 10);
        assert_eq!(s.error_skip, 2);
        assert_eq!(s.error_c2, 1);
        assert_eq!(s.bad_sectors(), 3);
        assert_eq!(s.bad_runs, vec![(2, 3), (7, 7)]);
        assert!(!s.is_complete());
        std::fs::remove_file(p).ok();
    }

    /// State 2 and 3 are success variants, not errors.
    #[test]
    fn success_variants_are_not_errors() {
        use std::io::Write;
        let mut p = std::env::temp_dir();
        p.push(format!("dumo-state-ok-{}", std::process::id()));
        std::fs::File::create(&p)
            .unwrap()
            .write_all(&[2u8, 3, 4, 4])
            .unwrap();
        let s = analyze_state_file(&p).unwrap();
        assert_eq!(s.bad_sectors(), 0);
        assert!(s.is_complete());
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn trailing_bad_run_is_closed() {
        use std::io::Write;
        let mut p = std::env::temp_dir();
        p.push(format!("dumo-state-tail-{}", std::process::id()));
        std::fs::File::create(&p)
            .unwrap()
            .write_all(&[4u8, 4, 0, 0])
            .unwrap();
        let s = analyze_state_file(&p).unwrap();
        assert_eq!(s.bad_runs, vec![(2, 3)]);
        std::fs::remove_file(p).ok();
    }

    /// Captured from the real PS2 dump's INFO stage.
    #[test]
    fn absorbs_info_metadata() {
        let mut m = DiscMetadata::default();
        for l in [
            "  EXE: SLUS_205.78",
            "  EXE date: 2002-09-16",
            "  version: 1.00",
            "  serial: SLUS-20578",
            "  region: USA",
        ] {
            absorb_metadata(l, &mut m);
        }
        assert_eq!(m.serial.as_deref(), Some("SLUS-20578"));
        assert_eq!(m.region.as_deref(), Some("USA"));
        assert_eq!(m.version.as_deref(), Some("1.00"));
        assert_eq!(m.exe_date.as_deref(), Some("2002-09-16"));
    }

    #[test]
    fn size_verification_requires_a_sector_count() {
        let o = DumpOutcome::default();
        assert!(verify_image_size(&o, ExpectedGeometry::Iso).is_err());
    }

    #[test]
    fn cd_size_verification_requires_a_leadout() {
        let o = DumpOutcome::default();
        assert!(verify_image_size(&o, ExpectedGeometry::CdTracks { leadout_lba: 0 }).is_err());
    }

    /// A CD `.state` byte is a 4-byte sample, not a sector, and the file covers the
    /// lead-in and lead-out as well as the tracks.
    ///
    /// Regression test for a real false negative: a flawless PS2 CD dump — redumper
    /// reported `SCSI: 0, C2: 0` and `REDUMP.INFO errors: 0`, and every sector of the
    /// emitted .bin was intact — was rejected as having "26547533 unreadable sectors at
    /// LBA 0". Those were the lead-in samples, which this drive cannot reach and which
    /// are not part of a Redump-conformant dump.
    #[test]
    fn cd_lead_in_gap_does_not_make_a_dump_unclean() {
        // The real shape: one contiguous bad run at the start, nothing after it.
        let lead_in = 26_547_533u64;
        let track = 98_584_080u64;
        let s = StateSummary {
            unit: StateUnit::Sample,
            total_sectors: lead_in + track,
            error_skip: lead_in,
            error_c2: 0,
            bad_runs: vec![(0, lead_in - 1)],
        };
        assert_eq!(s.edge_bad(), lead_in);
        assert_eq!(s.interior_bad(), 0);
        assert!(s.is_complete(), "unreachable lead-in is not damage");

        // Read as sectors — the old behaviour — the very same file is a failure.
        let as_sectors = StateSummary {
            unit: StateUnit::Sector,
            ..s
        };
        assert!(!as_sectors.is_complete());
    }

    /// Edge tolerance must not become a licence to lose track data.
    #[test]
    fn cd_interior_gap_still_makes_a_dump_unclean() {
        let s = StateSummary {
            unit: StateUnit::Sample,
            total_sectors: 1000,
            error_skip: 110,
            error_c2: 0,
            bad_runs: vec![(0, 99), (500, 509)],
        };
        assert_eq!(s.edge_bad(), 100);
        assert_eq!(s.interior_bad(), 10);
        assert!(!s.is_complete());
    }

    /// The sample/sector distinction is measured against the .scram sidecar, never
    /// guessed: the .state file is an undifferentiated byte array either way.
    #[test]
    fn state_unit_is_detected_from_the_scram_sidecar() {
        let dir = std::env::temp_dir().join(format!("dumo-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("g.state");
        let scram = dir.join("g.scram");
        std::fs::write(&state, vec![4u8; 100]).unwrap();

        // No scram at all: a DVD dump, one byte per sector.
        assert_eq!(detect_state_unit(&state, None), StateUnit::Sector);

        // Scram exactly 4x the state file: a raw CD dump.
        std::fs::write(&scram, vec![0u8; 400]).unwrap();
        assert_eq!(detect_state_unit(&state, Some(&scram)), StateUnit::Sample);

        // Sizes that do not line up mean the assumption does not hold; fall back to the
        // stricter reading rather than tolerating gaps we cannot account for.
        std::fs::write(&scram, vec![0u8; 399]).unwrap();
        assert_eq!(detect_state_unit(&state, Some(&scram)), StateUnit::Sector);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cr_lines_splits_carriage_returns() {
        let data = "first\rsecond\rthird\nfourth\r";
        let mut seen = Vec::new();
        read_cr_lines(data.as_bytes(), |l| seen.push(l.to_string()));
        assert_eq!(seen, vec!["first", "second", "third", "fourth"]);
    }
}
