//! Adapters for external ripping tools.
//!
//! dump-o-matic does not implement disc I/O itself. It drives proven backends
//! (MakeMKV for video, redumper for game discs, cdparanoia for audio) as subprocesses
//! and parses their output into structured progress and results.

pub mod ffprobe;
pub mod makemkv;
pub mod redumper;
pub mod subtitles;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{tool} is not installed or not on PATH")]
    NotInstalled { tool: &'static str },

    #[error("{tool} failed: {detail}")]
    Failed { tool: &'static str, detail: String },

    #[error("{tool} reported an error: {message}")]
    Reported { tool: &'static str, message: String },

    #[error("i/o error running {tool}: {source}")]
    Io {
        tool: &'static str,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, BackendError>;
