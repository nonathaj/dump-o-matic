//! Optical drive identity and state.

use serde::{Deserialize, Serialize};

/// Physical state of the drive's tray and media.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayState {
    /// No disc present.
    NoDisc,
    /// Tray is open.
    TrayOpen,
    /// Drive is spinning up / not yet ready to answer.
    NotReady,
    /// Media present and readable.
    DiscOk,
}

impl std::fmt::Display for TrayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TrayState::NoDisc => "no disc",
            TrayState::TrayOpen => "tray open",
            TrayState::NotReady => "not ready",
            TrayState::DiscOk => "disc present",
        };
        f.write_str(s)
    }
}

/// What a drive is physically capable of reading.
///
/// Used to warn early when a disc type cannot be read by the selected drive, rather
/// than failing partway through a rip.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveCapabilities {
    pub reads_cd: bool,
    pub reads_dvd: bool,
    pub reads_bluray: bool,
    pub writes_cd: bool,
    pub writes_dvd: bool,
    pub writes_bluray: bool,
}

/// An optical drive attached to the system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drive {
    /// Device node, e.g. `/dev/sr0`.
    pub path: String,
    /// Kernel name, e.g. `sr0`.
    pub name: String,
    /// Vendor string reported by the device, if available.
    pub vendor: Option<String>,
    /// Model string reported by the device, if available.
    pub model: Option<String>,
    /// Firmware revision, if available.
    pub revision: Option<String>,
    pub capabilities: DriveCapabilities,
}

impl Drive {
    /// Human-readable one-line description of the hardware.
    pub fn description(&self) -> String {
        match (&self.vendor, &self.model) {
            (Some(v), Some(m)) => format!("{} {}", v.trim(), m.trim()),
            (None, Some(m)) => m.trim().to_string(),
            (Some(v), None) => v.trim().to_string(),
            (None, None) => "unknown drive".to_string(),
        }
    }
}

/// A drive plus its current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveStatus {
    pub drive: Drive,
    pub tray: TrayState,
}
