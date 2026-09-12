//! Read-only inspection and extraction of console content from attached storage devices.
//!
//! The disc side of this tool asks "what is in the drive?"; this crate asks the same
//! question of a USB drive or memory card that a console has written to. The pipeline is
//! unchanged — probe, extract to staging, identify, migrate — but the medium is a
//! filesystem rather than a spiral of pits.
//!
//! Three properties are structural rather than conventional:
//!
//! - **Nothing here writes to the device.** Devices are opened `O_RDONLY` and the
//!   filesystem is parsed in-process rather than mounted, so the kernel never gets the
//!   chance to update a dirty bit on media we are preserving.
//! - **Layouts are detected, never assumed.** A console's storage layout is one of many a
//!   given device might hold, so detection is a registry of independent detectors
//!   ([`layout`]). A device nothing recognises is reported as unrecognised; it is never
//!   forced into the nearest matching shape.
//! - **Extraction is verified against the content's own hashes.** Xbox 360 packages carry
//!   a SHA-1 hash tree over every block. Extraction checks it as it reads, so a faithful
//!   copy is proven rather than assumed.

pub mod fat32;
pub mod god;
pub mod iso;
pub mod layout;
pub mod source;
pub mod storage;
pub mod xcontent;
pub mod xdvdfs;
pub mod xex;

pub use layout::{detect, Catalogue, CatalogueItem, ContentKind, Layout};
pub use source::{ContentSource, DirSource, Entry, Fat32Source, RandomRead};
pub use storage::{enumerate_devices, StorageDevice};

/// Errors from reading a storage device and the content on it.
#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("{path} not found")]
    NotFound { path: String },

    #[error(
        "permission denied opening {path}. Reading a block device directly needs \
         membership of the 'disk' group (log out and back in after being added), or run \
         as root. Alternatively, mount the device read-only and point this command at the \
         mount directory instead"
    )]
    PermissionDenied { path: String },

    #[error("{path} is not a FAT32 volume: {detail}")]
    NotFat32 { path: String, detail: String },

    #[error("{path} is structurally inconsistent: {detail}")]
    Corrupt { path: String, detail: String },

    #[error("{path} holds no content layout this tool recognises{}", match hint {
        Some(h) => format!(" ({h})"),
        None => String::new(),
    })]
    UnknownLayout { path: String, hint: Option<String> },

    #[error(
        "block {block} of {item} does not match the hash recorded in the package \
         (expected {expected}, computed {computed}); the source data is damaged and the \
         extraction has been abandoned rather than written out as if it were good"
    )]
    HashMismatch {
        item: String,
        block: u64,
        expected: String,
        computed: String,
    },

    #[error("{item} is not usable: {detail}")]
    Unsupported { item: String, detail: String },

    #[error("i/o error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, DeviceError>;
