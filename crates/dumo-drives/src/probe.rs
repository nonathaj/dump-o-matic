//! Stage 1: the fast, read-only probe.
//!
//! Goal is to answer "what is in the drive?" in seconds, reading only a handful of
//! sectors. Nothing here is authoritative — Stage 3 does real identification against
//! datfiles and metadata services. Everything reported carries its evidence so the
//! operator can judge it.

use crate::{device, discid, ioctl, iso9660, mmc, DriveError, Result};
use dumo_core::{
    Confidence, ContentHint, DiscProbe, DiscProfile, GameSerial, MediaKind, TrayState,
};
use std::io::{Read, Seek};
use std::os::unix::io::AsRawFd;

/// `SYSTEM.CNF` is a few hundred bytes; anything larger is not the file we want.
const MAX_CONFIG_FILE: u32 = 8 * 1024;

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
    }

    // The medium type comes from the drive itself and is the most trustworthy signal.
    let profile = mmc::current_profile(fd, device_path).ok();
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
    let mut reader = std::fs::File::open(device_path).map_err(|e| DriveError::Io {
        path: device_path.to_string(),
        source: e,
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
        let mut hint = ContentHint::new(MediaKind::DvdVideo, Confidence::Strong)
            .with_evidence("VIDEO_TS directory present in root");
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
