//! SCSI/MMC commands issued via the SG_IO ioctl.
//!
//! Only informational commands are implemented here, deliberately. GET CONFIGURATION
//! tells us what the medium actually is (as opposed to what its filesystem looks like),
//! and READ CAPACITY gives the medium's true size.

use crate::{DriveError, Result};
use dumo_core::DiscProfile;
use std::os::unix::io::RawFd;

const SG_IO: libc::c_ulong = 0x2285;
const SG_DXFER_FROM_DEV: libc::c_int = -3;
const SG_INTERFACE_ID_ORIG: libc::c_int = b'S' as libc::c_int;

#[repr(C)]
struct SgIoHdr {
    interface_id: libc::c_int,
    dxfer_direction: libc::c_int,
    cmd_len: libc::c_uchar,
    mx_sb_len: libc::c_uchar,
    iovec_count: libc::c_ushort,
    dxfer_len: libc::c_uint,
    dxferp: *mut libc::c_void,
    cmdp: *const libc::c_uchar,
    sbp: *mut libc::c_uchar,
    timeout: libc::c_uint,
    flags: libc::c_uint,
    pack_id: libc::c_int,
    usr_ptr: *mut libc::c_void,
    status: libc::c_uchar,
    masked_status: libc::c_uchar,
    msg_status: libc::c_uchar,
    sb_len_wr: libc::c_uchar,
    host_status: libc::c_ushort,
    driver_status: libc::c_ushort,
    resid: libc::c_int,
    duration: libc::c_uint,
    info: libc::c_uint,
}

/// Issue a read-only SCSI command and fill `buf`.
fn scsi_read(fd: RawFd, path: &str, op: &'static str, cdb: &[u8], buf: &mut [u8]) -> Result<()> {
    let mut sense = [0u8; 32];
    let mut hdr = SgIoHdr {
        interface_id: SG_INTERFACE_ID_ORIG,
        dxfer_direction: SG_DXFER_FROM_DEV,
        cmd_len: cdb.len() as libc::c_uchar,
        mx_sb_len: sense.len() as libc::c_uchar,
        iovec_count: 0,
        dxfer_len: buf.len() as libc::c_uint,
        dxferp: buf.as_mut_ptr().cast(),
        cmdp: cdb.as_ptr(),
        sbp: sense.as_mut_ptr(),
        // Informational commands; a drive that cannot answer in 15s is wedged.
        timeout: 15_000,
        flags: 0,
        pack_id: 0,
        usr_ptr: std::ptr::null_mut(),
        status: 0,
        masked_status: 0,
        msg_status: 0,
        sb_len_wr: 0,
        host_status: 0,
        driver_status: 0,
        resid: 0,
        duration: 0,
        info: 0,
    };

    let rc = unsafe { libc::ioctl(fd, SG_IO, &mut hdr as *mut SgIoHdr) };
    if rc < 0 {
        return Err(DriveError::Command {
            path: path.to_string(),
            operation: op,
            detail: std::io::Error::last_os_error().to_string(),
        });
    }
    if hdr.status != 0 || hdr.host_status != 0 {
        return Err(DriveError::Command {
            path: path.to_string(),
            operation: op,
            detail: format!(
                "scsi status 0x{:02X}, host 0x{:04X}, sense key 0x{:02X}",
                hdr.status,
                hdr.host_status,
                sense.get(2).copied().unwrap_or(0) & 0x0F
            ),
        });
    }
    Ok(())
}

/// GET CONFIGURATION (0x46), returning the drive's current profile.
///
/// This is the authoritative answer to "what kind of disc is this?" — it comes from the
/// drive, not from guessing at filesystem contents.
pub fn current_profile(fd: RawFd, path: &str) -> Result<DiscProfile> {
    let mut buf = [0u8; 32];
    let cdb: [u8; 10] = [
        0x46, // GET CONFIGURATION
        0x01, // RT = 1: report only the current feature set
        0, 0, // starting feature number
        0, 0, 0, // reserved
        0,
        buf.len() as u8, // allocation length
        0,               // control
    ];
    scsi_read(fd, path, "GET CONFIGURATION", &cdb, &mut buf)?;
    // Feature header: bytes 6-7 hold the current profile, big-endian.
    let code = u16::from_be_bytes([buf[6], buf[7]]);
    Ok(DiscProfile::from_mmc(code))
}

/// READ CAPACITY (0x25): last LBA and block size, giving true medium capacity.
pub fn read_capacity(fd: RawFd, path: &str) -> Result<u64> {
    let mut buf = [0u8; 8];
    let cdb: [u8; 10] = [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    scsi_read(fd, path, "READ CAPACITY", &cdb, &mut buf)?;
    let last_lba = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let block_size = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    // last_lba is the address of the final block, so the count is one greater.
    Ok(u64::from(last_lba).saturating_add(1) * u64::from(block_size))
}

/// INQUIRY (0x12): vendor, model, and firmware revision straight from the device.
pub fn inquiry(fd: RawFd, path: &str) -> Result<(String, String, String)> {
    let mut buf = [0u8; 96];
    let cdb: [u8; 6] = [0x12, 0, 0, 0, buf.len() as u8, 0];
    scsi_read(fd, path, "INQUIRY", &cdb, &mut buf)?;
    let field = |r: std::ops::Range<usize>| {
        String::from_utf8_lossy(&buf[r]).trim().to_string()
    };
    Ok((field(8..16), field(16..32), field(32..36)))
}
