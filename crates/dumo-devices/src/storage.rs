//! Enumeration of attached storage devices.
//!
//! The optical side of the tool lists drives; this lists the block devices that might hold
//! console content. Only removable, non-optical whole disks are offered: the point is to
//! surface the USB drive someone has just plugged in, without ever presenting the system's
//! own disks as candidates for inspection.

use std::path::Path;

/// A block device that could hold console content.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StorageDevice {
    /// Device node, e.g. `/dev/sdd`.
    pub path: String,
    /// Kernel name, e.g. `sdd`.
    pub name: String,
    pub size_bytes: u64,
    pub removable: bool,
    pub vendor: Option<String>,
    pub model: Option<String>,
    /// Transport as the kernel reports it, e.g. `usb`.
    pub transport: Option<String>,
    /// Whether this process can actually read the device.
    ///
    /// Reported rather than discovered on use, so `devices` can explain the permission
    /// problem while listing the device instead of failing at extraction time.
    pub readable: bool,
}

impl StorageDevice {
    pub fn description(&self) -> String {
        match (&self.vendor, &self.model) {
            (Some(v), Some(m)) => format!("{v} {m}"),
            (None, Some(m)) => m.clone(),
            (Some(v), None) => v.clone(),
            (None, None) => "unknown device".to_string(),
        }
    }
}

fn sysfs(name: &str, attr: &str) -> Option<String> {
    std::fs::read_to_string(format!("/sys/block/{name}/{attr}"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn sysfs_device(name: &str, attr: &str) -> Option<String> {
    std::fs::read_to_string(format!("/sys/block/{name}/device/{attr}"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// List candidate storage devices, most likely first.
///
/// Removable devices sort ahead of fixed ones because that is nearly always what the
/// operator just plugged in and means.
pub fn enumerate_devices() -> std::io::Result<Vec<StorageDevice>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/sys/block")?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Optical drives are the other crate's business; virtual devices are nobody's.
        if name.starts_with("sr")
            || name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("dm-")
            || name.starts_with("zram")
            || name.starts_with("md")
        {
            continue;
        }
        // Sizes are always in 512-byte units here, whatever the device's own sector size.
        let sectors: u64 = sysfs(&name, "size")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if sectors == 0 {
            continue;
        }
        let path = format!("/dev/{name}");
        out.push(StorageDevice {
            readable: Path::new(&path).exists()
                && std::fs::File::open(&path).is_ok(),
            removable: sysfs(&name, "removable").as_deref() == Some("1"),
            size_bytes: sectors * 512,
            vendor: sysfs_device(&name, "vendor"),
            model: sysfs_device(&name, "model"),
            transport: transport_of(&name),
            path,
            name,
        });
    }
    out.sort_by(|a, b| {
        b.removable
            .cmp(&a.removable)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// Infer the transport by walking the sysfs device link upward looking for a bus.
fn transport_of(name: &str) -> Option<String> {
    let link = std::fs::read_link(format!("/sys/block/{name}")).ok()?;
    let text = link.to_string_lossy();
    for bus in ["usb", "nvme", "mmc", "firewire", "ata", "scsi"] {
        if text.contains(&format!("/{bus}")) || text.contains(&format!("{bus}:")) {
            return Some(bus.to_string());
        }
    }
    None
}
