//! Optical drive detection and **read-only** disc probing.
//!
//! Everything in this crate is non-destructive: it opens devices read-only, and issues
//! only informational SCSI/MMC commands and reads. Nothing here writes to a disc, ejects
//! a tray, or modifies the filesystem.
//!
//! Platform support is currently Linux-only; the public API is platform-neutral so other
//! backends can be added without changing callers.

mod device;
mod discid;
mod ioctl;
mod iso9660;
mod mmc;
mod probe;

pub use device::{enumerate_drives, open_drive_status, read_drive};
pub use probe::probe_disc;

/// Errors from drive access and probing.
#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    #[error("device {path} not found")]
    NotFound { path: String },

    #[error("permission denied opening {path} (is your user in the 'cdrom' group?)")]
    PermissionDenied { path: String },

    #[error("no disc present in {path}")]
    NoDisc { path: String },

    #[error(
        "a disc is loaded in {path} but cannot be read: the drive reports media present, \
         yet its table of contents does not come back. Common causes: the disc is upside \
         down, its inner ring near the hub is dirty or scratched, or it is blank/unfinalised"
    )]
    DiscUnreadable { path: String },

    #[error("drive {path} is not ready (still spinning up?)")]
    NotReady { path: String },

    #[error("tray is open on {path}")]
    TrayOpen { path: String },

    #[error("i/o error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{operation} failed on {path}: {detail}")]
    Command {
        path: String,
        operation: &'static str,
        detail: String,
    },
}

pub type Result<T> = std::result::Result<T, DriveError>;
