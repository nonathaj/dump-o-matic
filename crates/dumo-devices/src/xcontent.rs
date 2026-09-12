//! XContent package header: what a console content file says about itself.
//!
//! Every piece of content an Xbox 360 writes — a save, a downloaded title, an installed
//! game — is fronted by a header of this shape, signed either by a console (`CON `) or by
//! Microsoft (`LIVE`/`PIRS`). It carries the title name, the title and media IDs, and, for
//! Games on Demand, the volume descriptor describing the image that follows.
//!
//! All multi-byte fields are big-endian, and strings are UTF-16BE, because this is a
//! PowerPC console's format.
//!
//! The two volume-descriptor fields this tool depends on were determined empirically and
//! then verified, because published descriptions of them disagree: the block **count** is
//! a 24-bit big-endian value at +0x19, while the block **offset** is 24-bit *little*-endian
//! at +0x1C. That mixed endianness looks like a mistake but is not: read either field the
//! other way round and it becomes an absurd value (billions of blocks). Both readings were
//! confirmed against eight real packages, where the count matched the data files' geometry
//! and the offset matched the filesystem's own addressing to the block.

use crate::{DeviceError, Result};

/// Size of a package header. Fixed by the format.
pub const HEADER_SIZE: u64 = 0xB000;

/// Offset of the volume descriptor within the header.
const VOLUME_DESCRIPTOR: usize = 0x379;
/// Volume descriptor size that marks a Games-on-Demand (SVOD) descriptor.
const GOD_DESCRIPTOR_SIZE: u8 = 0x24;

/// How a package is signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signature {
    /// Signed by a specific console. Its licence is bound to that console.
    Console,
    /// Signed by Microsoft, from Xbox Live.
    Live,
    /// Signed by Microsoft, from offline media.
    Pirs,
}

impl Signature {
    fn from_magic(magic: &[u8]) -> Option<Self> {
        match magic {
            b"CON " => Some(Signature::Console),
            b"LIVE" => Some(Signature::Live),
            b"PIRS" => Some(Signature::Pirs),
            _ => None,
        }
    }
}

impl std::fmt::Display for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Signature::Console => "console-signed",
            Signature::Live => "Xbox Live",
            Signature::Pirs => "PIRS",
        })
    }
}

/// A parsed package header.
#[derive(Debug, Clone)]
pub struct XContent {
    pub signature: Signature,
    /// Raw content type, e.g. `0x00004000` for Games on Demand.
    pub content_type: u32,
    /// Title ID, the console's identifier for the game, e.g. `545407E0`.
    pub title_id: String,
    /// Media ID, which also appears inside the executable and so can be cross-checked.
    pub media_id: String,
    /// Name shown on the dashboard.
    pub display_name: String,
    /// Name of the title the content belongs to.
    pub title_name: String,
    pub disc_number: u8,
    pub disc_in_set: u8,
    pub version: u32,
    /// Volume descriptor size byte; `0x24` indicates a Games-on-Demand image.
    pub descriptor_size: u8,
    /// Root of the package's hash tree, recorded as provenance.
    pub root_hash: String,
    data_block_count: u64,
    data_block_offset: u64,
}

impl XContent {
    /// Parse a header. `bytes` must be the whole header.
    pub fn parse(bytes: &[u8], label: &str) -> Result<Self> {
        if bytes.len() < 0x1711 {
            return Err(DeviceError::Corrupt {
                path: label.to_string(),
                detail: format!("header is only {} bytes", bytes.len()),
            });
        }
        let signature = Signature::from_magic(&bytes[0..4]).ok_or_else(|| DeviceError::Corrupt {
            path: label.to_string(),
            detail: format!(
                "unrecognised package magic {:?}",
                String::from_utf8_lossy(&bytes[0..4])
            ),
        })?;

        let vd = &bytes[VOLUME_DESCRIPTOR..VOLUME_DESCRIPTOR + 0x24];

        Ok(Self {
            signature,
            content_type: u32be(bytes, 0x344),
            title_id: format!("{:08X}", u32be(bytes, 0x360)),
            media_id: format!("{:08X}", u32be(bytes, 0x354)),
            display_name: utf16be(bytes, 0x411, 0x100),
            title_name: utf16be(bytes, 0x1691, 0x80),
            disc_number: bytes[0x366],
            disc_in_set: bytes[0x367],
            version: u32be(bytes, 0x358),
            descriptor_size: vd[0],
            root_hash: vd[0x04..0x18].iter().map(|b| format!("{b:02x}")).collect(),
            // See the module note on the endianness of these two.
            data_block_count: u24be(vd, 0x19),
            data_block_offset: u24le(vd, 0x1c),
        })
    }

    /// Human-readable content type.
    pub fn content_type_name(&self) -> &'static str {
        match self.content_type {
            0x0000_1000 => "saved game",
            0x0000_2000 => "marketplace content",
            0x0000_4000 => "Games on Demand",
            0x0000_5000 => "installer",
            0x0000_7000 => "installed game",
            0x0000_9000 => "avatar item",
            0x0001_0000 => "profile",
            0x0002_0000 => "gamer picture",
            0x0003_0000 => "theme",
            0x0004_0000 => "cache file",
            0x0008_0000 => "game demo",
            0x000D_0000 => "game video",
            _ => "unknown",
        }
    }

    /// Whether this package is a Games-on-Demand game image.
    pub fn is_games_on_demand(&self) -> bool {
        self.content_type == 0x0000_4000 && self.descriptor_size == GOD_DESCRIPTOR_SIZE
    }

    /// Whether this package is an installed-from-disc game image, which shares the
    /// Games-on-Demand layout.
    pub fn is_installed_game(&self) -> bool {
        self.content_type == 0x0000_7000 && self.descriptor_size == GOD_DESCRIPTOR_SIZE
    }

    /// Data blocks the header declares. Zero when the header does not say.
    pub fn data_block_count(&self) -> u64 {
        self.data_block_count
    }

    /// Block offset the filesystem's sector numbers are relative to, as declared.
    ///
    /// Treated as a starting point rather than gospel: [`crate::xdvdfs`] validates it by
    /// parsing the directory it points at, because a wrong base silently yields a
    /// plausible but corrupt image.
    pub fn data_block_offset_hint(&self) -> u64 {
        self.data_block_offset
    }

    /// The name to show for this content, preferring the more specific field.
    pub fn best_name(&self) -> &str {
        if !self.display_name.trim().is_empty() {
            &self.display_name
        } else {
            &self.title_name
        }
    }
}

fn u32be(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn u24be(b: &[u8], o: usize) -> u64 {
    ((b[o] as u64) << 16) | ((b[o + 1] as u64) << 8) | b[o + 2] as u64
}

fn u24le(b: &[u8], o: usize) -> u64 {
    ((b[o + 2] as u64) << 16) | ((b[o + 1] as u64) << 8) | b[o] as u64
}

/// Read a fixed-width UTF-16BE string, stopping at the first NUL.
fn utf16be(b: &[u8], offset: usize, len: usize) -> String {
    let end = (offset + len).min(b.len());
    let units: Vec<u16> = b[offset..end]
        .chunks_exact(2)
        .map(|p| u16::from_be_bytes([p[0], p[1]]))
        .take_while(|u| *u != 0)
        .collect();
    String::from_utf16_lossy(&units).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a header carrying the values measured from a real package (Prey).
    fn header() -> Vec<u8> {
        let mut h = vec![0u8; HEADER_SIZE as usize];
        h[0..4].copy_from_slice(b"CON ");
        h[0x344..0x348].copy_from_slice(&0x0000_4000u32.to_be_bytes());
        h[0x354..0x358].copy_from_slice(&0x1ABA_0DD5u32.to_be_bytes());
        h[0x358..0x35c].copy_from_slice(&3u32.to_be_bytes());
        h[0x360..0x364].copy_from_slice(&0x5454_07E0u32.to_be_bytes());
        h[0x366] = 0;
        h[0x367] = 0;
        for (i, u) in "Prey".encode_utf16().enumerate() {
            let o = 0x411 + i * 2;
            h[o..o + 2].copy_from_slice(&u.to_be_bytes());
        }
        let vd = VOLUME_DESCRIPTOR;
        h[vd] = 0x24;
        h[vd + 0x18] = 0x40;
        // Count 0x10FBCC big-endian; offset 0x0307F0 little-endian.
        h[vd + 0x19..vd + 0x1c].copy_from_slice(&[0x10, 0xfb, 0xcc]);
        h[vd + 0x1c..vd + 0x1f].copy_from_slice(&[0xf0, 0x07, 0x03]);
        h
    }

    #[test]
    fn parses_a_measured_games_on_demand_header() {
        let x = XContent::parse(&header(), "prey").unwrap();
        assert_eq!(x.signature, Signature::Console);
        assert_eq!(x.title_id, "545407E0");
        assert_eq!(x.media_id, "1ABA0DD5");
        assert_eq!(x.display_name, "Prey");
        assert_eq!(x.content_type_name(), "Games on Demand");
        assert!(x.is_games_on_demand());
        assert_eq!(x.version, 3);
    }

    /// The endianness that published references get wrong. These are the real values for
    /// Prey: 1,113,036 blocks of image, addressed from block 198,640.
    #[test]
    fn volume_descriptor_fields_use_their_measured_endianness() {
        let x = XContent::parse(&header(), "prey").unwrap();
        assert_eq!(x.data_block_count(), 1_113_036);
        assert_eq!(x.data_block_offset_hint(), 198_640);
    }

    #[test]
    fn an_unsigned_blob_is_refused() {
        let mut h = header();
        h[0..4].copy_from_slice(b"ZZZZ");
        assert!(XContent::parse(&h, "x").is_err());
    }

    #[test]
    fn a_truncated_header_is_refused() {
        assert!(XContent::parse(&[0u8; 64], "x").is_err());
    }

    /// A cache file is not a game, and must not be offered as one.
    #[test]
    fn non_game_content_is_not_games_on_demand() {
        let mut h = header();
        h[0x344..0x348].copy_from_slice(&0x0004_0000u32.to_be_bytes());
        let x = XContent::parse(&h, "cache").unwrap();
        assert!(!x.is_games_on_demand());
        assert_eq!(x.content_type_name(), "cache file");
    }
}
