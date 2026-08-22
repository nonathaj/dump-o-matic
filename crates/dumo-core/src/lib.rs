//! Core domain model for dump-o-matic.
//!
//! This crate is deliberately free of I/O and platform specifics: it defines the
//! vocabulary that every stage, backend, and front-end speaks. Drive access lives in
//! `dumo-drives`, ripping backends in `dumo-backends`, and so on.

pub mod config;
pub mod disc;
pub mod drive;
pub mod fsops;
pub mod hash;
pub mod job;

pub use config::{Config, ConfigError};
pub use job::{Artifact, Identification, Job, JobStage, ReadyFile, TitleInfo};

pub use disc::{
    AudioTrack, ContentHint, DiscProbe, DiscProfile, GameSerial, MediaKind, TocInfo, VolumeInfo,
};
pub use drive::{Drive, DriveCapabilities, DriveStatus, TrayState};

/// Confidence in an automated determination.
///
/// Deliberately coarse. Per the project's identification policy, only [`Confidence::Exact`]
/// is ever eligible for unattended auto-accept; everything else requires confirmation.
/// A finer-grained numeric score would invite a tunable threshold, which is exactly what
/// the policy forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// No determination could be made.
    Unknown,
    /// A guess from weak signals (e.g. a volume label that looks like a title).
    Weak,
    /// Multiple independent signals agree, but nothing cryptographic.
    Strong,
    /// Cryptographic or structural certainty (hash match, disc ID match).
    Exact,
}

impl Confidence {
    /// Whether a result at this confidence may be accepted without human confirmation.
    ///
    /// Only exact matches qualify, by design.
    pub fn is_auto_acceptable(self) -> bool {
        matches!(self, Confidence::Exact)
    }
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Confidence::Unknown => "unknown",
            Confidence::Weak => "weak",
            Confidence::Strong => "strong",
            Confidence::Exact => "exact",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_is_auto_acceptable() {
        assert!(Confidence::Exact.is_auto_acceptable());
        assert!(!Confidence::Strong.is_auto_acceptable());
        assert!(!Confidence::Weak.is_auto_acceptable());
        assert!(!Confidence::Unknown.is_auto_acceptable());
    }

    #[test]
    fn confidence_orders_sensibly() {
        assert!(Confidence::Exact > Confidence::Strong);
        assert!(Confidence::Strong > Confidence::Weak);
        assert!(Confidence::Weak > Confidence::Unknown);
    }
}
