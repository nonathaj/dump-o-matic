//! Content identification.
//!
//! Turns staged artifacts into confident answers about what they are. The only source
//! implemented so far is Redump/No-Intro datfile hash matching, which yields
//! [`dumo_core::Confidence::Exact`] — the sole confidence level eligible for unattended
//! acceptance.

pub mod datfile;
pub mod platform;

pub use datfile::{Datfile, DatfileSet, Game, Rom};
pub use platform::es_de_slug;

#[derive(Debug, thiserror::Error)]
pub enum IdentifyError {
    #[error("reading {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("parsing {path}: {detail}")]
    Parse {
        path: std::path::PathBuf,
        detail: String,
    },

    #[error("no datfiles configured; set datfiles.redump_dir in your config")]
    NoDatfiles,
}

pub type Result<T> = std::result::Result<T, IdentifyError>;
