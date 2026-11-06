//! Decode DVD (VobSub) subtitles into images, so their words can be read by OCR.
//!
//! A DVD subtitle is a picture, not text: each caption is a subpicture unit (SPU) holding
//! a two-bit, run-length-encoded bitmap, a control sequence saying where it goes and
//! which four of the disc's sixteen palette colours its two bits pick, and how opaque
//! each one is. Most rips also carry the disc's closed captions as a text track, which
//! is read directly and needs none of this; some, Marvel's The Punisher among them, have
//! only the pictures.
//!
//! In a Matroska file the SPUs are stored one to a block, stripped of their MPEG
//! wrapping, and the palette sits in the track's private data as the text of a VobSub
//! `.idx` file. `mkvextract --raw` writes the blocks back to back, and since every SPU
//! begins with its own length, they split apart again exactly.

use crate::pgs::{paper_shade, Caption};
use crate::{BackendError, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const PROBE_TOOL: &str = "mkvmerge";
const EXTRACT_TOOL: &str = "mkvextract";

/// Largest bitmap side accepted. A DVD frame is 720 wide; anything larger is a corrupt
/// display area, not a picture.
const MAX_SIDE: usize = 1024;

/// Sixteen RGB colours, as the disc's palette gives them.
pub type Palette = [[u8; 3]; 16];

/// The palette from the text of a VobSub `.idx` file: `palette: 000000, ffffff, ...`.
pub fn parse_palette(idx: &str) -> Option<Palette> {
    let line = idx
        .lines()
        .find_map(|l| l.trim().strip_prefix("palette:"))?;
    let mut palette = [[0u8; 3]; 16];
    let mut n = 0;
    for (slot, hex) in palette.iter_mut().zip(line.split(',')) {
        let v = u32::from_str_radix(hex.trim(), 16).ok()?;
        *slot = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        n += 1;
    }
    (n == 16).then_some(palette)
}

/// Split raw SPU data, as `mkvextract --raw` writes it, into one slice per SPU.
///
/// Stops at the first length that does not fit, keeping the SPUs before it.
pub fn split_spus(raw: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= raw.len() {
        let size = usize::from(u16::from_be_bytes([raw[i], raw[i + 1]]));
        if size < 4 || i + size > raw.len() {
            break;
        }
        out.push(&raw[i..i + size]);
        i += size;
    }
    out
}

/// What an SPU's control sequences set: where the bitmap goes and how it is coloured.
#[derive(Default)]
struct Control {
    /// Palette index for each of the four pixel values.
    colour: Option<[u8; 4]>,
    /// Opacity, 0 to 15, for each of the four pixel values.
    alpha: Option<[u8; 4]>,
    /// Display area, inclusive: x1, x2, y1, y2.
    area: Option<(usize, usize, usize, usize)>,
    /// Byte offsets of the RLE for the even (top) and odd (bottom) lines.
    fields: Option<(usize, usize)>,
}

/// The four values of a two-byte colour or contrast command, by pixel value 0 to 3.
fn nibbles(b0: u8, b1: u8) -> [u8; 4] {
    [b1 & 0x0f, b1 >> 4, b0 & 0x0f, b0 >> 4]
}

fn read_control(spu: &[u8]) -> Option<Control> {
    let mut c = Control::default();
    let mut seq = usize::from(u16::from_be_bytes([*spu.get(2)?, *spu.get(3)?]));
    // A sequence's link to the next points at itself when it is the last. Bounded, so a
    // corrupt chain cannot loop forever.
    for _ in 0..16 {
        let next = usize::from(u16::from_be_bytes([*spu.get(seq + 2)?, *spu.get(seq + 3)?]));
        let mut i = seq + 4;
        loop {
            let cmd = *spu.get(i)?;
            i += 1;
            match cmd {
                0x00..=0x02 => {}
                0x03 => {
                    c.colour = Some(nibbles(*spu.get(i)?, *spu.get(i + 1)?));
                    i += 2;
                }
                0x04 => {
                    c.alpha = Some(nibbles(*spu.get(i)?, *spu.get(i + 1)?));
                    i += 2;
                }
                0x05 => {
                    let b = spu.get(i..i + 6)?;
                    let x1 = (usize::from(b[0]) << 4) | usize::from(b[1] >> 4);
                    let x2 = (usize::from(b[1] & 0x0f) << 8) | usize::from(b[2]);
                    let y1 = (usize::from(b[3]) << 4) | usize::from(b[4] >> 4);
                    let y2 = (usize::from(b[4] & 0x0f) << 8) | usize::from(b[5]);
                    c.area = Some((x1, x2, y1, y2));
                    i += 6;
                }
                0x06 => {
                    let b = spu.get(i..i + 4)?;
                    c.fields = Some((
                        usize::from(u16::from_be_bytes([b[0], b[1]])),
                        usize::from(u16::from_be_bytes([b[2], b[3]])),
                    ));
                    i += 4;
                }
                // Colour and contrast changes partway along a line: rare, and only for
                // effects. Skipped by their stated length.
                0x07 => {
                    let len = usize::from(u16::from_be_bytes([*spu.get(i)?, *spu.get(i + 1)?]));
                    i += len.max(2);
                }
                0xff => break,
                _ => return None,
            }
        }
        if next == seq {
            break;
        }
        seq = next;
    }
    Some(c)
}

/// Reads four-bit values from a byte offset onwards.
struct Nibbles<'a> {
    data: &'a [u8],
    /// Position in nibbles.
    pos: usize,
}

impl Nibbles<'_> {
    fn next(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos / 2)?;
        let n = if self.pos % 2 == 0 { b >> 4 } else { b & 0x0f };
        self.pos += 1;
        Some(n)
    }

    fn align(&mut self) {
        self.pos += self.pos % 2;
    }

    /// One run: its length (0 means to the end of the line) and its two-bit value.
    ///
    /// The code is one to four nibbles long; a value too small to be a whole code says
    /// another nibble follows.
    fn run(&mut self) -> Option<(usize, u8)> {
        let mut v = u16::from(self.next()?);
        for limit in [0x4, 0x10, 0x40] {
            if v >= limit {
                break;
            }
            v = (v << 4) | u16::from(self.next()?);
        }
        Some((usize::from(v >> 2), (v & 3) as u8))
    }
}

/// Decode one SPU into a caption, or `None` if it is malformed or shows nothing.
pub fn decode_spu(spu: &[u8], palette: &Palette) -> Option<Caption> {
    let c = read_control(spu)?;
    let (x1, x2, y1, y2) = c.area?;
    let (top, bottom) = c.fields?;
    if x2 < x1 || y2 < y1 {
        return None;
    }
    let (width, height) = (x2 - x1 + 1, y2 - y1 + 1);
    if width > MAX_SIDE || height > MAX_SIDE {
        return None;
    }
    let colour = c.colour.unwrap_or([0, 1, 2, 3]);
    let alpha = c.alpha.unwrap_or([0, 15, 15, 15]);
    let shade: Vec<u8> = (0..4)
        .map(|v| {
            let [r, g, b] = palette[usize::from(colour[v]) & 0x0f];
            let luma = (77 * u32::from(r) + 150 * u32::from(g) + 29 * u32::from(b)) >> 8;
            paper_shade(luma as u8, alpha[v] * 17)
        })
        .collect();

    let mut pixels = vec![255u8; width * height];
    // Even lines come from the top field, odd lines from the bottom one.
    for (first_line, offset) in [(0usize, top), (1usize, bottom)] {
        let mut rle = Nibbles {
            data: spu,
            pos: offset * 2,
        };
        for y in (first_line..height).step_by(2) {
            let mut x = 0usize;
            while x < width {
                let (run, value) = rle.run()?;
                let run = if run == 0 { width - x } else { run.min(width - x) };
                let row = y * width;
                pixels[row + x..row + x + run].fill(shade[usize::from(value)]);
                x += run;
            }
            rle.align();
        }
    }
    Some(Caption {
        start_ms: 0,
        width,
        height,
        pixels,
    })
}

/// Decode every SPU in raw data, skipping any that are malformed.
pub fn decode(raw: &[u8], palette: &Palette) -> Vec<Caption> {
    split_spus(raw)
        .into_iter()
        .filter_map(|spu| decode_spu(spu, palette))
        .collect()
}

/// The mkvmerge track ID and palette of the `nth` subtitle track of a Matroska file.
fn track_and_palette(path: &Path, nth: usize) -> Result<Option<(u64, Palette)>> {
    let out = Command::new(PROBE_TOOL)
        .arg("-J")
        .arg(path)
        .output()
        .map_err(|e| not_installed(PROBE_TOOL, e))?;
    if !out.status.success() {
        return Ok(None);
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| BackendError::Failed {
        tool: PROBE_TOOL,
        detail: format!("unreadable track list: {e}"),
    })?;
    let Some(track) = v["tracks"]
        .as_array()
        .and_then(|a| a.iter().filter(|t| t["type"] == "subtitles").nth(nth))
    else {
        return Ok(None);
    };
    let (Some(id), Some(hex)) = (
        track["id"].as_u64(),
        track["properties"]["codec_private_data"].as_str(),
    ) else {
        return Ok(None);
    };
    let idx = (0..hex.len() / 2)
        .filter_map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok())
        .collect::<Vec<u8>>();
    Ok(parse_palette(&String::from_utf8_lossy(&idx)).map(|p| (id, p)))
}

/// A temporary file, removed when dropped.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The captions of the `nth` subtitle track of a Matroska file, which must be VobSub.
///
/// A track that cannot be read gives no captions rather than an error, as a text track
/// that will not convert does: the title simply has no readable dialogue.
pub fn read(path: &Path, nth: usize) -> Result<Vec<Caption>> {
    let Some((id, palette)) = track_and_palette(path, nth)? else {
        return Ok(Vec::new());
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = TempFile(std::env::temp_dir().join(format!(
        "dumo-vobsub-{}-{nanos}.raw",
        std::process::id()
    )));
    let out = Command::new(EXTRACT_TOOL)
        .arg("tracks")
        .arg(path)
        .arg("--raw")
        .arg(format!("{id}:{}", tmp.0.display()))
        .output()
        .map_err(|e| not_installed(EXTRACT_TOOL, e))?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read(&tmp.0).map_err(|e| BackendError::Io {
        tool: EXTRACT_TOOL,
        source: e,
    })?;
    Ok(decode(&raw, &palette))
}

fn not_installed(tool: &'static str, e: std::io::Error) -> BackendError {
    if e.kind() == std::io::ErrorKind::NotFound {
        BackendError::NotInstalled { tool }
    } else {
        BackendError::Io { tool, source: e }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        let mut p = [[0u8; 3]; 16];
        p[1] = [0xff, 0xff, 0xff];
        p
    }

    /// A 4x2 caption: a white line of four pixels over a transparent one.
    fn spu() -> Vec<u8> {
        let mut s = vec![0, 0, 0, 7];
        // Top field at 4: one code, four pixels of value 1 (0x11).
        s.push(0x11);
        // Bottom field at 5: value 0 to the end of the line (four zero nibbles).
        s.extend([0x00, 0x00]);
        // Control sequence at 7, the last: date, link to itself, then commands.
        s.extend([0x00, 0x00, 0x00, 0x07]);
        s.extend([0x03, 0x00, 0x10]); // value 1 -> palette 1, value 0 -> palette 0
        s.extend([0x04, 0x00, 0xf0]); // value 1 opaque, value 0 transparent
        s.extend([0x05, 0x00, 0x00, 0x03, 0x00, 0x00, 0x01]); // x 0..3, y 0..1
        s.extend([0x06, 0x00, 0x04, 0x00, 0x05]);
        s.extend([0x01, 0xff]);
        let len = s.len() as u16;
        s[0..2].copy_from_slice(&len.to_be_bytes());
        s
    }

    #[test]
    fn reads_the_palette_from_the_idx_text() {
        let idx = "# VobSub index file, v7\nsize: 720x480\npalette: 000000, ffffff, 808080, \
                   ff0000, 000000, 000000, 000000, 000000, 000000, 000000, 000000, 000000, \
                   000000, 000000, 000000, 00ff00\n";
        let p = parse_palette(idx).unwrap();
        assert_eq!(p[1], [0xff, 0xff, 0xff]);
        assert_eq!(p[3], [0xff, 0, 0]);
        assert_eq!(p[15], [0, 0xff, 0]);
        assert!(parse_palette("palette: 000000, ffffff").is_none());
        assert!(parse_palette("size: 720x480").is_none());
    }

    #[test]
    fn splits_spus_by_their_own_lengths() {
        let one = spu();
        let mut raw = one.clone();
        raw.extend(&one);
        assert_eq!(split_spus(&raw).len(), 2);
        // A length running past the end stops the split, keeping what came before.
        raw.extend([0xff, 0xff, 0, 0]);
        assert_eq!(split_spus(&raw).len(), 2);
    }

    #[test]
    fn decodes_a_caption_with_its_fill_as_ink() {
        let c = decode_spu(&spu(), &palette()).unwrap();
        assert_eq!((c.width, c.height), (4, 2));
        // White fill becomes ink; the transparent line is paper.
        assert_eq!(&c.pixels[0..4], &[0, 0, 0, 0]);
        assert_eq!(&c.pixels[4..8], &[255, 255, 255, 255]);
    }

    #[test]
    fn malformed_data_is_skipped_not_fatal() {
        let mut bad = spu();
        bad.truncate(10);
        bad[0..2].copy_from_slice(&10u16.to_be_bytes());
        assert!(decode_spu(&bad, &palette()).is_none());
        let mut raw = bad;
        raw.extend(spu());
        assert_eq!(decode(&raw, &palette()).len(), 1);
    }
}
