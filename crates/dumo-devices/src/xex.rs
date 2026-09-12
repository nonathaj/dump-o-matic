//! XEX2 executable header, read only for the identity it carries.
//!
//! `default.xex` embeds the title ID, media ID and version of the game it belongs to. Those
//! same values appear in the package header outside the image, so reading both and
//! comparing them is an independent cross-check: it confirms the container was parsed
//! correctly *and* that the package describes the game it actually holds. On the eight
//! packages measured, all eight agreed.

/// Optional-header key identifying the execution-info block.
const EXECUTION_INFO: u32 = 0x0004_0006;
/// Cap on optional headers, to bound a malformed count.
const MAX_OPTIONAL_HEADERS: u32 = 256;

/// Identity read out of an executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionInfo {
    pub title_id: String,
    pub media_id: String,
    pub version: u32,
    pub base_version: u32,
    pub disc_number: u8,
    pub disc_count: u8,
}

/// Parse the execution info out of an XEX2 image.
///
/// `bytes` need only be the start of the file, but must reach the execution-info block —
/// which on some titles sits well past the first few kilobytes, so pass a generous prefix.
/// Returns `None` when this is not an XEX2 or the block is not present in what was read.
pub fn execution_info(bytes: &[u8]) -> Option<ExecutionInfo> {
    if bytes.len() < 0x18 || &bytes[0..4] != b"XEX2" {
        return None;
    }
    let count = u32be(bytes, 0x14)?;
    for i in 0..count.min(MAX_OPTIONAL_HEADERS) {
        let entry = 0x18 + i as usize * 8;
        if entry + 8 > bytes.len() {
            return None;
        }
        let key = u32be(bytes, entry)?;
        let value = u32be(bytes, entry + 4)? as usize;
        if key != EXECUTION_INFO {
            continue;
        }
        if value + 0x14 > bytes.len() {
            return None;
        }
        return Some(ExecutionInfo {
            media_id: format!("{:08X}", u32be(bytes, value)?),
            version: u32be(bytes, value + 4)?,
            base_version: u32be(bytes, value + 8)?,
            title_id: format!("{:08X}", u32be(bytes, value + 12)?),
            disc_number: bytes[value + 0x12],
            disc_count: bytes[value + 0x13],
        });
    }
    None
}

fn u32be(b: &[u8], o: usize) -> Option<u32> {
    let s = b.get(o..o + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xex(exec_offset: usize, total: usize) -> Vec<u8> {
        let mut b = vec![0u8; total];
        b[0..4].copy_from_slice(b"XEX2");
        b[0x14..0x18].copy_from_slice(&1u32.to_be_bytes());
        b[0x18..0x1c].copy_from_slice(&EXECUTION_INFO.to_be_bytes());
        b[0x1c..0x20].copy_from_slice(&(exec_offset as u32).to_be_bytes());
        b[exec_offset..exec_offset + 4].copy_from_slice(&0x1ABA_0DD5u32.to_be_bytes());
        b[exec_offset + 4..exec_offset + 8].copy_from_slice(&3u32.to_be_bytes());
        b[exec_offset + 12..exec_offset + 16].copy_from_slice(&0x5454_07E0u32.to_be_bytes());
        b[exec_offset + 0x12] = 1;
        b[exec_offset + 0x13] = 1;
        b
    }

    #[test]
    fn reads_identity_from_an_executable() {
        let info = execution_info(&xex(0x400, 0x1000)).unwrap();
        assert_eq!(info.title_id, "545407E0");
        assert_eq!(info.media_id, "1ABA0DD5");
        assert_eq!(info.version, 3);
        assert_eq!(info.disc_count, 1);
    }

    /// Measured behaviour: three of eight titles keep this block beyond 8 KB, so a short
    /// read must report "not found" rather than a wrong answer.
    #[test]
    fn a_block_beyond_the_read_is_not_guessed() {
        let full = xex(0x9000, 0x10000);
        assert!(execution_info(&full).is_some());
        assert!(execution_info(&full[..0x2000]).is_none());
    }

    #[test]
    fn other_formats_are_rejected() {
        assert!(execution_info(b"XBEH____").is_none());
        assert!(execution_info(&[]).is_none());
    }
}
