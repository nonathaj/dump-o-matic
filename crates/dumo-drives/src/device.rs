//! Drive enumeration and opening.

use crate::{ioctl, mmc, DriveError, Result};
use dumo_core::{Drive, DriveCapabilities, DriveStatus};
use std::fs::File;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

/// Open a drive read-only without blocking on media.
///
/// `O_NONBLOCK` is essential: opening an optical device without it blocks until a disc is
/// present and spun up, which would hang enumeration on an empty drive.
pub(crate) fn open_device(path: &str) -> Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => DriveError::NotFound {
                path: path.to_string(),
            },
            std::io::ErrorKind::PermissionDenied => DriveError::PermissionDenied {
                path: path.to_string(),
            },
            _ => DriveError::Io {
                path: path.to_string(),
                source: e,
            },
        })
}

fn sysfs_string(name: &str, attr: &str) -> Option<String> {
    let p = format!("/sys/block/{name}/device/{attr}");
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read a procfs sysctl file whole.
///
/// `std::fs::read_to_string` is not usable here: `/proc/sys/dev/cdrom/info` reports a
/// size of zero and signals EOF after the first partial read, so the standard helper
/// silently returns only the first ~32 bytes. Issuing one large read gets the whole
/// table.
fn read_proc_sysctl(path: &str) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 16 * 1024];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    String::from_utf8(buf).ok()
}

/// Read drive capabilities from the kernel's cdrom capability table.
fn capabilities(name: &str) -> DriveCapabilities {
    // /proc/sys/dev/cdrom/info is a table covering all drives; parse the column for ours.
    let Some(info) = read_proc_sysctl("/proc/sys/dev/cdrom/info") else {
        return DriveCapabilities::default();
    };

    let mut column = None;
    let mut caps = DriveCapabilities::default();

    for line in info.lines() {
        let Some((label, values)) = line.split_once(':') else {
            continue;
        };
        let fields: Vec<&str> = values.split_whitespace().collect();

        if label.trim() == "drive name" {
            column = fields.iter().position(|f| *f == name);
            continue;
        }
        let Some(col) = column else { continue };
        let Some(v) = fields.get(col) else { continue };
        let on = *v == "1";

        match label.trim() {
            "Can read DVD" => caps.reads_dvd = on,
            "Can write CD-R" => caps.writes_cd = on,
            "Can write DVD-R" => caps.writes_dvd = on,
            _ => {}
        }
    }
    // Any optical drive reads CDs; the info table has no explicit flag for it.
    caps.reads_cd = true;
    caps
}

/// Build a [`Drive`] for a kernel device name such as `sr0`.
pub fn read_drive(name: &str) -> Result<Drive> {
    let path = format!("/dev/{name}");
    let mut caps = capabilities(name);

    // udev's ID_CDROM_BD is the reliable Blu-ray signal; the /proc table has no BD row.
    if let Ok(out) = std::process::Command::new("udevadm")
        .args(["info", "--query=property", "--name", &path])
        .output()
    {
        let props = String::from_utf8_lossy(&out.stdout);
        caps.reads_bluray = props.contains("ID_CDROM_BD=1");
        caps.writes_bluray =
            props.contains("ID_CDROM_BD_R=1") || props.contains("ID_CDROM_BD_RE=1");
    }

    // Prefer INQUIRY (authoritative, from the device); fall back to sysfs strings.
    let (vendor, model, revision) = match open_device(&path) {
        Ok(f) => match mmc::inquiry(f.as_raw_fd(), &path) {
            Ok((v, m, r)) => (Some(v), Some(m), Some(r)),
            Err(_) => (
                sysfs_string(name, "vendor"),
                sysfs_string(name, "model"),
                sysfs_string(name, "rev"),
            ),
        },
        Err(_) => (
            sysfs_string(name, "vendor"),
            sysfs_string(name, "model"),
            sysfs_string(name, "rev"),
        ),
    };

    Ok(Drive {
        path,
        name: name.to_string(),
        vendor,
        model,
        revision,
        capabilities: caps,
    })
}

/// Find every optical drive on the system.
///
/// Returns them sorted by device name so output is stable between runs.
pub fn enumerate_drives() -> Result<Vec<Drive>> {
    let mut names: Vec<String> = Vec::new();

    if let Ok(entries) = std::fs::read_dir("/sys/block") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            // Optical devices are sr* under the sr_mod driver.
            if name.starts_with("sr") && name[2..].chars().all(|c| c.is_ascii_digit()) {
                names.push(name);
            }
        }
    }
    names.sort();

    let mut drives = Vec::new();
    for n in &names {
        // A drive we cannot describe is still worth listing, so failures are not fatal.
        if let Ok(d) = read_drive(n) {
            drives.push(d);
        }
    }
    Ok(drives)
}

/// Current tray/media state for a drive.
pub fn open_drive_status(drive: &Drive) -> Result<DriveStatus> {
    let f = open_device(&drive.path)?;
    let tray = ioctl::drive_status(f.as_raw_fd(), &drive.path)?;
    Ok(DriveStatus {
        drive: drive.clone(),
        tray,
    })
}
