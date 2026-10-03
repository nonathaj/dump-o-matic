//! Stage 1: the fast, read-only probe.
//!
//! Goal is to answer "what is in the drive?" in seconds, reading only a handful of
//! sectors. Nothing here is authoritative — Stage 3 does real identification against
//! datfiles and metadata services. Everything reported carries its evidence so the
//! operator can judge it.

use crate::{device, discid, ioctl, iso9660, mmc, udf, DriveError, Result};
use dumo_core::{
    Confidence, ContentHint, DiscProbe, DiscProfile, GameSerial, MediaKind, TrayState,
};
use std::io::{Read, Seek};
use std::os::unix::io::AsRawFd;

/// `SYSTEM.CNF` is a few hundred bytes; anything larger is not the file we want.
const MAX_CONFIG_FILE: u32 = 8 * 1024;

/// Marker in the ISO 9660 application identifier of an original Xbox disc.
///
/// Xbox discs carry two partitions: a small DVD-Video partition holding the "for Xbox
/// only" warning clip, and an XDVDFS partition with the game, deliberately placed
/// outside the range a standard drive will address. Only the first is visible to us, and
/// it is a structurally valid DVD-Video — `VIDEO_TS` in the root and all — so without
/// this marker it classifies as a feature film whose title is the pressing date code.
const XBOX_VIDEO_PARTITION_MARKER: &str = "VTC Sector Offset";

/// Volume label prefix on an Xbox 360 game disc's video partition.
///
/// "XGD" is Xbox Game Disc, and the label names the generation: `XGD2DVD_NTSC`,
/// `XGD3DVD_NTSC`. Unlike original Xbox discs these carry no application identifier at
/// all, so the label is the only structural signal — which is why size alone must also
/// be enough to withhold confidence.
const XBOX_360_LABEL_PREFIX: &str = "XGD";

/// A DVD-Video feature runs to gigabytes; an Xbox warning clip is tens of megabytes.
///
/// Used only to lower confidence in a `VIDEO_TS` disc too small to be a real feature,
/// never on its own to call something an Xbox disc — a short promo DVD is also small.
const MIN_PLAUSIBLE_DVD_VIDEO_SECTORS: u32 = 512 * 1024;

/// Probe whichever disc is currently in `device_path`.
pub fn probe_disc(device_path: &str) -> Result<DiscProbe> {
    let started = std::time::Instant::now();
    let f = device::open_device(device_path)?;
    let fd = f.as_raw_fd();

    match ioctl::drive_status(fd, device_path)? {
        TrayState::DiscOk => {}
        TrayState::NoDisc => {
            return Err(DriveError::NoDisc {
                path: device_path.to_string(),
            })
        }
        TrayState::TrayOpen => {
            return Err(DriveError::TrayOpen {
                path: device_path.to_string(),
            })
        }
        TrayState::NotReady => {
            return Err(DriveError::NotReady {
                path: device_path.to_string(),
            })
        }
        TrayState::DiscUnreadable => {
            return Err(DriveError::DiscUnreadable {
                path: device_path.to_string(),
                medium: mmc::current_profile(fd, device_path)
                    .ok()
                    .map(|p| p.to_string()),
            })
        }
    }

    // The medium type comes from the drive itself and is the most trustworthy signal.
    // Read before checking readability: GET CONFIGURATION answers from the drive, not
    // the disc, so it still works on a disc nothing can read — and it is the most useful
    // thing to report in that case. A recognised profile means the format is understood
    // and the disc is damaged or dirty; no profile at all means the drive does not
    // understand the format, which cleaning will never fix.
    let profile = mmc::current_profile(fd, device_path).ok();

    // `DiscOk` is only the drive's belief that media is loaded. Everything below assumes
    // the disc can be read, so settle that here rather than surfacing a bare ENOMEDIUM
    // from whichever read happens to run first.
    device::medium_is_readable(device_path, profile.map(|p| p.to_string()))?;

    let capacity_bytes = mmc::read_capacity(fd, device_path).ok();
    let kernel_class = ioctl::disc_class(fd);

    // A meaningful TOC only exists on CD media. DVD/BD drives synthesise a single-track
    // TOC covering the whole disc; reading it would let us compute a MusicBrainz disc ID
    // that looks authoritative but identifies nothing. Only read a TOC when the medium is
    // actually a CD, so downstream stages never see a bogus "exact" identifier.
    let is_cd = profile.map(|p| p.family() == "CD").unwrap_or(false)
        || matches!(
            kernel_class,
            ioctl::KernelDiscClass::Audio | ioctl::KernelDiscClass::Mixed
        );

    let mut toc = if is_cd {
        ioctl::read_toc(fd, device_path).ok()
    } else {
        None
    };
    if let Some(t) = toc.as_mut() {
        // Disc IDs are only valid where there is at least one audio track.
        if t.audio_track_count() > 0 {
            t.musicbrainz_discid = Some(discid::musicbrainz_discid(t));
            t.freedb_discid = Some(discid::freedb_discid(t));
        }
    }

    // Re-open blocking for data reads; O_NONBLOCK is only needed to avoid hanging on open.
    let mut reader = std::fs::File::open(device_path).map_err(|e| {
        if e.raw_os_error() == Some(libc::ENOMEDIUM) {
            DriveError::DiscUnreadable {
                path: device_path.to_string(),
                medium: profile.map(|p| p.to_string()),
            }
        } else {
            DriveError::Io {
                path: device_path.to_string(),
                source: e,
            }
        }
    })?;

    let volume = iso9660::read_volume(&mut reader).ok().flatten();
    let is_udf = iso9660::detect_udf(&mut reader);

    let mut root_entries = Vec::new();
    let mut dir_entries = Vec::new();
    if let Some(v) = &volume {
        if let Ok(entries) = iso9660::read_dir(&mut reader, v.root_extent, v.root_size) {
            root_entries = entries
                .iter()
                .map(|e| {
                    if e.is_dir {
                        format!("{}/", e.name)
                    } else {
                        e.name.clone()
                    }
                })
                .collect();
            dir_entries = entries;
        }
    }
    // Blu-ray video discs are UDF without an ISO 9660 bridge, so the above never finds
    // anything — read the UDF root directly instead.
    if root_entries.is_empty() && is_udf {
        if let Ok(Some(entries)) = udf::read_root_entries(&mut reader) {
            root_entries = entries
                .iter()
                .map(|e| if e.is_dir { format!("{}/", e.name) } else { e.name.clone() })
                .collect();
        }
    }

    let game_serial = detect_game_serial(&mut reader, &dir_entries);
    let content = classify(
        profile,
        kernel_class,
        &toc,
        volume.as_ref().map(|v| &v.info),
        &root_entries,
        game_serial.as_ref(),
        is_udf,
    );

    Ok(DiscProbe {
        device: device_path.to_string(),
        profile,
        content,
        volume: volume.map(|v| v.info),
        toc,
        game_serial,
        capacity_bytes,
        root_entries,
        probe_millis: started.elapsed().as_millis(),
    })
}

/// Recover a PlayStation game serial from `SYSTEM.CNF`.
///
/// PS1 and PS2 discs both carry a `SYSTEM.CNF` naming the boot executable, whose filename
/// *is* the serial (`SLUS_203.12` → `SLUS-20312`). The `BOOT2` key marks PS2, `BOOT` marks
/// PS1, which also gives us the platform for free.
fn detect_game_serial<R: Read + Seek>(
    reader: &mut R,
    entries: &[iso9660::DirEntry],
) -> Option<GameSerial> {
    let cnf = entries
        .iter()
        .find(|e| !e.is_dir && e.name.eq_ignore_ascii_case("SYSTEM.CNF"))?;
    let data = iso9660::read_small_file(reader, cnf, MAX_CONFIG_FILE).ok()?;
    let text = String::from_utf8_lossy(&data);

    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_uppercase();
        let platform = match key.as_str() {
            "BOOT2" => "ps2",
            "BOOT" => "psx",
            _ => continue,
        };
        // e.g. "cdrom0:\SLUS_203.12;1"
        let raw = value
            .trim()
            .rsplit(['\\', '/', ':'])
            .next()?
            .split(';')
            .next()?
            .trim();
        if let Some(serial) = normalise_serial(raw) {
            return Some(GameSerial {
                serial,
                platform: platform.to_string(),
                evidence: format!("SYSTEM.CNF {key}={}", raw),
            });
        }
    }
    None
}

/// `SLUS_203.12` → `SLUS-20312`, the Redump serial form.
fn normalise_serial(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
        .collect();
    let (prefix, rest) = cleaned.split_once('_')?;
    if prefix.len() < 3 || !prefix.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
    if digits.len() < 4 {
        return None;
    }
    Some(format!("{}-{}", prefix.to_ascii_uppercase(), digits))
}

/// Decide what the disc holds, recording the evidence for each conclusion.
#[allow(clippy::too_many_arguments)]
fn classify(
    profile: Option<DiscProfile>,
    kernel_class: ioctl::KernelDiscClass,
    toc: &Option<dumo_core::TocInfo>,
    volume: Option<&dumo_core::VolumeInfo>,
    root_entries: &[String],
    game_serial: Option<&GameSerial>,
    is_udf: bool,
) -> ContentHint {
    let has = |needle: &str| {
        root_entries
            .iter()
            .any(|e| e.trim_end_matches('/').eq_ignore_ascii_case(needle))
    };

    // A recognised console serial is structural evidence, not a guess.
    if let Some(g) = game_serial {
        return ContentHint::new(MediaKind::GameDisc, Confidence::Strong)
            .with_title(g.serial.clone())
            .with_evidence(format!("{} ({})", g.evidence, g.platform))
            .with_evidence("serial still needs a datfile match to identify the title");
    }

    if has("VIDEO_TS") {
        let app = volume
            .and_then(|v| v.application_id.as_deref())
            .unwrap_or_default();
        let sectors = volume.and_then(|v| v.volume_space_size);
        let too_small_for_a_feature = sectors
            .map(|s| s < MIN_PLAUSIBLE_DVD_VIDEO_SECTORS)
            .unwrap_or(false);

        // The application identifier is the decisive signal. Size alone is not: a short
        // promo DVD is also small, so it only ever lowers confidence below.
        if app.contains(XBOX_VIDEO_PARTITION_MARKER) {
            let mut hint = ContentHint::new(MediaKind::XboxGameDisc, Confidence::Strong)
                .with_evidence(format!(
                    "ISO 9660 application identifier {app:?} marks the Xbox video partition"
                ));
            if let Some(s) = sectors {
                hint = hint.with_evidence(format!(
                    "visible volume is {s} sectors ({:.1} MB) — the warning clip, not the game",
                    (u64::from(s) * 2048) as f64 / 1_000_000.0
                ));
            }
            // Deliberately no title guess: the volume label is a pressing date code
            // (e.g. "SEP13011042"), and offering it as a title invites filing the disc
            // under a meaningless name.
            if let Some(label) = volume.and_then(|v| v.volume_id.as_ref()) {
                hint = hint.with_evidence(format!(
                    "volume label {label:?} is a pressing date code, not a title"
                ));
            }
            return hint.with_evidence(
                "the game is in an XDVDFS partition a standard drive cannot address; \
                 dumping needs a Kreon-firmware drive (TSSTcorp SH-D162/D163) or a \
                 softmodded console",
            );
        }

        // Xbox 360 discs announce their own generation in the volume label and carry no
        // application identifier, so they need their own signal.
        if let Some(label) = volume.and_then(|v| v.volume_id.as_deref()) {
            if label.to_ascii_uppercase().starts_with(XBOX_360_LABEL_PREFIX) {
                let generation = label
                    .chars()
                    .skip(XBOX_360_LABEL_PREFIX.len())
                    .take_while(char::is_ascii_digit)
                    .collect::<String>();
                let mut hint = ContentHint::new(MediaKind::Xbox360GameDisc, Confidence::Strong)
                    .with_evidence(format!(
                        "volume label {label:?} identifies an Xbox Game Disc, not a title"
                    ));
                if let Some(s) = sectors {
                    hint = hint.with_evidence(format!(
                        "visible volume is {s} sectors ({:.1} MB) — the warning clip, not the game",
                        (u64::from(s) * 2048) as f64 / 1_000_000.0
                    ));
                }
                hint = hint.with_evidence(match generation.as_str() {
                    "2" => "XGD2: the game partition needs a Kreon-firmware drive \
                            (TSSTcorp SH-D162/D163)"
                        .to_string(),
                    "3" => "XGD3: the game partition needs a Kreon-firmware drive, and XGD3 \
                            is the harder case — confirm the drive and method before relying \
                            on a dump"
                        .to_string(),
                    other => format!(
                        "XGD generation {other:?} not recognised; dumping needs a \
                         Kreon-firmware drive"
                    ),
                });
                return hint;
            }
        }

        let confidence = if too_small_for_a_feature {
            Confidence::Weak
        } else {
            Confidence::Strong
        };
        let mut hint = ContentHint::new(MediaKind::DvdVideo, confidence)
            .with_evidence("VIDEO_TS directory present in root");
        if too_small_for_a_feature {
            if let Some(s) = sectors {
                hint = hint.with_evidence(format!(
                    "volume is only {s} sectors ({:.1} MB), too small for a feature — \
                     this may be the video partition of a console game disc",
                    (u64::from(s) * 2048) as f64 / 1_000_000.0
                ));
            }
        }
        if let Some(label) = volume.and_then(|v| v.volume_id.as_ref()) {
            hint = hint
                .with_title(label.clone())
                .with_evidence(format!("volume label {label:?} used as title guess"));
        }
        return hint;
    }

    if has("BDMV") {
        let mut hint = ContentHint::new(MediaKind::BluRayVideo, Confidence::Strong)
            .with_evidence("BDMV directory present in root");
        if let Some(label) = volume.and_then(|v| v.volume_id.as_ref()) {
            hint = hint.with_title(label.clone());
        }
        return hint;
    }

    match kernel_class {
        ioctl::KernelDiscClass::Audio => {
            let tracks = toc.as_ref().map(|t| t.audio_track_count()).unwrap_or(0);
            let mut hint = ContentHint::new(MediaKind::AudioCd, Confidence::Strong)
                .with_evidence(format!("kernel reports audio disc, {tracks} audio tracks"));
            if let Some(id) = toc.as_ref().and_then(|t| t.musicbrainz_discid.as_ref()) {
                hint = hint.with_evidence(format!(
                    "MusicBrainz disc ID {id} (exact lookup available in stage 3)"
                ));
            }
            return hint;
        }
        ioctl::KernelDiscClass::Mixed => {
            return ContentHint::new(MediaKind::MixedMode, Confidence::Strong)
                .with_evidence("kernel reports mixed audio+data disc");
        }
        _ => {}
    }

    if volume.is_some() || is_udf {
        let fs = match (volume.is_some(), is_udf) {
            (true, true) => "ISO 9660 + UDF",
            (true, false) => "ISO 9660",
            _ => "UDF",
        };
        let mut hint = ContentHint::new(MediaKind::Data, Confidence::Weak)
            .with_evidence(format!("{fs} filesystem, no recognised content structure"));
        if let Some(label) = volume.and_then(|v| v.volume_id.as_ref()) {
            hint = hint
                .with_title(label.clone())
                .with_evidence(format!("volume label {label:?}"));
        }
        if let Some(p) = profile {
            hint = hint.with_evidence(format!("medium is {p}"));
        }
        return hint;
    }

    ContentHint::new(MediaKind::Unknown, Confidence::Unknown)
        .with_evidence("no readable filesystem or table of contents")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_playstation_serials() {
        assert_eq!(normalise_serial("SLUS_203.12").as_deref(), Some("SLUS-20312"));
        assert_eq!(normalise_serial("SCUS_971.13").as_deref(), Some("SCUS-97113"));
        assert_eq!(normalise_serial("slps_015.55").as_deref(), Some("SLPS-01555"));
    }

    #[test]
    fn rejects_non_serials() {
        assert_eq!(normalise_serial("README.TXT"), None);
        assert_eq!(normalise_serial("A_12"), None);
        assert_eq!(normalise_serial("NOUNDERSCORE"), None);
    }

    #[test]
    fn video_ts_classifies_as_dvd_video() {
        let hint = classify(
            Some(DiscProfile::DvdRom),
            ioctl::KernelDiscClass::Data,
            &None,
            None,
            &["VIDEO_TS/".to_string(), "AUDIO_TS/".to_string()],
            None,
            true,
        );
        assert_eq!(hint.kind, MediaKind::DvdVideo);
        assert_eq!(hint.confidence, Confidence::Strong);
        assert!(!hint.evidence.is_empty());
    }

    #[test]
    fn game_serial_wins_over_generic_data() {
        let g = GameSerial {
            serial: "SLUS-20488".into(),
            platform: "ps2".into(),
            evidence: "SYSTEM.CNF BOOT2=cdrom0:\\SLUS_204.88;1".into(),
        };
        let hint = classify(
            Some(DiscProfile::DvdRom),
            ioctl::KernelDiscClass::Data,
            &None,
            None,
            &["SYSTEM.CNF".to_string()],
            Some(&g),
            false,
        );
        assert_eq!(hint.kind, MediaKind::GameDisc);
        assert_eq!(hint.title_guess.as_deref(), Some("SLUS-20488"));
    }

    /// Values taken from a real original Xbox disc, pressed 2001-09-13. The title is
    /// unknown and unknowable from this partition, which is the point of the test.
    fn xbox_volume() -> dumo_core::VolumeInfo {
        dumo_core::VolumeInfo {
            volume_id: Some("SEP13011042".into()),
            volume_set_id: None,
            publisher_id: None,
            application_id: Some("Session Offset : 0 VTC Sector Offset: 0".into()),
            created: None,
            volume_space_size: Some(6992),
            logical_block_size: Some(2048),
        }
    }

    #[test]
    fn xbox_video_partition_is_not_a_dvd_video() {
        let v = xbox_volume();
        let hint = classify(
            Some(DiscProfile::DvdRom),
            ioctl::KernelDiscClass::Data,
            &None,
            Some(&v),
            &["VIDEO_TS/".to_string()],
            None,
            false,
        );
        assert_eq!(hint.kind, MediaKind::XboxGameDisc);
        // The pressing date code must never be offered as a title.
        assert_eq!(hint.title_guess, None);
        let ev = hint.evidence.join(" | ");
        assert!(ev.contains("VTC Sector Offset"), "{ev}");
        assert!(ev.contains("Kreon"), "{ev}");
    }

    #[test]
    fn a_real_dvd_video_is_still_strong_and_keeps_its_title() {
        let v = dumo_core::VolumeInfo {
            volume_id: Some("THE_THIN_RED_LINE".into()),
            volume_set_id: None,
            publisher_id: None,
            application_id: None,
            created: None,
            // ~7.9 GB, a dual-layer feature.
            volume_space_size: Some(3_800_000),
            logical_block_size: Some(2048),
        };
        let hint = classify(
            Some(DiscProfile::DvdRom),
            ioctl::KernelDiscClass::Data,
            &None,
            Some(&v),
            &["VIDEO_TS/".to_string(), "AUDIO_TS/".to_string()],
            None,
            true,
        );
        assert_eq!(hint.kind, MediaKind::DvdVideo);
        assert_eq!(hint.confidence, Confidence::Strong);
        assert_eq!(hint.title_guess.as_deref(), Some("THE_THIN_RED_LINE"));
    }

    #[test]
    fn xbox_360_is_recognised_from_its_volume_label() {
        // Values from a real XGD2 disc. Note the absent application_id: the original
        // Xbox marker is no help here, so the label has to carry it.
        let v = dumo_core::VolumeInfo {
            volume_id: Some("XGD2DVD_NTSC".into()),
            volume_set_id: None,
            publisher_id: None,
            application_id: None,
            created: None,
            volume_space_size: Some(2724),
            logical_block_size: Some(2048),
        };
        let hint = classify(
            Some(DiscProfile::DvdRom),
            ioctl::KernelDiscClass::Data,
            &None,
            Some(&v),
            &["AUDIO_TS/".to_string(), "VIDEO_TS/".to_string()],
            None,
            false,
        );
        assert_eq!(hint.kind, MediaKind::Xbox360GameDisc);
        assert_eq!(hint.title_guess, None);
        let ev = hint.evidence.join(" | ");
        assert!(ev.contains("XGD2"), "{ev}");
        assert!(ev.contains("Kreon"), "{ev}");
    }

    #[test]
    fn an_unrecognised_xgd_generation_still_refuses_to_guess_a_title() {
        let v = dumo_core::VolumeInfo {
            volume_id: Some("XGD9DVD_PAL".into()),
            volume_set_id: None,
            publisher_id: None,
            application_id: None,
            created: None,
            volume_space_size: Some(3000),
            logical_block_size: Some(2048),
        };
        let hint = classify(
            Some(DiscProfile::DvdRom),
            ioctl::KernelDiscClass::Data,
            &None,
            Some(&v),
            &["VIDEO_TS/".to_string()],
            None,
            false,
        );
        assert_eq!(hint.kind, MediaKind::Xbox360GameDisc);
        assert_eq!(hint.title_guess, None);
        assert!(
            hint.evidence.join(" | ").contains("not recognised"),
            "{:?}",
            hint.evidence
        );
    }

    #[test]
    fn a_tiny_video_ts_disc_is_not_claimed_with_strong_confidence() {
        // Same size as the Xbox partition but without the marker: we do not know what
        // this is, so it stays DVD-Video with the doubt recorded rather than guessing.
        let mut v = xbox_volume();
        v.application_id = None;
        let hint = classify(
            Some(DiscProfile::DvdRom),
            ioctl::KernelDiscClass::Data,
            &None,
            Some(&v),
            &["VIDEO_TS/".to_string()],
            None,
            false,
        );
        assert_eq!(hint.kind, MediaKind::DvdVideo);
        assert_eq!(hint.confidence, Confidence::Weak);
        assert!(
            hint.evidence.join(" | ").contains("too small for a feature"),
            "{:?}",
            hint.evidence
        );
    }

    #[test]
    fn unreadable_disc_is_unknown_not_a_guess() {
        let hint = classify(
            None,
            ioctl::KernelDiscClass::NoInfo,
            &None,
            None,
            &[],
            None,
            false,
        );
        assert_eq!(hint.kind, MediaKind::Unknown);
        assert_eq!(hint.confidence, Confidence::Unknown);
    }
}
