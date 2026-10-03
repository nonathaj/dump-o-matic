//! ID3 tag writer: ID3v2.4 at the front of an MP3, ID3v1 at the end.
//!
//! Written by hand rather than through ffmpeg, because remuxing to add tags can replace
//! the LAME/Xing header the encoder wrote, and that header carries the encoder delay and
//! padding a player needs for gapless playback. Here the encoded audio is never touched:
//! the tag is prepended and the v1 block appended, byte for byte around it.
//!
//! The frame layout mirrors MusicBrainz Picard's (via mutagen): UTF-8 text frames,
//! Latin-1 for the purely numeric ones, `TXXX` for Picard's custom fields and a `UFID`
//! carrying the MusicBrainz recording ID.

/// One ID3v2.4 frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// A text frame, written as UTF-8 (`TIT2`, `TPE1`, ...).
    Text(&'static str, String),
    /// A text frame that only ever holds ASCII digits (`TRCK`, `TPOS`, `TLEN`), written
    /// as Latin-1 as Picard does.
    Numeric(&'static str, String),
    /// User-defined text: description and value (`TXXX`).
    UserText(String, String),
    /// Unique file identifier: owner and identifier (`UFID`).
    UniqueId(String, String),
}

/// The ID3v1 fields, which are fixed-width Latin-1.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct V1 {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub year: String,
    pub track: u8,
}

/// Room left after the frames so a tag editor can grow the tag without rewriting the
/// whole file, as most writers do.
const PADDING: usize = 1024;
const ENC_LATIN1: u8 = 0;
const ENC_UTF8: u8 = 3;
/// ID3v1 genre byte meaning "none".
const V1_NO_GENRE: u8 = 255;

fn synchsafe(n: usize) -> [u8; 4] {
    assert!(n < 1 << 28, "ID3v2 size {n} does not fit a synchsafe integer");
    [
        ((n >> 21) & 0x7f) as u8,
        ((n >> 14) & 0x7f) as u8,
        ((n >> 7) & 0x7f) as u8,
        (n & 0x7f) as u8,
    ]
}

fn latin1(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| if (c as u32) < 256 { c as u32 as u8 } else { b'?' })
        .collect()
}

fn frame_bytes(f: &Frame) -> Vec<u8> {
    let (id, body): (&str, Vec<u8>) = match f {
        Frame::Text(id, v) => {
            let mut b = vec![ENC_UTF8];
            b.extend_from_slice(v.as_bytes());
            b.push(0);
            (id, b)
        }
        Frame::Numeric(id, v) => {
            let mut b = vec![ENC_LATIN1];
            b.extend(latin1(v));
            b.push(0);
            (id, b)
        }
        Frame::UserText(desc, v) => {
            let mut b = vec![ENC_UTF8];
            b.extend_from_slice(desc.as_bytes());
            b.push(0);
            b.extend_from_slice(v.as_bytes());
            b.push(0);
            ("TXXX", b)
        }
        Frame::UniqueId(owner, ident) => {
            let mut b = latin1(owner);
            b.push(0);
            b.extend_from_slice(ident.as_bytes());
            ("UFID", b)
        }
    };
    let mut out = Vec::with_capacity(10 + body.len());
    out.extend_from_slice(id.as_bytes());
    out.extend_from_slice(&synchsafe(body.len()));
    out.extend_from_slice(&[0, 0]);
    out.extend(body);
    out
}

/// A complete ID3v2.4 tag, header included, ready to prepend to MPEG frames.
pub fn v2_tag(frames: &[Frame]) -> Vec<u8> {
    let mut body: Vec<u8> = frames.iter().flat_map(frame_bytes).collect();
    body.resize(body.len() + PADDING, 0);
    let mut out = Vec::with_capacity(10 + body.len());
    out.extend_from_slice(b"ID3");
    out.extend_from_slice(&[4, 0, 0]);
    out.extend_from_slice(&synchsafe(body.len()));
    out.extend(body);
    out
}

/// A 128-byte ID3v1.1 tag. Characters outside Latin-1 become `?`, which is what the
/// format can hold.
pub fn v1_tag(t: &V1) -> [u8; 128] {
    let mut out = [0u8; 128];
    out[..3].copy_from_slice(b"TAG");
    let mut put = |at: usize, len: usize, s: &str| {
        let b = latin1(s);
        let n = b.len().min(len);
        out[at..at + n].copy_from_slice(&b[..n]);
    };
    put(3, 30, &t.title);
    put(33, 30, &t.artist);
    put(63, 30, &t.album);
    put(93, 4, &t.year);
    // ID3v1.1: comment shortened to 28 bytes, a zero byte, then the track number.
    out[126] = t.track;
    out[127] = V1_NO_GENRE;
    out
}

/// Wrap untagged MP3 data with both tags.
pub fn tag_mp3(audio: &[u8], frames: &[Frame], v1: &V1) -> Vec<u8> {
    let v2 = v2_tag(frames);
    let mut out = Vec::with_capacity(v2.len() + audio.len() + 128);
    out.extend(v2);
    out.extend_from_slice(audio);
    out.extend_from_slice(&v1_tag(v1));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsynch(b: &[u8]) -> usize {
        (b[0] as usize) << 21 | (b[1] as usize) << 14 | (b[2] as usize) << 7 | b[3] as usize
    }

    #[test]
    fn synchsafe_never_sets_a_high_bit() {
        for n in [0, 127, 128, 9445, (1 << 28) - 1] {
            let s = synchsafe(n);
            assert!(s.iter().all(|b| b & 0x80 == 0));
            assert_eq!(unsynch(&s), n);
        }
    }

    #[test]
    fn writes_a_v24_header_whose_size_covers_the_frames() {
        let tag = v2_tag(&[Frame::Text("TIT2", "Hells Bells".into())]);
        assert_eq!(&tag[..5], b"ID3\x04\x00");
        assert_eq!(unsynch(&tag[6..10]), tag.len() - 10);
        assert_eq!(&tag[10..14], b"TIT2");
    }

    #[test]
    fn text_frames_are_utf8_and_keep_their_script() {
        let f = frame_bytes(&Frame::Text("TALB", "アルドノア・ゼロ".into()));
        assert_eq!(f[10], ENC_UTF8);
        let body = &f[11..f.len() - 1];
        assert_eq!(std::str::from_utf8(body).unwrap(), "アルドノア・ゼロ");
        assert_eq!(unsynch(&f[4..8]), f.len() - 10);
    }

    #[test]
    fn user_text_carries_description_then_value() {
        let f = frame_bytes(&Frame::UserText("CATALOGNUMBER".into(), "SVWC-70016".into()));
        assert_eq!(&f[..4], b"TXXX");
        assert_eq!(&f[10..], b"\x03CATALOGNUMBER\0SVWC-70016\0");
    }

    #[test]
    fn ufid_has_no_encoding_byte() {
        let f = frame_bytes(&Frame::UniqueId("http://musicbrainz.org".into(), "abc".into()));
        assert_eq!(&f[10..], b"http://musicbrainz.org\0abc");
    }

    #[test]
    fn v1_replaces_what_latin1_cannot_hold_and_records_the_track() {
        let t = v1_tag(&V1 {
            title: "No differences".into(),
            artist: "Hiroyuki Sawano".into(),
            album: "アルドノア".into(),
            year: "2014".into(),
            track: 7,
        });
        assert_eq!(&t[..3], b"TAG");
        assert_eq!(&t[63..68], b"?????");
        assert_eq!(&t[93..97], b"2014");
        assert_eq!(t[125], 0);
        assert_eq!(t[126], 7);
    }

    #[test]
    fn tagging_leaves_the_audio_bytes_untouched() {
        let audio = b"\xff\xfb\x90\x44 mpeg frames";
        let out = tag_mp3(audio, &[Frame::Text("TIT2", "x".into())], &V1::default());
        let v2_len = 10 + unsynch(&out[6..10]);
        assert_eq!(&out[v2_len..out.len() - 128], audio);
        assert_eq!(&out[out.len() - 128..out.len() - 125], b"TAG");
    }
}
