//! Disc-level probe results: what is in the drive, described as cheaply as possible.

use crate::Confidence;
use serde::Serialize;

/// MMC "current profile" — what kind of physical medium this is.
///
/// Reported by the drive itself via GET CONFIGURATION, so it is authoritative about the
/// medium (unlike filesystem sniffing, which describes only the content).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscProfile {
    CdRom,
    CdR,
    CdRw,
    DvdRom,
    DvdR,
    DvdRam,
    DvdRw,
    DvdPlusR,
    DvdPlusRw,
    DvdPlusRDl,
    BdRom,
    BdR,
    BdRe,
    Unknown(u16),
}

impl DiscProfile {
    /// Map an MMC profile number to a profile.
    pub fn from_mmc(code: u16) -> Self {
        match code {
            0x0008 => DiscProfile::CdRom,
            0x0009 => DiscProfile::CdR,
            0x000A => DiscProfile::CdRw,
            0x0010 => DiscProfile::DvdRom,
            0x0011 => DiscProfile::DvdR,
            0x0012 => DiscProfile::DvdRam,
            0x0013 | 0x0014 => DiscProfile::DvdRw,
            0x001B => DiscProfile::DvdPlusR,
            0x001A => DiscProfile::DvdPlusRw,
            0x002B => DiscProfile::DvdPlusRDl,
            0x0040 => DiscProfile::BdRom,
            0x0041 | 0x0042 => DiscProfile::BdR,
            0x0043 => DiscProfile::BdRe,
            other => DiscProfile::Unknown(other),
        }
    }

    /// The physical family, useful for picking a ripping backend.
    pub fn family(self) -> &'static str {
        match self {
            DiscProfile::CdRom | DiscProfile::CdR | DiscProfile::CdRw => "CD",
            DiscProfile::DvdRom
            | DiscProfile::DvdR
            | DiscProfile::DvdRam
            | DiscProfile::DvdRw
            | DiscProfile::DvdPlusR
            | DiscProfile::DvdPlusRw
            | DiscProfile::DvdPlusRDl => "DVD",
            DiscProfile::BdRom | DiscProfile::BdR | DiscProfile::BdRe => "Blu-ray",
            DiscProfile::Unknown(_) => "unknown",
        }
    }
}

impl std::fmt::Display for DiscProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DiscProfile::CdRom => "CD-ROM",
            DiscProfile::CdR => "CD-R",
            DiscProfile::CdRw => "CD-RW",
            DiscProfile::DvdRom => "DVD-ROM",
            DiscProfile::DvdR => "DVD-R",
            DiscProfile::DvdRam => "DVD-RAM",
            DiscProfile::DvdRw => "DVD-RW",
            DiscProfile::DvdPlusR => "DVD+R",
            DiscProfile::DvdPlusRw => "DVD+RW",
            DiscProfile::DvdPlusRDl => "DVD+R DL",
            DiscProfile::BdRom => "BD-ROM",
            DiscProfile::BdR => "BD-R",
            DiscProfile::BdRe => "BD-RE",
            DiscProfile::Unknown(c) => return write!(f, "unknown profile 0x{c:04X}"),
        };
        f.write_str(s)
    }
}

/// What kind of *content* the disc holds, as distinct from the medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// Red Book audio CD.
    AudioCd,
    /// Audio and data tracks on one disc.
    MixedMode,
    /// DVD-Video (has a VIDEO_TS structure).
    DvdVideo,
    /// Blu-ray video (has a BDMV structure).
    BluRayVideo,
    /// A game disc for a recognised console.
    GameDisc,
    /// Readable data disc of no recognised special kind.
    Data,
    /// Nothing readable.
    Blank,
    Unknown,
}

impl std::fmt::Display for MediaKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            MediaKind::AudioCd => "Audio CD",
            MediaKind::MixedMode => "Mixed-mode CD",
            MediaKind::DvdVideo => "DVD-Video",
            MediaKind::BluRayVideo => "Blu-ray Video",
            MediaKind::GameDisc => "Game disc",
            MediaKind::Data => "Data disc",
            MediaKind::Blank => "Blank",
            MediaKind::Unknown => "Unknown",
        };
        f.write_str(s)
    }
}

/// One track from a CD table of contents.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct AudioTrack {
    pub number: u8,
    /// Absolute start sector (LBA).
    pub start_lba: u32,
    /// Length in sectors, if computable.
    pub length_sectors: Option<u32>,
    /// True for a data track in a mixed-mode disc.
    pub is_data: bool,
}

impl AudioTrack {
    /// Duration in seconds, at the Red Book rate of 75 sectors per second.
    pub fn duration_secs(&self) -> Option<f64> {
        self.length_sectors.map(|len| f64::from(len) / 75.0)
    }
}

/// CD table of contents, plus identifiers derived from it.
#[derive(Debug, Clone, Serialize)]
pub struct TocInfo {
    pub first_track: u8,
    pub last_track: u8,
    /// Lead-out position, needed for disc-ID computation.
    pub leadout_lba: u32,
    pub tracks: Vec<AudioTrack>,
    /// MusicBrainz Disc ID (base64-ish, 28 chars). An exact match against MusicBrainz
    /// identifies the pressing outright.
    pub musicbrainz_discid: Option<String>,
    /// FreeDB/CDDB disc ID, retained because some sources still key on it.
    pub freedb_discid: Option<String>,
}

impl TocInfo {
    /// Total audio duration in seconds.
    pub fn total_duration_secs(&self) -> f64 {
        f64::from(self.leadout_lba.saturating_sub(
            self.tracks.first().map(|t| t.start_lba).unwrap_or(0),
        )) / 75.0
    }

    pub fn audio_track_count(&self) -> usize {
        self.tracks.iter().filter(|t| !t.is_data).count()
    }
}

/// ISO 9660 / UDF primary volume descriptor fields.
#[derive(Debug, Clone, Default, Serialize)]
pub struct VolumeInfo {
    pub volume_id: Option<String>,
    pub volume_set_id: Option<String>,
    pub publisher_id: Option<String>,
    pub application_id: Option<String>,
    /// Creation timestamp as recorded on the disc, raw form.
    pub created: Option<String>,
    /// Total sectors as declared by the volume descriptor.
    pub volume_space_size: Option<u32>,
    pub logical_block_size: Option<u16>,
}

/// A console game serial recovered from a disc.
#[derive(Debug, Clone, Serialize)]
pub struct GameSerial {
    /// e.g. `SLUS-20488`.
    pub serial: String,
    /// Platform slug in ES-DE / EmuDeck vocabulary, e.g. `ps2`.
    pub platform: String,
    /// Where the serial came from, for auditability.
    pub evidence: String,
}

/// A best-effort guess at what the disc contains, with its supporting evidence.
///
/// This is Stage 1 output: fast, and explicitly *not* authoritative. Stage 3 does the
/// real identification against datfiles and metadata services.
#[derive(Debug, Clone, Serialize)]
pub struct ContentHint {
    pub kind: MediaKind,
    /// Human-readable guess at the title, if one is available cheaply.
    pub title_guess: Option<String>,
    pub confidence: Confidence,
    /// Why the tool believes this, in plain language.
    pub evidence: Vec<String>,
}

impl ContentHint {
    pub fn new(kind: MediaKind, confidence: Confidence) -> Self {
        Self {
            kind,
            title_guess: None,
            confidence,
            evidence: Vec::new(),
        }
    }

    pub fn with_evidence(mut self, e: impl Into<String>) -> Self {
        self.evidence.push(e.into());
        self
    }

    pub fn with_title(mut self, t: impl Into<String>) -> Self {
        self.title_guess = Some(t.into());
        self
    }
}

/// The complete result of a Stage 1 fast probe.
#[derive(Debug, Clone, Serialize)]
pub struct DiscProbe {
    pub device: String,
    pub profile: Option<DiscProfile>,
    pub content: ContentHint,
    pub volume: Option<VolumeInfo>,
    pub toc: Option<TocInfo>,
    pub game_serial: Option<GameSerial>,
    /// Capacity in bytes, from the medium rather than the filesystem.
    pub capacity_bytes: Option<u64>,
    /// Top-level entries in the root directory, for eyeballing an unrecognised disc.
    pub root_entries: Vec<String>,
    /// How long the probe took; Stage 1 is supposed to be fast, so this is worth showing.
    pub probe_millis: u128,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmc_profiles_map_to_families() {
        assert_eq!(DiscProfile::from_mmc(0x0008), DiscProfile::CdRom);
        assert_eq!(DiscProfile::from_mmc(0x0010), DiscProfile::DvdRom);
        assert_eq!(DiscProfile::from_mmc(0x0040), DiscProfile::BdRom);
        assert_eq!(DiscProfile::from_mmc(0x0010).family(), "DVD");
        assert_eq!(DiscProfile::from_mmc(0x0040).family(), "Blu-ray");
        assert_eq!(DiscProfile::from_mmc(0x0008).family(), "CD");
    }

    #[test]
    fn unknown_profile_preserves_code() {
        match DiscProfile::from_mmc(0xABCD) {
            DiscProfile::Unknown(c) => assert_eq!(c, 0xABCD),
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[test]
    fn track_duration_uses_red_book_rate() {
        let t = AudioTrack {
            number: 1,
            start_lba: 0,
            length_sectors: Some(75 * 30),
            is_data: false,
        };
        assert_eq!(t.duration_secs(), Some(30.0));
    }
}
