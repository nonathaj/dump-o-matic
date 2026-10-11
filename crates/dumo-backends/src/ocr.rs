//! Read the words in picture subtitles, with Tesseract.
//!
//! Used only when a title has no text subtitle track: a Blu-ray's subtitles are
//! bitmaps ([`crate::pgs`]), so its dialogue is invisible to word matching until it is
//! recognised. Captions are stacked into pages and each page is one Tesseract run, the
//! pages spread across the machine's cores.

use crate::pgs::{self, Caption};
use crate::{BackendError, Result};
use std::io::Write;
use std::process::{Command, Stdio};

const TOOL: &str = "tesseract";

/// Captions per page. Enough that process start-up is a small share of the work, few
/// enough that one page's failure loses little.
const CAPTIONS_PER_PAGE: usize = 40;

/// Paper left around and between captions, in pixels. Tesseract finds lines by the
/// white space between them.
const GAP: usize = 16;

/// Whether Tesseract can be run at all.
pub fn available() -> bool {
    Command::new(TOOL)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Recognise the words in a set of captions, in order, one line of text per line read.
pub fn read_captions(captions: &[Caption]) -> Result<String> {
    if captions.is_empty() {
        return Ok(String::new());
    }
    if !available() {
        return Err(BackendError::NotInstalled { tool: TOOL });
    }
    let trimmed: Vec<Caption> = captions
        .iter()
        .map(|c| pgs::trim(c, 4))
        .filter(|c| c.width > 0)
        .collect();
    let pages: Vec<Vec<u8>> = trimmed
        .chunks(CAPTIONS_PER_PAGE)
        .map(|chunk| {
            let (w, h, px) = pgs::stack(chunk, GAP);
            pgs::pgm(w, h, &px)
        })
        .collect();

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .min(pages.len())
        .max(1);
    let mut texts: Vec<Option<Result<String>>> = (0..pages.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|w| {
                let pages = &pages;
                scope.spawn(move || {
                    (w..pages.len())
                        .step_by(workers)
                        .map(|i| (i, recognise_page(&pages[i])))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            for (i, r) in h.join().unwrap_or_default() {
                texts[i] = Some(r);
            }
        }
    });

    let mut out = String::new();
    for t in texts.into_iter().flatten() {
        out.push_str(&t?);
        out.push('\n');
    }
    Ok(out)
}

/// One Tesseract run over one page, image in on stdin and text out on stdout.
fn recognise_page(page: &[u8]) -> Result<String> {
    let mut child = Command::new(TOOL)
        // Page segmentation mode 6: one uniform block of text, which a stack of
        // left-aligned captions is. The default tries to find columns and pictures.
        .args(["stdin", "stdout", "-l", "eng", "--psm", "6"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                BackendError::NotInstalled { tool: TOOL }
            } else {
                BackendError::Io { tool: TOOL, source: e }
            }
        })?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(page)
        .map_err(|e| BackendError::Io { tool: TOOL, source: e })?;
    let out = child
        .wait_with_output()
        .map_err(|e| BackendError::Io { tool: TOOL, source: e })?;
    if !out.status.success() {
        return Err(BackendError::Failed {
            tool: TOOL,
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_captions_needs_no_tesseract() {
        assert_eq!(read_captions(&[]).unwrap(), "");
    }

    /// Runs only where Tesseract is installed, like the chdman round trips.
    #[test]
    fn a_blank_page_reads_as_no_words() {
        if !available() {
            return;
        }
        let blank = Caption {
            start_ms: 0,
            width: 200,
            height: 40,
            pixels: vec![255; 200 * 40],
        };
        // A caption with no ink is trimmed away entirely, so nothing is sent.
        assert_eq!(read_captions(&[blank]).unwrap().trim(), "");
    }
}
