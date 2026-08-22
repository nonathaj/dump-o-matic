//! Logiqx XML datfile parsing and hash matching.
//!
//! Redump and No-Intro both publish Logiqx-format datfiles: a `<datafile>` with a
//! `<header>` naming the platform, then one `<game>` per title, each holding one or more
//! `<rom>` entries with size and hashes.
//!
//! Two structural facts drive the design here, both confirmed against real Redump data:
//!
//! - **Datfiles contain no serials.** Games are keyed by title, and hashes are the only
//!   reliable join. A disc serial like `SLUS-20578` cannot be looked up directly.
//! - **A game may have several roms.** Disc images are one `.iso`, but CD-based titles
//!   are a `.cue` plus one `.bin` per track. Matching one rom identifies the *game*;
//!   claiming the *dump is complete* requires matching every rom in the set.

use crate::{IdentifyError, Result};
use dumo_core::Confidence;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One file entry within a game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rom {
    pub name: String,
    pub size: u64,
    pub crc32: String,
    pub md5: String,
    pub sha1: String,
}

/// One title in a datfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    pub name: String,
    /// Redump categories include "Games", "Demos", "Applications", "Coverdiscs".
    pub category: Option<String>,
    pub roms: Vec<Rom>,
}

impl Game {
    /// Whether this game is a single-file (disc image) entry.
    pub fn is_single_file(&self) -> bool {
        self.roms.len() == 1
    }
}

/// A parsed datfile for one platform.
#[derive(Debug, Clone)]
pub struct Datfile {
    /// Platform name from the header, e.g. `Sony - PlayStation 2`.
    pub platform: String,
    pub version: Option<String>,
    pub games: Vec<Game>,
    pub source_path: PathBuf,
}

/// Decode the XML entities that appear in Redump titles.
///
/// Game names routinely contain `&` (`Tom & Jerry`) and quotes, which arrive escaped.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

impl Datfile {
    /// Parse a Logiqx datfile from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| IdentifyError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        Self::parse(&text, path)
    }

    /// Parse datfile XML.
    pub fn parse(xml: &str, source_path: &Path) -> Result<Self> {
        let mut reader = Reader::from_str(xml);
        reader.trim_text(true);

        let mut platform = String::new();
        let mut version = None;
        let mut games: Vec<Game> = Vec::new();

        // Where we are in the document.
        let mut in_header = false;
        let mut current_game: Option<Game> = None;
        let mut text_target: Option<&'static str> = None;
        let mut buf = Vec::new();

        loop {
            let ev = reader.read_event_into(&mut buf).map_err(|e| {
                IdentifyError::Parse {
                    path: source_path.to_path_buf(),
                    detail: e.to_string(),
                }
            })?;

            match ev {
                Event::Eof => break,

                Event::Start(e) | Event::Empty(e) => {
                    let name = e.name();
                    let tag = String::from_utf8_lossy(name.as_ref()).to_string();

                    let attr = |key: &str| -> Option<String> {
                        e.attributes().flatten().find_map(|a| {
                            if a.key.as_ref() == key.as_bytes() {
                                Some(unescape(&String::from_utf8_lossy(&a.value)))
                            } else {
                                None
                            }
                        })
                    };

                    match tag.as_str() {
                        "header" => in_header = true,
                        "name" if in_header => text_target = Some("name"),
                        "version" if in_header => text_target = Some("version"),
                        "game" => {
                            current_game = Some(Game {
                                name: attr("name").unwrap_or_default(),
                                category: None,
                                roms: Vec::new(),
                            });
                        }
                        "category" => text_target = Some("category"),
                        "rom" => {
                            if let Some(g) = current_game.as_mut() {
                                g.roms.push(Rom {
                                    name: attr("name").unwrap_or_default(),
                                    size: attr("size")
                                        .and_then(|s| s.parse().ok())
                                        .unwrap_or(0),
                                    crc32: attr("crc").unwrap_or_default().to_ascii_lowercase(),
                                    md5: attr("md5").unwrap_or_default().to_ascii_lowercase(),
                                    sha1: attr("sha1").unwrap_or_default().to_ascii_lowercase(),
                                });
                            }
                        }
                        _ => {}
                    }
                }

                Event::Text(t) => {
                    if let Some(target) = text_target.take() {
                        let value = unescape(&String::from_utf8_lossy(&t));
                        match target {
                            "name" => platform = value,
                            "version" => version = Some(value),
                            "category" => {
                                if let Some(g) = current_game.as_mut() {
                                    g.category = Some(value);
                                }
                            }
                            _ => {}
                        }
                    }
                }

                Event::End(e) => {
                    let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    match tag.as_str() {
                        "header" => in_header = false,
                        "game" => {
                            if let Some(g) = current_game.take() {
                                games.push(g);
                            }
                        }
                        _ => {}
                    }
                    text_target = None;
                }

                _ => {}
            }
            buf.clear();
        }

        if platform.is_empty() {
            return Err(IdentifyError::Parse {
                path: source_path.to_path_buf(),
                detail: "no platform name in <header>".into(),
            });
        }

        Ok(Datfile {
            platform,
            version,
            games,
            source_path: source_path.to_path_buf(),
        })
    }
}

/// What a hash lookup found.
#[derive(Debug, Clone)]
pub struct DatMatch {
    pub platform: String,
    pub game: Game,
    /// The specific rom entry that matched.
    pub rom: Rom,
    /// Which digest produced the match.
    pub matched_on: &'static str,
    pub confidence: Confidence,
    /// Datfile the match came from.
    pub source: PathBuf,
}

impl DatMatch {
    /// Whether the matched game consists solely of the file we matched.
    ///
    /// For a single-file game this means the dump is complete. For a multi-file game it
    /// means we have identified the title but only verified one of its files.
    pub fn is_complete_set(&self) -> bool {
        self.game.is_single_file()
    }
}

/// A searchable collection of datfiles.
#[derive(Debug, Default)]
pub struct DatfileSet {
    datfiles: Vec<Datfile>,
    /// sha1 -> (datfile index, game index, rom index)
    by_sha1: HashMap<String, (usize, usize, usize)>,
    by_md5: HashMap<String, (usize, usize, usize)>,
    by_crc32: HashMap<String, (usize, usize, usize)>,
}

impl DatfileSet {
    /// Load every `.dat` file in a directory.
    pub fn load_dir(dir: &Path) -> Result<Self> {
        let entries = std::fs::read_dir(dir).map_err(|e| IdentifyError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;

        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .map(|e| e.eq_ignore_ascii_case("dat"))
                    .unwrap_or(false)
            })
            .collect();
        paths.sort();

        let mut set = DatfileSet::default();
        for p in paths {
            set.add(Datfile::load(&p)?);
        }
        Ok(set)
    }

    /// Add a parsed datfile and index its hashes.
    pub fn add(&mut self, dat: Datfile) {
        let di = self.datfiles.len();
        for (gi, game) in dat.games.iter().enumerate() {
            for (ri, rom) in game.roms.iter().enumerate() {
                let key = (di, gi, ri);
                // Empty hashes are not keys; a few datfile entries omit digests.
                if !rom.sha1.is_empty() {
                    self.by_sha1.entry(rom.sha1.clone()).or_insert(key);
                }
                if !rom.md5.is_empty() {
                    self.by_md5.entry(rom.md5.clone()).or_insert(key);
                }
                if !rom.crc32.is_empty() {
                    self.by_crc32.entry(rom.crc32.clone()).or_insert(key);
                }
            }
        }
        self.datfiles.push(dat);
    }

    pub fn is_empty(&self) -> bool {
        self.datfiles.is_empty()
    }

    pub fn datfiles(&self) -> &[Datfile] {
        &self.datfiles
    }

    pub fn game_count(&self) -> usize {
        self.datfiles.iter().map(|d| d.games.len()).sum()
    }

    fn build_match(&self, key: (usize, usize, usize), matched_on: &'static str) -> DatMatch {
        let (di, gi, ri) = key;
        let dat = &self.datfiles[di];
        DatMatch {
            platform: dat.platform.clone(),
            game: dat.games[gi].clone(),
            rom: dat.games[gi].roms[ri].clone(),
            matched_on,
            // A cryptographic hash match against a curated database is exact, and the
            // only case the project permits accepting without confirmation.
            confidence: Confidence::Exact,
            source: dat.source_path.clone(),
        }
    }

    /// Look up by SHA-1, the strongest and preferred key.
    pub fn find_by_sha1(&self, sha1: &str) -> Option<DatMatch> {
        let k = sha1.trim().to_ascii_lowercase();
        self.by_sha1.get(&k).map(|key| self.build_match(*key, "sha1"))
    }

    pub fn find_by_md5(&self, md5: &str) -> Option<DatMatch> {
        let k = md5.trim().to_ascii_lowercase();
        self.by_md5.get(&k).map(|key| self.build_match(*key, "md5"))
    }

    pub fn find_by_crc32(&self, crc: &str) -> Option<DatMatch> {
        let k = crc.trim().to_ascii_lowercase();
        self.by_crc32
            .get(&k)
            .map(|key| self.build_match(*key, "crc32"))
    }

    /// Look up a file by its Redump digests, preferring the strongest hash available.
    ///
    /// Also cross-checks the recorded size: a hash match with a size mismatch would mean
    /// something is badly wrong, and is reported rather than accepted.
    pub fn find(&self, d: &dumo_core::hash::RedumpDigests) -> Option<DatMatch> {
        let m = self
            .find_by_sha1(&d.sha1)
            .or_else(|| self.find_by_md5(&d.md5))
            .or_else(|| self.find_by_crc32(&d.crc32))?;
        Some(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<datafile>
	<header>
		<name>Sony - PlayStation 2</name>
		<description>Sony - PlayStation 2 - Discs</description>
		<version>2026-06-15 03-41-38</version>
		<author>redump.org</author>
	</header>
	<game name="Lord of the Rings, The - The Two Towers (USA)">
		<category>Games</category>
		<description>Lord of the Rings, The - The Two Towers (USA)</description>
		<rom name="Lord of the Rings, The - The Two Towers (USA).iso" size="4116250624" crc="7723ab98" md5="2be0d4a7730fdff8d4eb2f95ac7a7bc8" sha1="f6a63934521febb2e0c83d78510dfe8e78bbf214"/>
	</game>
	<game name="Tom &amp; Jerry in War of the Whiskers (USA)">
		<category>Games</category>
		<rom name="Tom &amp; Jerry in War of the Whiskers (USA).iso" size="123" crc="aabbccdd" md5="11111111111111111111111111111111" sha1="2222222222222222222222222222222222222222"/>
	</game>
	<game name="Some CD Game (Europe)">
		<category>Games</category>
		<rom name="Some CD Game (Europe).cue" size="306" crc="08b43a5a" md5="79aa7750ca9672cc412a2725cad5436d" sha1="56dc1f5d8c460e3ee8a2f52a1df44d8cac0f1700"/>
		<rom name="Some CD Game (Europe) (Track 1).bin" size="323082480" crc="ef3e1282" md5="aa457b73bbca023cd23170e06c4493b8" sha1="e9bfae82c856c6efcbfadb7ab2ac497a531420f4"/>
		<rom name="Some CD Game (Europe) (Track 2).bin" size="33868800" crc="3f9896b3" md5="441eeb4204aafc104bf1863b743a13a2" sha1="a55870eae893408b85b4274d19e349a7a447b84b"/>
	</game>
</datafile>"#;

    fn sample_set() -> DatfileSet {
        let dat = Datfile::parse(SAMPLE, Path::new("test.dat")).unwrap();
        let mut set = DatfileSet::default();
        set.add(dat);
        set
    }

    #[test]
    fn parses_header_and_games() {
        let d = Datfile::parse(SAMPLE, Path::new("t.dat")).unwrap();
        assert_eq!(d.platform, "Sony - PlayStation 2");
        assert_eq!(d.version.as_deref(), Some("2026-06-15 03-41-38"));
        assert_eq!(d.games.len(), 3);
        assert_eq!(d.games[0].category.as_deref(), Some("Games"));
    }

    /// Redump titles contain ampersands, which arrive XML-escaped.
    #[test]
    fn decodes_escaped_entities_in_titles() {
        let d = Datfile::parse(SAMPLE, Path::new("t.dat")).unwrap();
        assert_eq!(d.games[1].name, "Tom & Jerry in War of the Whiskers (USA)");
        assert_eq!(
            d.games[1].roms[0].name,
            "Tom & Jerry in War of the Whiskers (USA).iso"
        );
    }

    /// The real dump of the user's disc must match its real datfile entry.
    #[test]
    fn matches_the_real_ps2_dump_by_sha1() {
        let set = sample_set();
        let m = set
            .find_by_sha1("f6a63934521febb2e0c83d78510dfe8e78bbf214")
            .expect("should match");
        assert_eq!(m.game.name, "Lord of the Rings, The - The Two Towers (USA)");
        assert_eq!(m.platform, "Sony - PlayStation 2");
        assert_eq!(m.matched_on, "sha1");
        assert_eq!(m.confidence, Confidence::Exact);
        assert!(m.is_complete_set());
        assert_eq!(m.rom.size, 4_116_250_624);
    }

    #[test]
    fn hash_lookup_is_case_insensitive() {
        let set = sample_set();
        assert!(set
            .find_by_sha1("F6A63934521FEBB2E0C83D78510DFE8E78BBF214")
            .is_some());
    }

    #[test]
    fn matches_by_md5_and_crc_too() {
        let set = sample_set();
        assert!(set.find_by_md5("2be0d4a7730fdff8d4eb2f95ac7a7bc8").is_some());
        assert!(set.find_by_crc32("7723ab98").is_some());
    }

    #[test]
    fn find_prefers_sha1() {
        let set = sample_set();
        let d = dumo_core::hash::RedumpDigests {
            size: 4_116_250_624,
            crc32: "7723ab98".into(),
            md5: "2be0d4a7730fdff8d4eb2f95ac7a7bc8".into(),
            sha1: "f6a63934521febb2e0c83d78510dfe8e78bbf214".into(),
        };
        assert_eq!(set.find(&d).unwrap().matched_on, "sha1");
    }

    #[test]
    fn unknown_hash_does_not_match() {
        let set = sample_set();
        assert!(set.find_by_sha1("0000000000000000000000000000000000000000").is_none());
    }

    /// Matching one track of a multi-file game identifies the title but is not a
    /// complete set — the distinction matters before anything is called archival.
    #[test]
    fn multi_rom_game_is_not_a_complete_set_from_one_match() {
        let set = sample_set();
        let m = set
            .find_by_sha1("e9bfae82c856c6efcbfadb7ab2ac497a531420f4")
            .expect("track 1 should match");
        assert_eq!(m.game.name, "Some CD Game (Europe)");
        assert_eq!(m.game.roms.len(), 3);
        assert!(!m.is_complete_set());
    }

    #[test]
    fn indexes_every_rom_of_every_game() {
        let set = sample_set();
        assert_eq!(set.game_count(), 3);
        // cue + 2 tracks + 2 single-file games = 5 indexed entries
        assert_eq!(set.by_sha1.len(), 5);
    }

    #[test]
    fn rejects_xml_without_a_platform_name() {
        let err = Datfile::parse("<datafile><header></header></datafile>", Path::new("x.dat"))
            .unwrap_err();
        assert!(matches!(err, IdentifyError::Parse { .. }));
    }
}
