//! AccurateRip verification: did this rip produce the same audio as everyone else's?
//!
//! AccurateRip's database holds per-track checksums submitted by other people ripping the
//! same pressing. A match is the audio counterpart of a Redump hash match — independent
//! evidence the rip is bit-exact — and it also proves the drive's read offset is right,
//! since a rip shifted by even one sample matches nothing.
//!
//! The disc is looked up by three IDs derived from the TOC; each track's checksum is a
//! weighted sum of its samples (v1, and v2 which keeps the 64-bit product's high half).
//! The first 5 sectors of track 1 and last 5 of the final track are excluded, because
//! drives differ in whether they can read that close to the lead-in and lead-out.

use crate::http;

const DB_BASE: &str = "http://www.accuraterip.com/accuraterip";
const TIMEOUT_SECS: u32 = 20;
const SAMPLES_PER_SECTOR: u64 = 588;
const EDGE_SAMPLES: u64 = 5 * SAMPLES_PER_SECTOR;

/// The three IDs AccurateRip files a disc under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscIds {
    pub track_count: u8,
    pub id1: u32,
    pub id2: u32,
    pub cddb: u32,
}

impl DiscIds {
    /// From the audio tracks' 0-based start LBAs and the lead-out, plus the FreeDB ID.
    pub fn compute(starts: &[u32], leadout: u32, cddb: u32) -> Self {
        let mut id1: u32 = 0;
        let mut id2: u32 = 0;
        for (i, &s) in starts.iter().enumerate() {
            id1 = id1.wrapping_add(s);
            id2 = id2.wrapping_add(s.max(1).wrapping_mul(i as u32 + 1));
        }
        id1 = id1.wrapping_add(leadout);
        id2 = id2.wrapping_add(leadout.wrapping_mul(starts.len() as u32 + 1));
        DiscIds {
            track_count: starts.len() as u8,
            id1,
            id2,
            cddb,
        }
    }

    pub fn url(&self) -> String {
        format!(
            "{DB_BASE}/{:x}/{:x}/{:x}/dBAR-{:03}-{:08x}-{:08x}-{:08x}.bin",
            self.id1 & 0xf,
            (self.id1 >> 4) & 0xf,
            (self.id1 >> 8) & 0xf,
            self.track_count,
            self.id1,
            self.id2,
            self.cddb
        )
    }
}

/// One pressing's submitted checksums for every track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Per track: (confidence, checksum).
    pub tracks: Vec<(u8, u32)>,
}

/// Fetch the database record. A disc nobody has submitted is an empty list.
pub fn lookup(ids: &DiscIds) -> Result<Vec<Entry>, http::HttpError> {
    match http::get_bytes(&ids.url(), &[], TIMEOUT_SECS) {
        Ok(b) => Ok(parse(&b, ids)),
        Err(http::HttpError::Status { status: 404, .. }) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Parse a `dBAR` file: repeated blocks of a 13-byte header (track count, the three
/// IDs) followed by 9 bytes per track (confidence, CRC, and a CRC of an offset-finding
/// frame we do not use). Blocks for a different disc are skipped.
pub fn parse(b: &[u8], ids: &DiscIds) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut o = 0usize;
    let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    while o + 13 <= b.len() {
        let n = b[o] as usize;
        let header = DiscIds {
            track_count: b[o],
            id1: u32_at(o + 1),
            id2: u32_at(o + 5),
            cddb: u32_at(o + 9),
        };
        o += 13;
        if o + n * 9 > b.len() {
            break;
        }
        let tracks = (0..n).map(|t| (b[o + t * 9], u32_at(o + t * 9 + 1))).collect();
        o += n * 9;
        if header == *ids {
            out.push(Entry { tracks });
        }
    }
    out
}

/// AccurateRip v1 and v2 checksums of one track's PCM (16-bit stereo, little-endian).
pub fn checksums(pcm: &[u8], is_first: bool, is_last: bool) -> (u32, u32) {
    let total = (pcm.len() / 4) as u64;
    let start = if is_first { EDGE_SAMPLES } else { 1 };
    let end = if is_last {
        total.saturating_sub(EDGE_SAMPLES)
    } else {
        total
    };
    let mut v1: u32 = 0;
    let mut v2: u32 = 0;
    for (i, s) in pcm.chunks_exact(4).enumerate() {
        let mult = i as u64 + 1;
        if mult < start || mult > end {
            continue;
        }
        let sample = u64::from(u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
        let product = sample * mult;
        v1 = v1.wrapping_add(product as u32);
        v2 = v2
            .wrapping_add(product as u32)
            .wrapping_add((product >> 32) as u32);
    }
    (v1, v2)
}

/// How one track fared against the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackResult {
    /// Matched; the confidence is how many submitters produced the same checksum.
    Accurate { confidence: u32 },
    /// The disc is in the database but this track's checksum matched no submission.
    Mismatch,
}

/// Compare one track's checksums against every pressing in the record. Confidence is
/// summed across pressings that agree, as AccurateRip reports it.
pub fn verify_track(entries: &[Entry], index: usize, v1: u32, v2: u32) -> TrackResult {
    let confidence: u32 = entries
        .iter()
        .filter_map(|e| e.tracks.get(index))
        .filter(|(_, crc)| *crc == v1 || *crc == v2)
        .map(|(c, _)| u32::from(*c))
        .sum();
    if confidence > 0 {
        TrackResult::Accurate { confidence }
    } else {
        TrackResult::Mismatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IDs and URL for the disc this module was first used on, whose TOC is
    /// cross-checked against cdparanoia's.
    #[test]
    fn computes_ids_and_url_from_the_toc() {
        let starts = [
            0, 20776, 34101, 51257, 67933, 85139, 103025, 120594, 137706, 149188, 166295,
            187553, 210205, 228578, 246399, 265721, 282678, 294931, 312658, 334079,
        ];
        let ids = DiscIds::compute(&starts, 352228, 0x3812_5814);
        let expected_id1: u32 = starts.iter().sum::<u32>() + 352228;
        assert_eq!(ids.id1, expected_id1);
        let expected_id2: u32 = starts
            .iter()
            .enumerate()
            .map(|(i, &s)| s.max(1) * (i as u32 + 1))
            .sum::<u32>()
            + 352228 * 21;
        assert_eq!(ids.id2, expected_id2);
        assert!(ids.url().contains(&format!("dBAR-020-{:08x}-{:08x}-38125814.bin", ids.id1, ids.id2)));
    }

    fn record(ids: &DiscIds, tracks: &[(u8, u32)]) -> Vec<u8> {
        let mut b = vec![ids.track_count];
        for v in [ids.id1, ids.id2, ids.cddb] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for (conf, crc) in tracks {
            b.push(*conf);
            b.extend_from_slice(&crc.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
        }
        b
    }

    #[test]
    fn parses_pressings_and_skips_other_discs() {
        let ids = DiscIds { track_count: 2, id1: 1, id2: 2, cddb: 3 };
        let other = DiscIds { id1: 9, ..ids };
        let mut b = record(&ids, &[(5, 0xaa), (4, 0xbb)]);
        b.extend(record(&other, &[(1, 0xcc), (1, 0xdd)]));
        b.extend(record(&ids, &[(2, 0xee), (2, 0xff)]));
        let e = parse(&b, &ids);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].tracks, vec![(5, 0xaa), (4, 0xbb)]);
        assert_eq!(verify_track(&e, 1, 0xbb, 0), TrackResult::Accurate { confidence: 4 });
        assert_eq!(verify_track(&e, 1, 0x00, 0x01), TrackResult::Mismatch);
    }

    #[test]
    fn checksum_weights_each_sample_by_its_position() {
        // Two samples of value 1 and 2, mid-disc: 1*1 + 2*2.
        let pcm = [1u8, 0, 0, 0, 2, 0, 0, 0];
        assert_eq!(checksums(&pcm, false, false), (5, 5));
    }

    #[test]
    fn edge_sectors_are_excluded_on_the_first_and_last_tracks() {
        let n = (EDGE_SAMPLES * 3) as usize;
        let pcm: Vec<u8> = (0..n).flat_map(|_| 1u32.to_le_bytes()).collect();
        let (all, _) = checksums(&pcm, false, false);
        let (first, _) = checksums(&pcm, true, false);
        let (last, _) = checksums(&pcm, false, true);
        // Sum of positions 1..=n, minus those skipped at each edge.
        let sum = |a: u64, b: u64| ((a + b) * (b - a + 1) / 2) as u32;
        assert_eq!(all, sum(1, n as u64));
        assert_eq!(first, sum(EDGE_SAMPLES, n as u64));
        assert_eq!(last, sum(1, n as u64 - EDGE_SAMPLES));
    }

    #[test]
    fn v2_keeps_the_high_half_of_the_product() {
        // 0xffffffff * 2 overflows 32 bits; v1 drops the carry, v2 adds it back.
        let pcm = [0u8, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        let (v1, v2) = checksums(&pcm, false, false);
        assert_eq!(v1, 0xffff_fffe);
        assert_eq!(v2, 0xffff_ffff);
    }
}
