//! Linux CDROM ioctl bindings.
//!
//! These are the cheap, universally-supported queries: tray state, coarse disc class,
//! and the table of contents. All are read-only.

use crate::{DriveError, Result};
use dumo_core::{AudioTrack, TocInfo, TrayState};
use std::os::unix::io::RawFd;

// From <linux/cdrom.h>
const CDROM_DRIVE_STATUS: libc::c_ulong = 0x5326;
const CDROM_DISC_STATUS: libc::c_ulong = 0x5327;
const CDROMREADTOCHDR: libc::c_ulong = 0x5305;
const CDROMREADTOCENTRY: libc::c_ulong = 0x5306;

const CDS_NO_INFO: libc::c_int = 0;
const CDS_NO_DISC: libc::c_int = 1;
const CDS_TRAY_OPEN: libc::c_int = 2;
const CDS_DRIVE_NOT_READY: libc::c_int = 3;
const CDS_DISC_OK: libc::c_int = 4;

const CDS_AUDIO: libc::c_int = 100;
const CDS_DATA_1: libc::c_int = 101;
const CDS_DATA_2: libc::c_int = 102;
const CDS_XA_2_1: libc::c_int = 103;
const CDS_XA_2_2: libc::c_int = 104;
const CDS_MIXED: libc::c_int = 105;

const CDROM_LBA: u8 = 0x01;
const CDROM_LEADOUT: u8 = 0xAA;

/// Coarse disc classification from the kernel, used as a cross-check on our own sniffing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelDiscClass {
    Audio,
    Data,
    Mixed,
    NoInfo,
}

#[repr(C)]
#[derive(Default)]
struct CdromTocHdr {
    trk0: u8,
    trk1: u8,
}

#[repr(C)]
#[derive(Default)]
struct CdromTocEntry {
    track: u8,
    /// `adr` in the low nibble, `ctrl` in the high nibble.
    adr_ctrl: u8,
    format: u8,
    addr: i32,
    datamode: u8,
}

/// `ctrl` bit 2 marks a data track.
const CTRL_DATA_TRACK: u8 = 0x04;

pub fn drive_status(fd: RawFd, path: &str) -> Result<TrayState> {
    // Argument 0 selects the current slot on changers; harmless on single-slot drives.
    let rc = unsafe { libc::ioctl(fd, CDROM_DRIVE_STATUS, 0 as libc::c_ulong) };
    if rc < 0 {
        return Err(DriveError::Io {
            path: path.to_string(),
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(match rc {
        CDS_NO_DISC => TrayState::NoDisc,
        CDS_TRAY_OPEN => TrayState::TrayOpen,
        CDS_DRIVE_NOT_READY => TrayState::NotReady,
        CDS_DISC_OK => TrayState::DiscOk,
        CDS_NO_INFO => TrayState::NotReady,
        _ => TrayState::NotReady,
    })
}

pub fn disc_class(fd: RawFd) -> KernelDiscClass {
    let rc = unsafe { libc::ioctl(fd, CDROM_DISC_STATUS, 0 as libc::c_ulong) };
    match rc {
        CDS_AUDIO => KernelDiscClass::Audio,
        CDS_DATA_1 | CDS_DATA_2 | CDS_XA_2_1 | CDS_XA_2_2 => KernelDiscClass::Data,
        CDS_MIXED => KernelDiscClass::Mixed,
        _ => KernelDiscClass::NoInfo,
    }
}

/// Read the full table of contents.
///
/// Only meaningful for CD media; DVD/BD drives will typically fail or report a single
/// synthetic track, which callers treat as "no TOC".
pub fn read_toc(fd: RawFd, path: &str) -> Result<TocInfo> {
    let mut hdr = CdromTocHdr::default();
    let rc = unsafe { libc::ioctl(fd, CDROMREADTOCHDR, &mut hdr as *mut CdromTocHdr) };
    if rc < 0 {
        return Err(DriveError::Command {
            path: path.to_string(),
            operation: "read TOC header",
            detail: std::io::Error::last_os_error().to_string(),
        });
    }

    let read_entry = |track: u8| -> Result<(u32, bool)> {
        let mut e = CdromTocEntry {
            track,
            format: CDROM_LBA,
            ..Default::default()
        };
        let rc = unsafe { libc::ioctl(fd, CDROMREADTOCENTRY, &mut e as *mut CdromTocEntry) };
        if rc < 0 {
            return Err(DriveError::Command {
                path: path.to_string(),
                operation: "read TOC entry",
                detail: format!(
                    "track {track}: {}",
                    std::io::Error::last_os_error()
                ),
            });
        }
        // LBA can be negative in the lead-in; clamp, since callers only use absolute
        // positions of real tracks.
        let lba = e.addr.max(0) as u32;
        let is_data = (e.adr_ctrl >> 4) & CTRL_DATA_TRACK != 0;
        Ok((lba, is_data))
    };

    let (leadout_lba, _) = read_entry(CDROM_LEADOUT)?;

    let mut tracks = Vec::new();
    for n in hdr.trk0..=hdr.trk1 {
        let (start_lba, is_data) = read_entry(n)?;
        tracks.push(AudioTrack {
            number: n,
            start_lba,
            length_sectors: None,
            is_data,
        });
    }

    // Track length is the gap to the next track's start, or to the lead-out for the last.
    for i in 0..tracks.len() {
        let next = tracks
            .get(i + 1)
            .map(|t| t.start_lba)
            .unwrap_or(leadout_lba);
        tracks[i].length_sectors = next.checked_sub(tracks[i].start_lba);
    }

    Ok(TocInfo {
        first_track: hdr.trk0,
        last_track: hdr.trk1,
        leadout_lba,
        tracks,
        musicbrainz_discid: None,
        freedb_discid: None,
    })
}
