//! MakeMKV adapter, driving `makemkvcon` in robot mode (`-r`).
//!
//! Two things about `makemkvcon` shape this module:
//!
//! - **Its exit code cannot be trusted.** It exits 0 even after printing
//!   `Failed to open disc`. Success is therefore determined by parsing messages, never
//!   by the process status. Getting this wrong would mean a failed rip recorded as a
//!   success, which is precisely the kind of mistake that later licenses a delete.
//! - **Robot mode is a stable, parseable CSV**, unlike its human output, so we always
//!   pass `-r` and parse structured records rather than scraping prose.

use crate::{BackendError, Result};
use dumo_core::TitleInfo;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

const TOOL: &str = "makemkvcon";

/// Message code emitted on successful completion of an operation.
const MSG_OPERATION_COMPLETE: u32 = 5011;
/// Message code emitted when the disc cannot be opened.
const MSG_FAILED_TO_OPEN: u32 = 5010;
/// Message code for a completed backup/copy.
const MSG_COPY_COMPLETE: u32 = 5036;
/// Message codes that indicate outright failure.
const FAILURE_CODES: &[u32] = &[MSG_FAILED_TO_OPEN, 5003, 5004];

/// A parsed line of robot-mode output.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// `MSG:code,flags,count,message,format,params...`
    Message { code: u32, text: String },
    /// `DRV:index,visible,enabled,flags,name,label,device`
    Drive {
        index: u32,
        name: String,
        label: String,
        device: String,
    },
    /// `TCOUNT:n`
    TitleCount(u32),
    /// `TINFO:title,attr,code,value`
    TitleInfoAttr { title: u32, attr: u32, value: String },
    /// `CINFO:attr,code,value`
    DiscInfoAttr { attr: u32, value: String },
    /// `PRGV:current,total,max`
    ProgressValue { current: u64, total: u64, max: u64 },
    /// `PRGC:code,id,name` — current operation.
    ProgressCurrent { name: String },
    /// Anything we do not model.
    Other,
}

/// Split a robot-mode value list, honouring quoted strings with `""` escapes.
///
/// A naive `split(',')` breaks on the many messages that contain commas inside quotes.
fn split_values(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                // A doubled quote inside a quoted field is a literal quote.
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            '"' => in_quotes = true,
            ',' if !in_quotes => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// Parse one line of robot-mode output.
pub fn parse_line(line: &str) -> Record {
    let Some((tag, rest)) = line.split_once(':') else {
        return Record::Other;
    };
    let v = split_values(rest);
    let num = |i: usize| -> Option<u64> { v.get(i)?.trim().parse().ok() };

    match tag {
        "MSG" => Record::Message {
            code: num(0).unwrap_or(0) as u32,
            text: v.get(3).cloned().unwrap_or_default(),
        },
        "DRV" => Record::Drive {
            index: num(0).unwrap_or(0) as u32,
            name: v.get(4).cloned().unwrap_or_default(),
            label: v.get(5).cloned().unwrap_or_default(),
            device: v.get(6).cloned().unwrap_or_default(),
        },
        "TCOUNT" => Record::TitleCount(num(0).unwrap_or(0) as u32),
        "TINFO" => Record::TitleInfoAttr {
            title: num(0).unwrap_or(0) as u32,
            attr: num(1).unwrap_or(0) as u32,
            value: v.get(3).cloned().unwrap_or_default(),
        },
        "CINFO" => Record::DiscInfoAttr {
            attr: num(0).unwrap_or(0) as u32,
            value: v.get(2).cloned().unwrap_or_default(),
        },
        "PRGV" => Record::ProgressValue {
            current: num(0).unwrap_or(0),
            total: num(1).unwrap_or(0),
            max: num(2).unwrap_or(0),
        },
        "PRGC" => Record::ProgressCurrent {
            name: v.get(2).cloned().unwrap_or_default(),
        },
        _ => Record::Other,
    }
}

// MakeMKV title attribute ids we care about.
const ATTR_CHAPTER_COUNT: u32 = 8;
const ATTR_DURATION: u32 = 9;
const ATTR_BYTES: u32 = 11;
const ATTR_OUTPUT_NAME: u32 = 27;

/// Parse `H:MM:SS` or `MM:SS` into seconds.
fn parse_duration(s: &str) -> u64 {
    let parts: Vec<u64> = s.split(':').filter_map(|p| p.trim().parse().ok()).collect();
    match parts.len() {
        3 => parts[0] * 3600 + parts[1] * 60 + parts[2],
        2 => parts[0] * 60 + parts[1],
        1 => parts[0],
        _ => 0,
    }
}

/// What a disc scan found.
#[derive(Debug, Clone, Default)]
pub struct DiscScan {
    pub disc_name: Option<String>,
    pub titles: Vec<TitleInfo>,
    /// Messages worth surfacing to the operator.
    pub messages: Vec<String>,
}

/// Build titles from a stream of robot-mode lines.
///
/// Separated from process handling so it can be tested against captured output.
pub fn parse_scan<I: IntoIterator<Item = String>>(lines: I) -> Result<DiscScan> {
    use std::collections::BTreeMap;

    let mut attrs: BTreeMap<u32, BTreeMap<u32, String>> = BTreeMap::new();
    let mut scan = DiscScan::default();
    let mut saw_success = false;
    let mut failure: Option<String> = None;

    for line in lines {
        match parse_line(&line) {
            Record::Message { code, text } => {
                if code == MSG_OPERATION_COMPLETE {
                    saw_success = true;
                }
                if FAILURE_CODES.contains(&code) {
                    failure = Some(text.clone());
                }
                scan.messages.push(text);
            }
            Record::DiscInfoAttr { attr, value } if attr == 2 => {
                scan.disc_name = Some(value);
            }
            Record::TitleInfoAttr { title, attr, value } => {
                attrs.entry(title).or_default().insert(attr, value);
            }
            _ => {}
        }
    }

    if let Some(detail) = failure {
        return Err(BackendError::Reported {
            tool: TOOL,
            message: detail,
        });
    }
    // Exit status is unreliable, so an explicit success message is required.
    if !saw_success {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: "scan did not report successful completion".into(),
        });
    }

    for (index, a) in attrs {
        let get = |k: u32| a.get(&k).cloned().unwrap_or_default();
        scan.titles.push(TitleInfo {
            index,
            duration_secs: parse_duration(&get(ATTR_DURATION)),
            estimated_bytes: get(ATTR_BYTES).trim().parse().unwrap_or(0),
            chapters: get(ATTR_CHAPTER_COUNT).trim().parse().unwrap_or(0),
            output_name: get(ATTR_OUTPUT_NAME),
        });
    }

    Ok(scan)
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

/// Check that the backend is available, returning its version string.
pub fn version() -> Result<String> {
    let mut child = spawn(&["-r".into(), "info".into(), "disc:-1".into()])?;
    let stdout = child.stdout.take().expect("piped");
    let mut version = None;
    for line in BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
        if let Record::Message { code: 1005, text } = parse_line(&line) {
            version = Some(text.replace(" started", ""));
            break;
        }
    }
    let _ = child.wait();
    version.ok_or(BackendError::Failed {
        tool: TOOL,
        detail: "could not determine version".into(),
    })
}

/// Scan a disc for titles.
///
/// `disc_index` is MakeMKV's own drive index, not a device path.
pub fn scan(disc_index: u32, min_length_secs: u32) -> Result<DiscScan> {
    let args = vec![
        "-r".to_string(),
        "--cache=1".to_string(),
        format!("--minlength={min_length_secs}"),
        "info".to_string(),
        format!("disc:{disc_index}"),
    ];
    let mut child = spawn(&args)?;
    let stdout = child.stdout.take().expect("piped");
    let lines: Vec<String> = BufReader::new(stdout)
        .lines()
        .map_while(std::result::Result::ok)
        .collect();
    let _ = child.wait();
    parse_scan(lines)
}

/// Find MakeMKV's drive index for a device path.
///
/// MakeMKV addresses drives by its own index, which need not match `/dev/srN`.
pub fn find_disc_index(device: &str) -> Result<u32> {
    let mut child = spawn(&["-r".into(), "info".into(), "disc:-1".into()])?;
    let stdout = child.stdout.take().expect("piped");
    let mut found = None;
    for line in BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
        if let Record::Drive { index, device: d, .. } = parse_line(&line) {
            if d == device {
                found = Some(index);
            }
        }
    }
    let _ = child.wait();
    found.ok_or_else(|| BackendError::Failed {
        tool: TOOL,
        detail: format!("no MakeMKV drive matches {device}"),
    })
}

/// Progress during a rip.
#[derive(Debug, Clone)]
pub struct RipProgress {
    /// Current operation description.
    pub operation: String,
    /// Fraction complete in `0.0..=1.0`, if known.
    pub fraction: Option<f64>,
}

/// Rip selected titles to `out_dir`.
///
/// `titles` selects title indices; an empty slice means all titles. Progress is reported
/// through `on_progress`. Every output line is passed to `on_log` so the caller can
/// retain the backend's full log for troubleshooting.
pub fn rip(
    disc_index: u32,
    titles: &[u32],
    out_dir: &Path,
    min_length_secs: u32,
    mut on_progress: impl FnMut(RipProgress),
    mut on_log: impl FnMut(&str),
) -> Result<Vec<String>> {
    let selector = match titles {
        [] => "all".to_string(),
        [one] => one.to_string(),
        // MakeMKV's CLI takes a single title or "all"; multiple selections are handled
        // by the caller invoking once per title.
        _ => {
            return Err(BackendError::Failed {
                tool: TOOL,
                detail: "rip() accepts one title or all; call once per title".into(),
            })
        }
    };

    let args = vec![
        "-r".to_string(),
        "--progress=-same".to_string(),
        format!("--minlength={min_length_secs}"),
        "mkv".to_string(),
        format!("disc:{disc_index}"),
        selector,
        out_dir.to_string_lossy().to_string(),
    ];

    let mut child = spawn(&args)?;
    let stdout = child.stdout.take().expect("piped");

    let mut operation = String::new();
    let mut saw_success = false;
    let mut failure: Option<String> = None;
    let mut messages: Vec<String> = Vec::new();

    for line in BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
        on_log(&line);
        match parse_line(&line) {
            Record::ProgressCurrent { name } => {
                operation = name;
                on_progress(RipProgress {
                    operation: operation.clone(),
                    fraction: None,
                });
            }
            Record::ProgressValue { current, max, .. } => {
                let fraction = if max > 0 {
                    Some((current as f64 / max as f64).clamp(0.0, 1.0))
                } else {
                    None
                };
                on_progress(RipProgress {
                    operation: operation.clone(),
                    fraction,
                });
            }
            Record::Message { code, text } => {
                if code == MSG_OPERATION_COMPLETE || code == MSG_COPY_COMPLETE {
                    saw_success = true;
                }
                if FAILURE_CODES.contains(&code) {
                    failure = Some(text.clone());
                }
                messages.push(text);
            }
            _ => {}
        }
    }

    let status = child.wait().map_err(|e| BackendError::Io {
        tool: TOOL,
        source: e,
    })?;

    if let Some(detail) = failure {
        return Err(BackendError::Reported {
            tool: TOOL,
            message: detail,
        });
    }
    // Deliberately belt-and-braces: a non-zero status is a failure even without a
    // failure message, and a zero status is *not* success without one.
    if !status.success() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: format!("exited with {status}"),
        });
    }
    if !saw_success {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: "rip did not report successful completion".into(),
        });
    }

    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_plain_values() {
        assert_eq!(split_values("1,2,3"), vec!["1", "2", "3"]);
    }

    /// MakeMKV messages routinely contain commas inside quotes.
    #[test]
    fn splits_quoted_values_containing_commas() {
        let v = split_values(r#"3028,16777216,3,"Title #5 was added (4 cell(s), 0:51:06)""#);
        assert_eq!(v.len(), 4);
        assert_eq!(v[3], "Title #5 was added (4 cell(s), 0:51:06)");
    }

    #[test]
    fn handles_escaped_quotes() {
        let v = split_values(r#"1,"he said ""hi""","x""#);
        assert_eq!(v[1], r#"he said "hi""#);
    }

    #[test]
    fn parses_drive_record() {
        let line = r#"DRV:0,2,999,1,"BD-RE PIONEER BD-RW  BDR-XD07U 1.03","ESPN_30_FOR_30_DISC_1","/dev/sr0""#;
        match parse_line(line) {
            Record::Drive {
                index,
                label,
                device,
                ..
            } => {
                assert_eq!(index, 0);
                assert_eq!(label, "ESPN_30_FOR_30_DISC_1");
                assert_eq!(device, "/dev/sr0");
            }
            other => panic!("expected drive, got {other:?}"),
        }
    }

    #[test]
    fn parses_progress_value() {
        match parse_line("PRGV:16384,32768,65536") {
            Record::ProgressValue { current, total, max } => {
                assert_eq!((current, total, max), (16384, 32768, 65536));
            }
            other => panic!("expected progress, got {other:?}"),
        }
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("0:51:06"), 3066);
        assert_eq!(parse_duration("51:06"), 3066);
        assert_eq!(parse_duration("1:00:00"), 3600);
        assert_eq!(parse_duration(""), 0);
    }

    /// Captured from a real scan of the ESPN 30 for 30 disc.
    fn real_scan_output() -> Vec<String> {
        vec![
            r#"MSG:1005,0,1,"MakeMKV v1.18.3 linux(x64-release) started","%1 started","MakeMKV v1.18.3 linux(x64-release)""#,
            r#"DRV:0,2,999,1,"BD-RE PIONEER BD-RW  BDR-XD07U 1.03","ESPN_30_FOR_30_DISC_1","/dev/sr0""#,
            r#"MSG:3028,16777216,3,"Title #5 was added (4 cell(s), 0:51:06)","Title #%1 was added (%2 cell(s), %3)","5","4","0:51:06""#,
            r#"MSG:5011,0,0,"Operation successfully completed","Operation successfully completed""#,
            r#"TCOUNT:2"#,
            r#"CINFO:2,0,"ESPN_30_FOR_30_DISC_1""#,
            r#"TINFO:0,8,0,"1""#,
            r#"TINFO:0,9,0,"0:07:07""#,
            r#"TINFO:0,11,0,"223938560""#,
            r#"TINFO:0,27,0,"C5_t00.mkv""#,
            r#"TINFO:1,8,0,"4""#,
            r#"TINFO:1,9,0,"0:51:06""#,
            r#"TINFO:1,11,0,"4000000000""#,
            r#"TINFO:1,27,0,"C5_t01.mkv""#,
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    #[test]
    fn parses_a_real_scan() {
        let scan = parse_scan(real_scan_output()).unwrap();
        assert_eq!(scan.disc_name.as_deref(), Some("ESPN_30_FOR_30_DISC_1"));
        assert_eq!(scan.titles.len(), 2);

        let t = &scan.titles[1];
        assert_eq!(t.index, 1);
        assert_eq!(t.duration_secs, 3066);
        assert_eq!(t.estimated_bytes, 4_000_000_000);
        assert_eq!(t.chapters, 4);
        assert_eq!(t.output_name, "C5_t01.mkv");
    }

    /// The critical case: makemkvcon exits 0 while reporting failure, so a scan that
    /// never reports success must be treated as a failure regardless of exit status.
    #[test]
    fn scan_without_success_message_is_an_error() {
        let lines: Vec<String> = vec![
            r#"MSG:1005,0,1,"MakeMKV started","%1 started","x""#.to_string(),
            r#"TCOUNT:0"#.to_string(),
        ];
        let err = parse_scan(lines).unwrap_err();
        assert!(matches!(err, BackendError::Failed { .. }), "got {err:?}");
    }

    #[test]
    fn explicit_failure_message_is_reported() {
        let lines: Vec<String> = vec![
            r#"MSG:5010,0,0,"Failed to open disc","Failed to open disc""#.to_string(),
            r#"MSG:5011,0,0,"Operation successfully completed","x""#.to_string(),
        ];
        let err = parse_scan(lines).unwrap_err();
        match err {
            BackendError::Reported { message, .. } => {
                assert!(message.contains("Failed to open disc"))
            }
            other => panic!("expected reported failure, got {other:?}"),
        }
    }
}
