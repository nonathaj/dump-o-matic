//! Recognising what a storage device holds.
//!
//! An Xbox 360 `Content` tree is one layout among many a plugged-in device might carry, so
//! detection is a registry of independent detectors rather than a single code path. Each is
//! asked, in turn, whether it recognises a source; the first that does produces a
//! [`Catalogue`] of the content it found. A device nothing recognises is reported as
//! unrecognised, listing what was actually at the top level so the operator can see why —
//! it is never coerced into the nearest familiar shape, because a misread layout would
//! produce confidently wrong extractions.
//!
//! Adding support for another console's storage means adding a detector here and a reader
//! beside [`crate::god`]; nothing above this module needs to change.

use crate::god::GodImage;
use crate::source::ContentSource;
use crate::xcontent::{self, Signature, XContent};
use crate::{DeviceError, Result};

/// A recognised on-device layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// An Xbox 360 `Content/<profile>/<title id>/<type>/` tree, as written to a USB drive
    /// or hard disk by the console itself.
    Xbox360Content,
}

impl std::fmt::Display for Layout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Layout::Xbox360Content => "Xbox 360 content storage",
        })
    }
}

/// What a catalogued item is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    /// A purchased Games-on-Demand title.
    GamesOnDemand,
    /// A game installed to storage from its disc. Same container as the above.
    InstalledGame,
    /// Recognised content that is not a game image.
    Other(String),
}

impl std::fmt::Display for ContentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContentKind::GamesOnDemand => f.write_str("Games on Demand"),
            ContentKind::InstalledGame => f.write_str("installed game"),
            ContentKind::Other(s) => f.write_str(s),
        }
    }
}

/// One piece of content found on a device.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CatalogueItem {
    /// Title ID, which is how the operator selects this item.
    pub title_id: String,
    pub name: String,
    pub kind: ContentKind,
    /// Path of the package header within the source.
    pub path: String,
    pub media_id: String,
    pub version: u32,
    pub disc_number: u8,
    pub disc_in_set: u8,
    pub signature: Signature,
    /// Bytes the package occupies on the device, header and data files together.
    pub package_bytes: u64,
    /// Bytes the reconstructed image would occupy, when this is a game image.
    pub image_bytes: u64,
    pub data_files: usize,
    /// Whether this item can be extracted as a game image.
    pub extractable: bool,
    /// Why it cannot be, when it cannot.
    pub note: Option<String>,
}

impl CatalogueItem {
    /// Size of the `.iso` a conversion would produce.
    pub fn iso_bytes(&self) -> u64 {
        self.image_bytes + crate::iso::RESERVED
    }
}

/// Everything found on one device.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Catalogue {
    pub layout: Layout,
    /// How the source was addressed, e.g. `/dev/sdd`.
    pub source: String,
    /// Provenance about the medium itself, such as the FAT32 OEM name.
    pub medium: Option<String>,
    pub items: Vec<CatalogueItem>,
}

impl Catalogue {
    /// Items that are game images, which is what extraction operates on.
    pub fn games(&self) -> impl Iterator<Item = &CatalogueItem> {
        self.items.iter().filter(|i| i.extractable)
    }

    /// Find one item by title ID, or by a unique prefix of its name.
    ///
    /// Ambiguity is an error rather than a first match: picking for the operator here would
    /// mean extracting several gigabytes of the wrong game.
    pub fn find(&self, needle: &str) -> std::result::Result<&CatalogueItem, String> {
        let n = needle.trim();
        if n.is_empty() {
            return Err("no title given".to_string());
        }
        let matches: Vec<&CatalogueItem> = self
            .items
            .iter()
            .filter(|i| {
                i.title_id.eq_ignore_ascii_case(n)
                    || i.name.to_lowercase().contains(&n.to_lowercase())
            })
            .collect();
        match matches.len() {
            0 => Err(format!("nothing on this device matches {needle:?}")),
            1 => Ok(matches[0]),
            _ => Err(format!(
                "{needle:?} matches {} items: {}. Use a title ID to be unambiguous",
                matches.len(),
                matches
                    .iter()
                    .map(|i| format!("{} ({})", i.name, i.title_id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// Every detector, in the order they are tried.
const DETECTORS: &[(&str, fn(&dyn ContentSource) -> Result<Option<Catalogue>>)] =
    &[("Xbox 360 content storage", detect_xbox360)];

/// Identify what a source holds.
pub fn detect(source: &dyn ContentSource) -> Result<Catalogue> {
    for (_, detector) in DETECTORS {
        if let Some(catalogue) = detector(source)? {
            return Ok(catalogue);
        }
    }
    // Say what was actually there: on an unrecognised device that is the one piece of
    // information that makes the next step obvious.
    let hint = source.list("").ok().map(|entries| {
        let mut names: Vec<String> = entries
            .iter()
            .take(8)
            .map(|e| {
                if e.is_dir {
                    format!("{}/", e.name)
                } else {
                    e.name.clone()
                }
            })
            .collect();
        if entries.len() > 8 {
            names.push(format!("and {} more", entries.len() - 8));
        }
        format!("top level holds: {}", names.join(", "))
    });
    Err(DeviceError::UnknownLayout {
        path: source.describe(),
        hint,
    })
}

/// Detect an Xbox 360 `Content` tree.
fn detect_xbox360(source: &dyn ContentSource) -> Result<Option<Catalogue>> {
    let Ok(profiles) = source.list("Content") else {
        return Ok(None);
    };

    let mut items = Vec::new();
    // Content/<profile id>/<title id>/<content type>/<package>
    for profile in profiles.iter().filter(|e| e.is_dir) {
        let profile_path = format!("Content/{}", profile.name);
        let Ok(titles) = source.list(&profile_path) else {
            continue;
        };
        for title in titles.iter().filter(|e| e.is_dir) {
            let title_path = format!("{profile_path}/{}", title.name);
            let Ok(types) = source.list(&title_path) else {
                continue;
            };
            for content_type in types.iter().filter(|e| e.is_dir) {
                let type_path = format!("{title_path}/{}", content_type.name);
                let Ok(entries) = source.list(&type_path) else {
                    continue;
                };
                let data_dirs: Vec<&str> = entries
                    .iter()
                    .filter(|e| e.is_dir)
                    .map(|e| e.name.as_str())
                    .collect();

                for entry in entries.iter().filter(|e| !e.is_dir) {
                    if entry.size < xcontent::HEADER_SIZE {
                        continue;
                    }
                    let path = format!("{type_path}/{}", entry.name);
                    let Ok(bytes) = source.read_prefix(&path, xcontent::HEADER_SIZE) else {
                        continue;
                    };
                    let Ok(header) = XContent::parse(&bytes, &path) else {
                        continue;
                    };
                    let has_data = data_dirs
                        .iter()
                        .any(|d| d.eq_ignore_ascii_case(&format!("{}.data", entry.name)));
                    items.push(describe(source, &path, &header, entry.size, has_data));
                }
            }
        }
    }

    if items.is_empty() {
        // A `Content` directory with nothing in it is still an Xbox 360 layout; reporting it
        // as an empty catalogue is more useful than claiming not to recognise the device.
        return Ok(Some(Catalogue {
            layout: Layout::Xbox360Content,
            source: source.describe(),
            medium: source.medium_hint(),
            items,
        }));
    }

    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(Some(Catalogue {
        layout: Layout::Xbox360Content,
        source: source.describe(),
        medium: source.medium_hint(),
        items,
    }))
}

/// Build a catalogue entry, working out whether it can actually be extracted.
fn describe(
    source: &dyn ContentSource,
    path: &str,
    header: &XContent,
    header_bytes: u64,
    has_data: bool,
) -> CatalogueItem {
    let is_image = header.is_games_on_demand() || header.is_installed_game();
    let kind = if header.is_games_on_demand() {
        ContentKind::GamesOnDemand
    } else if header.is_installed_game() {
        ContentKind::InstalledGame
    } else {
        ContentKind::Other(header.content_type_name().to_string())
    };

    let mut item = CatalogueItem {
        title_id: header.title_id.clone(),
        name: header.best_name().to_string(),
        kind,
        path: path.to_string(),
        media_id: header.media_id.clone(),
        version: header.version,
        disc_number: header.disc_number,
        disc_in_set: header.disc_in_set,
        signature: header.signature,
        package_bytes: header_bytes,
        image_bytes: 0,
        data_files: 0,
        extractable: false,
        note: None,
    };

    if !is_image {
        item.note = Some(format!("{} is not a game image", header.content_type_name()));
        return item;
    }
    if !has_data {
        item.note = Some("package has no .data directory".to_string());
        return item;
    }

    // Opening the image validates the data files line up, so an item that claims to be
    // extractable has already had its geometry checked.
    match GodImage::open(source, path, header) {
        Ok(img) => {
            item.image_bytes = img.size();
            item.data_files = img.data_files();
            item.package_bytes = header_bytes + package_data_bytes(source, path);
            item.extractable = true;
        }
        Err(e) => item.note = Some(e.to_string()),
    }
    item
}

/// Total size of a package's data files as they sit on the device.
fn package_data_bytes(source: &dyn ContentSource, header_path: &str) -> u64 {
    source
        .list(&format!("{header_path}.data"))
        .map(|entries| entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, title_id: &str) -> CatalogueItem {
        CatalogueItem {
            title_id: title_id.to_string(),
            name: name.to_string(),
            kind: ContentKind::GamesOnDemand,
            path: String::new(),
            media_id: String::new(),
            version: 0,
            disc_number: 0,
            disc_in_set: 0,
            signature: Signature::Console,
            package_bytes: 0,
            image_bytes: 1000,
            data_files: 1,
            extractable: true,
            note: None,
        }
    }

    fn catalogue() -> Catalogue {
        Catalogue {
            layout: Layout::Xbox360Content,
            source: "/dev/sdd".to_string(),
            medium: None,
            items: vec![
                item("Prey", "545407E0"),
                item("Rainbow Six Vegas", "555307D6"),
                item("Rainbow Six Vegas 2", "555307DA"),
            ],
        }
    }

    #[test]
    fn finds_an_item_by_title_id_whatever_the_case() {
        let c = catalogue();
        assert_eq!(c.find("545407E0").unwrap().name, "Prey");
        assert_eq!(c.find("545407e0").unwrap().name, "Prey");
    }

    #[test]
    fn finds_an_item_by_name_fragment() {
        assert_eq!(catalogue().find("prey").unwrap().title_id, "545407E0");
    }

    /// An ambiguous name must not silently pick one: the cost of guessing here is several
    /// gigabytes of the wrong game.
    #[test]
    fn an_ambiguous_name_is_an_error_naming_the_candidates() {
        let err = catalogue().find("Rainbow").unwrap_err();
        assert!(err.contains("matches 2 items"), "{err}");
        assert!(err.contains("555307D6"), "{err}");
        assert!(err.contains("title ID"), "{err}");
    }

    /// A title ID stays unambiguous even when one name is a prefix of another.
    #[test]
    fn a_title_id_disambiguates_similar_names() {
        let c = catalogue();
        assert_eq!(c.find("555307DA").unwrap().name, "Rainbow Six Vegas 2");
    }

    #[test]
    fn an_unknown_needle_is_an_error() {
        assert!(catalogue().find("Halo").is_err());
        assert!(catalogue().find("").is_err());
    }

    #[test]
    fn iso_size_includes_the_reserved_region() {
        assert_eq!(item("x", "y").iso_bytes(), 1000 + crate::iso::RESERVED);
    }
}
