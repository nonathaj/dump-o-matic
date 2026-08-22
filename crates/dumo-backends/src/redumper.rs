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

/// Summary of the `.state` file: the authoritative record of which sectors are good.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateSummary {
    pub total_sectors: u64,
    pub error_skip: u64,
    pub error_c2: u64,
    /// Contiguous runs of unreadable sectors, as inclusive `(first, last)` LBAs.
    pub bad_runs: Vec<(u64, u64)>,
}

impl StateSummary {
    pub fn bad_sectors(&self) -> u64 {
        self.error_skip + self.error_c2
    }

    pub fn is_complete(&self) -> bool {
        self.bad_sectors() == 0
    }
}

/// Read and summarise a `.state` file.
pub fn analyze_state_file(path: &Path) -> std::io::Result<StateSummary> {
    let data = std::fs::read(path)?;
    let mut s = StateSummary {
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

/// Extract the drive-reported sector count from a log line.
fn parse_sector_count(line: &str) -> Option<u64> {
    let line = line.trim();
    let prefix = "sectors count (READ_CAPACITY):";
    line.strip_prefix(prefix)?.trim().parse().ok()
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
pub fn dump(
    device: &str,
    image_path: &Path,
    image_name: &str,
    mut on_progress: impl FnMut(&DumpProgress),
    mut on_log: impl FnMut(&str),
) -> Result<DumpOutcome> {
    let args = vec![
        "disc".to_string(),
        format!("--drive={device}"),
        format!("--image-path={}", image_path.display()),
        format!("--image-name={image_name}"),
        // Retry sectors that error, rather than accepting the first failure. Archival
        // dumps are worth the extra time.
        "--retries=8".to_string(),
    ];

    let mut child = spawn(&args)?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    let mut outcome = DumpOutcome::default();
    let mut last = DumpProgress::default();

    read_cr_lines(stdout, |line| {
        on_log(line);
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
        let t = line.trim();
        if t.starts_with("warning:") || t.starts_with("error:") {
            outcome.warnings.push(t.to_string());
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

    if !status.success() {
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
        match analyze_state_file(&state_path) {
            Ok(s) => outcome.state = Some(s),
            Err(e) => outcome
                .warnings
                .push(format!("could not read {}: {e}", state_path.display())),
        }
    }

    Ok(outcome)
}

/// Verify the dumped image is the size the drive said it should be.
///
/// A truncated image that otherwise looks fine is the failure mode worth catching here:
/// it would hash cleanly and pass every later check while missing data.
pub fn verify_image_size(outcome: &DumpOutcome) -> std::result::Result<(), String> {
    let Some(sectors) = outcome.sectors else {
        return Err("drive did not report a sector count; cannot verify image length".into());
    };
    let expected = sectors * 2048;

    let Some(iso) = outcome
        .files
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(verify_image_size(&o).is_err());
    }

    #[test]
    fn cr_lines_splits_carriage_returns() {
        let data = "first\rsecond\rthird\nfourth\r";
        let mut seen = Vec::new();
        read_cr_lines(data.as_bytes(), |l| seen.push(l.to_string()));
        assert_eq!(seen, vec!["first", "second", "third", "fourth"]);
    }
}
