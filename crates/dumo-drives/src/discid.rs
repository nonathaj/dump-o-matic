//! Disc identifiers computed from a CD table of contents.
//!
//! Both of these are computed purely from track offsets, so they are available in
//! Stage 1 without reading a single byte of audio. A MusicBrainz Disc ID match is an
//! *exact* identification of a pressing, which is what makes audio CDs eligible for
//! unattended auto-accept.

use base64::Engine;
use dumo_core::TocInfo;
use sha1::{Digest, Sha1};

/// Red Book pregap: track offsets are expressed relative to the start of the lead-in,
/// which sits 150 sectors (2 seconds) before LBA 0.
const PREGAP_SECTORS: u32 = 150;

/// Compute the MusicBrainz Disc ID.
///
/// Algorithm (per the MusicBrainz specification): SHA-1 over the first track number,
/// last track number, and 100 track offsets — the lead-out first, then each track,
/// zero-padded — each formatted as 8 uppercase hex digits. The digest is then base64
/// encoded with a URL-safe alphabet using `.`, `_`, and `-`.
pub fn musicbrainz_discid(toc: &TocInfo) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("{:02X}", toc.first_track).as_bytes());
    hasher.update(format!("{:02X}", toc.last_track).as_bytes());

    let mut offsets = [0u32; 100];
    offsets[0] = toc.leadout_lba + PREGAP_SECTORS;
    for track in &toc.tracks {
        let idx = track.number as usize;
        if (1..100).contains(&idx) {
            offsets[idx] = track.start_lba + PREGAP_SECTORS;
        }
    }
    for offset in offsets {
        hasher.update(format!("{offset:08X}").as_bytes());
    }

    let digest = hasher.finalize();
    let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
    // MusicBrainz uses a variant alphabet so the ID is URL-safe.
    b64.replace('+', ".").replace('/', "_").replace('=', "-")
}

/// Compute the FreeDB/CDDB disc ID (8 hex digits).
///
/// Retained because several older metadata sources still key on it.
pub fn freedb_discid(toc: &TocInfo) -> String {
    fn digit_sum(mut n: u32) -> u32 {
        let mut sum = 0;
        while n > 0 {
            sum += n % 10;
            n /= 10;
        }
        sum
    }

    let checksum: u32 = toc
        .tracks
        .iter()
        .map(|t| digit_sum((t.start_lba + PREGAP_SECTORS) / 75))
        .sum();

    let first_start = toc.tracks.first().map(|t| t.start_lba).unwrap_or(0);
    let total_secs = (toc.leadout_lba + PREGAP_SECTORS) / 75 - (first_start + PREGAP_SECTORS) / 75;
    let n = checksum % 0xFF;
    let track_count = toc.tracks.len() as u32;

    format!("{:08x}", (n << 24) | (total_secs << 8) | track_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dumo_core::AudioTrack;

    /// Build a TOC from (start_lba) values plus a lead-out.
    fn toc(starts: &[u32], leadout: u32) -> TocInfo {
        let tracks = starts
            .iter()
            .enumerate()
            .map(|(i, &start_lba)| AudioTrack {
                number: (i + 1) as u8,
                start_lba,
                length_sectors: None,
                is_data: false,
            })
            .collect();
        TocInfo {
            first_track: 1,
            last_track: starts.len() as u8,
            leadout_lba: leadout,
            tracks,
            musicbrainz_discid: None,
            freedb_discid: None,
        }
    }

    /// Reference case from the MusicBrainz "Disc ID Calculation" specification.
    ///
    /// The published example lists *offsets*, which already include the 150-sector
    /// pregap; a drive reports raw LBAs. We feed the raw LBAs (offset - 150) so this
    /// exercises our pregap handling too, alongside the hashing, offset ordering, and
    /// the custom base64 alphabet. If this passes, our IDs match MusicBrainz's database.
    ///
    /// Published offsets: first 1, last 6, tracks
    /// 150 / 15363 / 32314 / 46592 / 63414 / 80489, lead-out 95462.
    #[test]
    fn musicbrainz_matches_published_reference() {
        let offsets = [150u32, 15363, 32314, 46592, 63414, 80489];
        let starts: Vec<u32> = offsets.iter().map(|o| o - PREGAP_SECTORS).collect();
        let t = toc(&starts, 95462 - PREGAP_SECTORS);
        assert_eq!(musicbrainz_discid(&t), "49HHV7Eb8UKF3aQiNmu1GR8vKTY-");
    }

    #[test]
    fn freedb_id_is_stable_and_encodes_track_count() {
        let starts = [150, 16174, 34719];
        let t = toc(&starts, 50000);
        let id = freedb_discid(&t);
        assert_eq!(id.len(), 8);
        // Low byte is the track count.
        assert_eq!(u32::from_str_radix(&id[6..8], 16).unwrap(), 3);
    }

    #[test]
    fn discids_differ_for_different_discs() {
        let a = toc(&[150, 16174], 50000);
        let b = toc(&[150, 20000], 50000);
        assert_ne!(musicbrainz_discid(&a), musicbrainz_discid(&b));
        assert_ne!(freedb_discid(&a), freedb_discid(&b));
    }
}
