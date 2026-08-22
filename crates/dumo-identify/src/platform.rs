//! Mapping from Redump/No-Intro platform names to ES-DE / EmuDeck directory slugs.
//!
//! The project's naming convention is Redump-accurate *filenames* inside
//! *emulator-standard directories*, so this table is what joins the two vocabularies.
//! Slugs match the directory names already in use under `~/Emulation/roms/`.

/// Map a datfile platform name to the ES-DE directory slug, if known.
///
/// Matching is case-insensitive and ignores surrounding whitespace. Returns `None` for
/// platforms we have no mapping for, so callers can report the gap rather than invent a
/// directory name.
pub fn es_de_slug(platform: &str) -> Option<&'static str> {
    let p = platform.trim().to_ascii_lowercase();
    let slug = match p.as_str() {
        "sony - playstation" => "psx",
        "sony - playstation 2" => "ps2",
        "sony - playstation 3" => "ps3",
        "sony - playstation portable" => "psp",
        "nintendo - gamecube" => "gc",
        "nintendo - wii" => "wii",
        "nintendo - wii u" => "wiiu",
        "microsoft - xbox" => "xbox",
        "microsoft - xbox 360" => "xbox360",
        "sega - dreamcast" => "dreamcast",
        "sega - saturn" => "saturn",
        "sega - mega cd & sega cd" => "segacd",
        "sega - mega-cd & sega cd" => "segacd",
        "nec - pc engine cd & turbografx cd" => "pcenginecd",
        "panasonic - 3do interactive multiplayer" => "3do",
        "philips - cd-i" => "cdimono1",
        "commodore - amiga cd32" => "amigacd32",
        "commodore - amiga cd" => "amigacd",
        "atari - jaguar cd interactive multimedia system" => "atarijaguarcd",
        "snk - neo geo cd" => "neogeocd",
        "bandai - playdia quick interactive system" => "playdia",
        "fujitsu - fm towns series" => "fmtowns",
        "vtech - v.flash & v.smile pro" => "vsmile",
        _ => return None,
    };
    Some(slug)
}

/// Every platform this table knows about, for diagnostics.
pub fn known_platforms() -> &'static [(&'static str, &'static str)] {
    &[
        ("Sony - PlayStation", "psx"),
        ("Sony - PlayStation 2", "ps2"),
        ("Sony - PlayStation 3", "ps3"),
        ("Sony - PlayStation Portable", "psp"),
        ("Nintendo - GameCube", "gc"),
        ("Nintendo - Wii", "wii"),
        ("Nintendo - Wii U", "wiiu"),
        ("Microsoft - Xbox", "xbox"),
        ("Microsoft - Xbox 360", "xbox360"),
        ("Sega - Dreamcast", "dreamcast"),
        ("Sega - Saturn", "saturn"),
        ("Sega - Mega CD & Sega CD", "segacd"),
        ("NEC - PC Engine CD & TurboGrafx CD", "pcenginecd"),
        ("Panasonic - 3DO Interactive Multiplayer", "3do"),
        ("Philips - CD-i", "cdimono1"),
        ("Commodore - Amiga CD32", "amigacd32"),
        ("Atari - Jaguar CD Interactive Multimedia System", "atarijaguarcd"),
        ("SNK - Neo Geo CD", "neogeocd"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_the_platforms_we_have_datfiles_for() {
        assert_eq!(es_de_slug("Sony - PlayStation 2"), Some("ps2"));
        assert_eq!(es_de_slug("Sony - PlayStation"), Some("psx"));
    }

    #[test]
    fn matching_ignores_case_and_whitespace() {
        assert_eq!(es_de_slug("  sony - playstation 2  "), Some("ps2"));
        assert_eq!(es_de_slug("SONY - PLAYSTATION"), Some("psx"));
    }

    /// An unknown platform must not silently produce a directory name.
    #[test]
    fn unknown_platform_returns_none() {
        assert_eq!(es_de_slug("Acme - Imaginary Console"), None);
        assert_eq!(es_de_slug(""), None);
    }

    #[test]
    fn every_advertised_platform_resolves() {
        for (name, slug) in known_platforms() {
            assert_eq!(es_de_slug(name), Some(*slug), "mapping missing for {name}");
        }
    }
}
