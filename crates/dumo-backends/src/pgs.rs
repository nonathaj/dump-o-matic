//! Decode Blu-ray PGS (Presentation Graphic Stream) subtitles into images.
//!
//! Blu-ray subtitles are pictures, not text: each caption is a run-length encoded,
//! palette-indexed bitmap the player lays over the video. To read the words they have
//! to be drawn and recognised, and this module does the drawing — turning a `.sup`
//! stream into one greyscale image per caption, dark text on white, which is what OCR
//! reads best.
//!
//! The format is small and public (it is the stream MakeMKV, ffmpeg and every Blu-ray
//! player read): a sequence of segments, each `"PG"`, two 90 kHz timestamps, a type
//! byte and a length. A palette segment, object segments holding the bitmap, and a
//! presentation segment saying which objects to show, ended by an END segment, make
//! one *display set*. Only what reading the words needs is implemented: no cropping,
//! no windows, no positioning beyond stacking a caption's objects top to bottom.

use std::collections::{HashMap, HashSet};

const SEG_PALETTE: u8 = 0x14;
const SEG_OBJECT: u8 = 0x15;
const SEG_PRESENTATION: u8 = 0x16;
const SEG_END: u8 = 0x80;

/// Largest bitmap side accepted. A caption on a 1080p disc is at most 1920 wide;
/// anything larger is a corrupt length, not a picture.
const MAX_SIDE: usize = 4096;

/// One caption as an 8-bit greyscale image: 255 is paper, 0 is ink.
#[derive(Debug, Clone, PartialEq)]
pub struct Caption {
    /// When it appears, in milliseconds from the start of the title.
    pub start_ms: u64,
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// An object (bitmap) as delivered: palette indices, possibly split across segments.
#[derive(Default)]
struct Object {
    width: usize,
    height: usize,
    rle: Vec<u8>,
}

/// Decode a whole `.sup` stream into captions, in display order.
///
/// A caption that is shown again unchanged — players re-send the current one at every
/// epoch boundary — is kept once, so its words are not counted twice. Malformed input
/// ends decoding where it goes wrong rather than failing: the captions before it are
/// still good.
pub fn decode(sup: &[u8]) -> Vec<Caption> {
    let mut palettes: HashMap<u8, [u8; 256]> = HashMap::new();
    let mut objects: HashMap<u16, Object> = HashMap::new();
    // The current display set's presentation: start time, palette, object ids.
    let mut shown: Option<(u64, u8, Vec<u16>)> = None;
    let mut seen: HashSet<u64> = HashSet::new();
    let mut out = Vec::new();

    let mut at = 0usize;
    while at + 13 <= sup.len() {
        if &sup[at..at + 2] != b"PG" {
            break;
        }
        let pts = u32::from_be_bytes([sup[at + 2], sup[at + 3], sup[at + 4], sup[at + 5]]);
        let kind = sup[at + 10];
        let len = u16::from_be_bytes([sup[at + 11], sup[at + 12]]) as usize;
        let body_start = at + 13;
        let Some(body) = sup.get(body_start..body_start + len) else {
            break;
        };
        at = body_start + len;

        match kind {
            SEG_PALETTE if body.len() >= 2 => {
                let entry = palettes.entry(body[0]).or_insert([255u8; 256]);
                for e in body[2..].chunks_exact(5) {
                    let (index, y, alpha) = (e[0], e[1], e[4]);
                    entry[index as usize] = paper_shade(y, alpha);
                }
            }
            SEG_OBJECT if body.len() >= 4 => {
                let id = u16::from_be_bytes([body[0], body[1]]);
                let first = body[3] & 0x80 != 0;
                if first {
                    if body.len() < 11 {
                        continue;
                    }
                    let width = u16::from_be_bytes([body[7], body[8]]) as usize;
                    let height = u16::from_be_bytes([body[9], body[10]]) as usize;
                    objects.insert(
                        id,
                        Object {
                            width,
                            height,
                            rle: body[11..].to_vec(),
                        },
                    );
                } else if let Some(o) = objects.get_mut(&id) {
                    o.rle.extend_from_slice(&body[4..]);
                }
            }
            SEG_PRESENTATION if body.len() >= 11 => {
                let palette_id = body[9];
                let count = body[10] as usize;
                let mut ids = Vec::new();
                let mut o = 11;
                for _ in 0..count {
                    let Some(rec) = body.get(o..o + 8) else { break };
                    ids.push(u16::from_be_bytes([rec[0], rec[1]]));
                    // A cropped object carries four more fields.
                    o += if rec[3] & 0x80 != 0 { 16 } else { 8 };
                }
                // An epoch start discards everything defined before it.
                if body[7] & 0x80 != 0 {
                    objects.clear();
                }
                shown = Some((u64::from(pts) / 90, palette_id, ids));
            }
            SEG_END => {
                let Some((start_ms, palette_id, ids)) = shown.take() else {
                    continue;
                };
                // A presentation with no objects clears the screen: nothing to read.
                let Some(palette) = palettes.get(&palette_id) else {
                    continue;
                };
                for id in ids {
                    let Some(obj) = objects.get(&id) else { continue };
                    let Some(pixels) = render(obj, palette) else {
                        continue;
                    };
                    if !seen.insert(fingerprint(&pixels, obj.width)) {
                        continue;
                    }
                    out.push(Caption {
                        start_ms,
                        width: obj.width,
                        height: obj.height,
                        pixels,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// The shade a palette entry paints, as dark-on-white.
///
/// The colour is composited over black, which leaves the text's fill bright and its
/// outline and the transparent surround dark, and then inverted: the fill becomes ink
/// and everything else paper. Hue is irrelevant to reading words, so only luma is used.
fn paper_shade(y: u8, alpha: u8) -> u8 {
    let over_black = u16::from(y) * u16::from(alpha) / 255;
    255 - over_black as u8
}

/// Expand an object's run-length encoding through a palette.
///
/// Codes: a non-zero byte is one pixel of that colour; `00 00` ends a line; otherwise
/// `00` is followed by a flag byte whose top two bits say whether the run length is 6
/// or 14 bits and whether a colour follows (absent means colour 0).
fn render(obj: &Object, palette: &[u8; 256]) -> Option<Vec<u8>> {
    if obj.width == 0 || obj.height == 0 || obj.width > MAX_SIDE || obj.height > MAX_SIDE {
        return None;
    }
    let mut px = vec![255u8; obj.width * obj.height];
    let (mut x, mut y) = (0usize, 0usize);
    let data = &obj.rle;
    let mut i = 0usize;
    let mut put = |x: &mut usize, y: usize, n: usize, colour: u8| {
        if y >= obj.height {
            return;
        }
        let shade = palette[colour as usize];
        let end = (*x + n).min(obj.width);
        if *x < end {
            px[y * obj.width + *x..y * obj.width + end].fill(shade);
        }
        *x += n;
    };
    while i < data.len() && y < obj.height {
        let b = data[i];
        i += 1;
        if b != 0 {
            put(&mut x, y, 1, b);
            continue;
        }
        let flag = *data.get(i)?;
        i += 1;
        if flag == 0 {
            x = 0;
            y += 1;
            continue;
        }
        let long = flag & 0x40 != 0;
        let coloured = flag & 0x80 != 0;
        let mut n = usize::from(flag & 0x3f);
        if long {
            n = (n << 8) | usize::from(*data.get(i)?);
            i += 1;
        }
        let colour = if coloured {
            let c = *data.get(i)?;
            i += 1;
            c
        } else {
            0
        };
        put(&mut x, y, n, colour);
    }
    Some(px)
}

fn fingerprint(pixels: &[u8], width: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    width.hash(&mut h);
    pixels.hash(&mut h);
    h.finish()
}

/// Crop a caption to its ink, plus a margin, so a page wastes no space on paper.
pub fn trim(c: &Caption, margin: usize) -> Caption {
    let ink = |v: u8| v < 128;
    let rows: Vec<usize> = (0..c.height)
        .filter(|&r| c.pixels[r * c.width..(r + 1) * c.width].iter().any(|&v| ink(v)))
        .collect();
    let cols: Vec<usize> = (0..c.width)
        .filter(|&col| (0..c.height).any(|r| ink(c.pixels[r * c.width + col])))
        .collect();
    let (Some(&top), Some(&bottom), Some(&left), Some(&right)) =
        (rows.first(), rows.last(), cols.first(), cols.last())
    else {
        return Caption {
            start_ms: c.start_ms,
            width: 0,
            height: 0,
            pixels: Vec::new(),
        };
    };
    let width = right - left + 1 + 2 * margin;
    let height = bottom - top + 1 + 2 * margin;
    let mut pixels = vec![255u8; width * height];
    for r in top..=bottom {
        let src = &c.pixels[r * c.width + left..=r * c.width + right];
        let dst = (r - top + margin) * width + margin;
        pixels[dst..dst + src.len()].copy_from_slice(src);
    }
    Caption {
        start_ms: c.start_ms,
        width,
        height,
        pixels,
    }
}

/// Stack captions into one page, top to bottom, for a single OCR pass.
///
/// Recognising one page of forty captions is far faster than forty separate runs, and
/// the words are all that is wanted — which caption said them does not matter.
pub fn stack(captions: &[Caption], gap: usize) -> (usize, usize, Vec<u8>) {
    let width = captions.iter().map(|c| c.width).max().unwrap_or(0) + 2 * gap;
    let height = captions.iter().map(|c| c.height + gap).sum::<usize>() + gap;
    let mut pixels = vec![255u8; width * height];
    let mut top = gap;
    for c in captions {
        for r in 0..c.height {
            let dst = (top + r) * width + gap;
            pixels[dst..dst + c.width].copy_from_slice(&c.pixels[r * c.width..(r + 1) * c.width]);
        }
        top += c.height + gap;
    }
    (width, height, pixels)
}

/// Encode greyscale pixels as binary PGM, which Tesseract reads directly.
pub fn pgm(width: usize, height: usize, pixels: &[u8]) -> Vec<u8> {
    let mut out = format!("P5\n{width} {height}\n255\n").into_bytes();
    out.extend_from_slice(pixels);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(kind: u8, pts_ms: u32, body: &[u8]) -> Vec<u8> {
        let mut s = b"PG".to_vec();
        s.extend_from_slice(&(pts_ms * 90).to_be_bytes());
        s.extend_from_slice(&0u32.to_be_bytes());
        s.push(kind);
        s.extend_from_slice(&(body.len() as u16).to_be_bytes());
        s.extend_from_slice(body);
        s
    }

    /// Palette 0: index 1 opaque white (text fill), index 0 transparent.
    fn palette() -> Vec<u8> {
        let mut b = vec![0u8, 0];
        b.extend_from_slice(&[0, 16, 128, 128, 0]);
        b.extend_from_slice(&[1, 235, 128, 128, 255]);
        b
    }

    /// A 4x2 object: row 0 = two transparent then two text pixels via runs, row 1 =
    /// a literal text pixel then a long transparent run.
    fn object(id: u16) -> Vec<u8> {
        let rle = [
            0x00, 0x02, // 2 pixels of colour 0
            0x00, 0x82, 0x01, // 2 pixels of colour 1
            0x00, 0x00, // end of line
            0x01, // 1 pixel of colour 1
            0x00, 0x40, 0x03, // 3 pixels of colour 0, 14-bit length form
            0x00, 0x00,
        ];
        let mut b = id.to_be_bytes().to_vec();
        b.push(0); // version
        b.push(0xC0); // first and last in sequence
        let len = (rle.len() + 4) as u32;
        b.extend_from_slice(&len.to_be_bytes()[1..]);
        b.extend_from_slice(&4u16.to_be_bytes());
        b.extend_from_slice(&2u16.to_be_bytes());
        b.extend_from_slice(&rle);
        b
    }

    fn presentation(objects: &[u16], epoch_start: bool) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&1920u16.to_be_bytes());
        b.extend_from_slice(&1080u16.to_be_bytes());
        b.push(0x10); // frame rate
        b.extend_from_slice(&1u16.to_be_bytes()); // composition number
        b.push(if epoch_start { 0x80 } else { 0x00 });
        b.push(0); // palette update flag
        b.push(0); // palette id
        b.push(objects.len() as u8);
        for id in objects {
            b.extend_from_slice(&id.to_be_bytes());
            b.push(0); // window
            b.push(0); // not cropped
            b.extend_from_slice(&100u16.to_be_bytes());
            b.extend_from_slice(&900u16.to_be_bytes());
        }
        b
    }

    fn stream() -> Vec<u8> {
        let mut s = Vec::new();
        s.extend(segment(SEG_PRESENTATION, 1000, &presentation(&[0], true)));
        s.extend(segment(SEG_PALETTE, 1000, &palette()));
        s.extend(segment(SEG_OBJECT, 1000, &object(0)));
        s.extend(segment(SEG_END, 1000, &[]));
        // Cleared, then the same caption re-sent at the next epoch.
        s.extend(segment(SEG_PRESENTATION, 3000, &presentation(&[], false)));
        s.extend(segment(SEG_END, 3000, &[]));
        s.extend(segment(SEG_PRESENTATION, 5000, &presentation(&[0], true)));
        s.extend(segment(SEG_PALETTE, 5000, &palette()));
        s.extend(segment(SEG_OBJECT, 5000, &object(0)));
        s.extend(segment(SEG_END, 5000, &[]));
        s
    }

    #[test]
    fn decodes_a_caption_as_dark_text_on_white() {
        let caps = decode(&stream());
        assert_eq!(caps.len(), 1, "a re-sent identical caption is kept once");
        let c = &caps[0];
        assert_eq!((c.start_ms, c.width, c.height), (1000, 4, 2));
        let ink = 255 - 235;
        assert_eq!(c.pixels, vec![255, 255, ink, ink, ink, 255, 255, 255]);
    }

    #[test]
    fn truncated_or_garbage_input_does_not_panic() {
        let s = stream();
        for cut in 0..s.len() {
            let _ = decode(&s[..cut]);
        }
        assert!(decode(b"not a sup stream at all").is_empty());
    }

    #[test]
    fn trimming_keeps_only_the_ink_and_a_margin() {
        let c = decode(&stream()).remove(0);
        let t = trim(&c, 1);
        // Ink spans columns 0..=3 and rows 0..=1.
        assert_eq!((t.width, t.height), (6, 4));
        assert!(t.pixels[..6].iter().all(|&v| v == 255), "margin row is paper");
    }

    #[test]
    fn stacked_page_holds_every_caption() {
        let c = decode(&stream()).remove(0);
        let (w, h, px) = stack(&[c.clone(), c], 2);
        assert_eq!((w, h), (8, 2 + 2 * (2 + 2)));
        assert_eq!(px.iter().filter(|&&v| v < 128).count(), 6);
        let header = pgm(w, h, &px);
        assert!(header.starts_with(b"P5\n8 10\n255\n"));
    }
}
