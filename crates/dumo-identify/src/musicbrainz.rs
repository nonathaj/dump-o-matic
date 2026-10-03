//! MusicBrainz client: identify an audio CD from its disc ID.
//!
//! The disc ID is a hash of the table of contents read off the disc itself, so a lookup
//! is an exact identification of the pressing — the audio equivalent of a Redump hash
//! match, keyed on layout rather than content. Read-only, no credentials.
//!
//! Metadata is shaped the way MusicBrainz Picard writes it, because that is what an
//! existing library tagged by Picard already looks like: artist names translated to
//! their English alias (Picard's "translate artist names"), titles left in their own
//! script.

use crate::http;

const API_BASE: &str = "https://musicbrainz.org/ws/2";
const TIMEOUT_SECS: u32 = 20;
/// MusicBrainz rejects anonymous clients; the agent must name the application and a
/// way to reach its maintainer.
const USER_AGENT: &str = concat!(
    "dump-o-matic/",
    env!("CARGO_PKG_VERSION"),
    " ( ",
    env!("CARGO_PKG_REPOSITORY"),
    " )"
);
/// The locale artist names are translated into, as Picard's default does for English.
const ARTIST_LOCALE: &str = "en";

#[derive(Debug, thiserror::Error)]
pub enum MusicBrainzError {
    #[error(transparent)]
    Http(#[from] http::HttpError),

    #[error("unexpected response from MusicBrainz: {0}")]
    Malformed(String),
}

type Result<T> = std::result::Result<T, MusicBrainzError>;

/// An artist credit, flattened: the display string plus the parts Picard tags.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Credit {
    /// Names joined with their join phrases, e.g. `Mel Gibson & Judy Kuhn`.
    pub name: String,
    /// Sort names joined the same way, e.g. `Sawano, Hiroyuki`.
    pub sort: String,
    /// Each credited artist's name on its own (Picard's `ARTISTS`).
    pub artists: Vec<String>,
    pub artist_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub id: String,
    pub recording_id: String,
    pub position: u32,
    pub title: String,
    pub length_ms: Option<u64>,
    pub artist: Credit,
    pub isrc: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Medium {
    pub position: u32,
    pub format: Option<String>,
    pub disc_ids: Vec<String>,
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub id: String,
    pub title: String,
    pub artist: Credit,
    pub date: Option<String>,
    pub country: Option<String>,
    pub status: Option<String>,
    pub barcode: Option<String>,
    pub script: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub release_group_id: String,
    /// Primary type, lowercased as Picard writes it: `album`, `single`, `ep`, ...
    pub release_type: Option<String>,
    pub original_date: Option<String>,
    pub genre: Option<String>,
    pub media: Vec<Medium>,
}

impl Release {
    /// The medium a given disc ID belongs to — which disc of a set this one is.
    pub fn medium_for(&self, disc_id: &str) -> Option<&Medium> {
        self.media
            .iter()
            .find(|m| m.disc_ids.iter().any(|d| d == disc_id))
    }

    /// Year for folder names: the release's own date, as Picard's `%date%` uses.
    pub fn year(&self) -> Option<&str> {
        self.date.as_deref().and_then(|d| d.get(..4))
    }

    /// Picard sets `compilation` when the album artist is Various Artists.
    pub fn is_compilation(&self) -> bool {
        self.artist.artist_ids.iter().any(|id| id == VARIOUS_ARTISTS_ID)
    }
}

/// MusicBrainz's special-purpose "Various Artists" artist.
pub const VARIOUS_ARTISTS_ID: &str = "89ad4ac3-39f7-470e-963a-56509c546377";

/// Look up every release carrying this disc ID. An unknown disc is an empty list, not
/// an error.
pub fn lookup_disc_id(disc_id: &str) -> Result<Vec<Release>> {
    let url = format!(
        "{API_BASE}/discid/{}?inc=artist-credits+aliases+recordings+release-groups+labels+isrcs+genres&fmt=json",
        http::urlencode(disc_id)
    );
    let headers = [("User-Agent", USER_AGENT.to_string())];
    let body = match http::get(&url, &headers, TIMEOUT_SECS) {
        Ok(b) => b,
        Err(http::HttpError::Status { status: 404, .. }) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| MusicBrainzError::Malformed(e.to_string()))?;
    Ok(parse_releases(&v))
}

/// Parse a `/discid` response's `releases` array.
pub fn parse_releases(v: &serde_json::Value) -> Vec<Release> {
    v["releases"]
        .as_array()
        .map(|rs| rs.iter().filter_map(parse_release).collect())
        .unwrap_or_default()
}

fn s(v: &serde_json::Value) -> Option<String> {
    v.as_str().filter(|x| !x.is_empty()).map(str::to_string)
}

fn parse_release(r: &serde_json::Value) -> Option<Release> {
    let rg = &r["release-group"];
    let label_info = r["label-info"].as_array().and_then(|l| l.first());
    let media = r["media"]
        .as_array()
        .map(|ms| ms.iter().map(parse_medium).collect())
        .unwrap_or_default();
    Some(Release {
        id: s(&r["id"])?,
        title: s(&r["title"])?,
        artist: parse_credit(&r["artist-credit"]),
        date: s(&r["date"]),
        country: s(&r["country"]),
        status: s(&r["status"]).map(|x| x.to_lowercase()),
        barcode: s(&r["barcode"]),
        script: s(&r["text-representation"]["script"]),
        label: label_info.and_then(|l| s(&l["label"]["name"])),
        catalog_number: label_info.and_then(|l| s(&l["catalog-number"])),
        release_group_id: s(&rg["id"]).unwrap_or_default(),
        release_type: s(&rg["primary-type"]).map(|x| x.to_lowercase()),
        original_date: s(&rg["first-release-date"]),
        genre: top_genre(&r["genres"]).or_else(|| top_genre(&rg["genres"])),
        media,
    })
}

fn parse_medium(m: &serde_json::Value) -> Medium {
    Medium {
        position: m["position"].as_u64().unwrap_or(1) as u32,
        format: s(&m["format"]),
        disc_ids: m["discs"]
            .as_array()
            .map(|ds| ds.iter().filter_map(|d| s(&d["id"])).collect())
            .unwrap_or_default(),
        tracks: m["tracks"]
            .as_array()
            .map(|ts| ts.iter().filter_map(parse_track).collect())
            .unwrap_or_default(),
    }
}

fn parse_track(t: &serde_json::Value) -> Option<Track> {
    let rec = &t["recording"];
    Some(Track {
        id: s(&t["id"])?,
        recording_id: s(&rec["id"]).unwrap_or_default(),
        position: t["position"].as_u64()? as u32,
        title: s(&t["title"])?,
        length_ms: t["length"].as_u64().or_else(|| rec["length"].as_u64()),
        // The track's own credit wins; the recording's is the fallback.
        artist: if t["artist-credit"].is_array() {
            parse_credit(&t["artist-credit"])
        } else {
            parse_credit(&rec["artist-credit"])
        },
        isrc: rec["isrcs"]
            .as_array()
            .and_then(|i| i.first())
            .and_then(s),
    })
}

/// The most-voted genre, title-cased as it appears in a Picard-tagged library. A tie
/// goes to the one MusicBrainz lists first (`max_by_key` alone would pick the last).
fn top_genre(v: &serde_json::Value) -> Option<String> {
    let best = v
        .as_array()?
        .iter()
        .rev()
        .max_by_key(|g| g["count"].as_u64().unwrap_or(0))?;
    s(&best["name"]).map(|n| title_case(&n))
}

fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_credit(v: &serde_json::Value) -> Credit {
    let mut c = Credit::default();
    for part in v.as_array().into_iter().flatten() {
        let artist = &part["artist"];
        let credited = s(&part["name"]).or_else(|| s(&artist["name"])).unwrap_or_default();
        let (name, sort) = translate_artist(artist, &credited);
        let join = part["joinphrase"].as_str().unwrap_or("");
        c.name.push_str(&name);
        c.name.push_str(join);
        c.sort.push_str(&sort);
        c.sort.push_str(join);
        c.artists.push(name);
        if let Some(id) = s(&artist["id"]) {
            c.artist_ids.push(id);
        }
    }
    c
}

/// Picard's artist-name translation: the primary alias in the target locale, else any
/// alias in it, else — for a name not in Latin script — the sort name turned back
/// round ("Sawano, Hiroyuki" -> "Hiroyuki Sawano"). Returns (name, sort name).
fn translate_artist(artist: &serde_json::Value, credited: &str) -> (String, String) {
    let sort = s(&artist["sort-name"]).unwrap_or_else(|| credited.to_string());
    let aliases: Vec<&serde_json::Value> = artist["aliases"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|x| x["locale"].as_str() == Some(ARTIST_LOCALE))
                .collect()
        })
        .unwrap_or_default();
    let alias = aliases
        .iter()
        .find(|a| a["primary"].as_bool() == Some(true))
        .or_else(|| aliases.first());
    if let Some(a) = alias {
        let name = s(&a["name"]).unwrap_or_else(|| credited.to_string());
        let alias_sort = s(&a["sort-name"]).unwrap_or(sort);
        return (name, alias_sort);
    }
    if !is_latin(credited) && is_latin(&sort) {
        return (unsort(&sort), sort);
    }
    (credited.to_string(), sort)
}

fn is_latin(s: &str) -> bool {
    s.chars()
        .filter(|c| c.is_alphabetic())
        .all(|c| (c as u32) < 0x0250)
}

/// "Sawano, Hiroyuki" -> "Hiroyuki Sawano". Names without a comma are left alone.
fn unsort(sort: &str) -> String {
    match sort.split_once(", ") {
        Some((last, first)) => format!("{first} {last}"),
        None => sort.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from the real response for disc ID `_ZoSLV2HFTtEQL1AWW4DMc2LJ2k-`
    /// (Aldnoah.Zero OST, JP Blu-spec CD), keeping the fields that matter.
    fn sample() -> serde_json::Value {
        serde_json::from_str(
            r#"{"releases": [{
            "id": "f8659b68-c6ad-443e-a006-0bcb468f3976",
            "title": "アルドノア・ゼロ オリジナル・サウンドトラック",
            "date": "2014-09-10", "country": "JP", "status": "Official",
            "barcode": "4534530078803",
            "text-representation": {"script": "Jpan", "language": "jpn"},
            "genres": [],
            "label-info": [{"catalog-number": "SVWC-70016", "label": {"name": "Aniplex"}}],
            "artist-credit": [{"name": "澤野弘之", "joinphrase": "", "artist": {
                "id": "4c5a8b4a-ba6f-4f4b-8e4c-4c4d2b8f1e5e",
                "name": "澤野弘之", "sort-name": "Sawano, Hiroyuki",
                "aliases": [
                    {"locale": "zh_Hans", "name": "泽野弘之", "sort-name": "泽野弘之", "primary": true},
                    {"locale": "en", "name": "Hiroyuki Sawano", "sort-name": "Sawano, Hiroyuki", "primary": true}
                ]}}],
            "release-group": {
                "id": "rg-1", "primary-type": "Album", "first-release-date": "2014-09-10",
                "genres": [{"name": "j-rock", "count": 1}, {"name": "electronic", "count": 2}]
            },
            "media": [{"position": 1, "format": "Blu-spec CD",
                "discs": [{"id": "_ZoSLV2HFTtEQL1AWW4DMc2LJ2k-"}],
                "tracks": [{
                    "id": "9e0148e0-5d58-4c7c-bdaf-1e7c7329e5ed", "position": 1,
                    "title": "No differences", "length": 277013,
                    "artist-credit": [{"name": "澤野弘之", "joinphrase": "", "artist": {
                        "id": "4c5a8b4a-ba6f-4f4b-8e4c-4c4d2b8f1e5e", "name": "澤野弘之",
                        "sort-name": "Sawano, Hiroyuki",
                        "aliases": [{"locale": "en", "name": "Hiroyuki Sawano", "primary": true}]}}],
                    "recording": {"id": "39685808-5691-4166-96f5-28283fd61fa6", "isrcs": ["JPE301400782"]}
                }]}]
        }]}"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_a_disc_id_release() {
        let rs = parse_releases(&sample());
        assert_eq!(rs.len(), 1);
        let r = &rs[0];
        assert_eq!(r.year(), Some("2014"));
        assert_eq!(r.label.as_deref(), Some("Aniplex"));
        assert_eq!(r.catalog_number.as_deref(), Some("SVWC-70016"));
        assert_eq!(r.release_type.as_deref(), Some("album"));
        assert_eq!(r.status.as_deref(), Some("official"));
        assert_eq!(r.script.as_deref(), Some("Jpan"));
        let m = r.medium_for("_ZoSLV2HFTtEQL1AWW4DMc2LJ2k-").expect("medium");
        let t = &m.tracks[0];
        assert_eq!(t.isrc.as_deref(), Some("JPE301400782"));
        assert_eq!(t.length_ms, Some(277013));
        assert_eq!(t.recording_id, "39685808-5691-4166-96f5-28283fd61fa6");
    }

    /// Titles keep their own script; artist names are translated, as Picard does.
    #[test]
    fn translates_artist_names_but_not_titles() {
        let r = &parse_releases(&sample())[0];
        assert_eq!(r.artist.name, "Hiroyuki Sawano");
        assert_eq!(r.artist.sort, "Sawano, Hiroyuki");
        assert_eq!(r.title, "アルドノア・ゼロ オリジナル・サウンドトラック");
        assert_eq!(r.media[0].tracks[0].artist.name, "Hiroyuki Sawano");
    }

    #[test]
    fn a_non_latin_name_without_an_alias_falls_back_to_its_sort_name() {
        let artist = serde_json::json!({"name": "澤野弘之", "sort-name": "Sawano, Hiroyuki"});
        assert_eq!(translate_artist(&artist, "澤野弘之").0, "Hiroyuki Sawano");
        let latin = serde_json::json!({"name": "AC/DC", "sort-name": "AC/DC"});
        assert_eq!(translate_artist(&latin, "AC/DC").0, "AC/DC");
    }

    #[test]
    fn joins_multiple_credits_with_their_join_phrases() {
        let v = serde_json::json!([
            {"name": "Mel Gibson", "joinphrase": " & ", "artist": {"id": "a", "sort-name": "Gibson, Mel"}},
            {"name": "Judy Kuhn", "joinphrase": "", "artist": {"id": "b", "sort-name": "Kuhn, Judy"}}
        ]);
        let c = parse_credit(&v);
        assert_eq!(c.name, "Mel Gibson & Judy Kuhn");
        assert_eq!(c.sort, "Gibson, Mel & Kuhn, Judy");
        assert_eq!(c.artists, vec!["Mel Gibson", "Judy Kuhn"]);
    }

    #[test]
    fn genre_prefers_the_release_then_the_most_voted_group_genre() {
        let r = &parse_releases(&sample())[0];
        assert_eq!(r.genre.as_deref(), Some("Electronic"));
    }

    /// The real release group lists electronic and j-rock with one vote each.
    #[test]
    fn a_genre_tie_goes_to_the_first_listed() {
        let v = serde_json::json!([{"name": "electronic", "count": 1}, {"name": "j-rock", "count": 1}]);
        assert_eq!(top_genre(&v).as_deref(), Some("Electronic"));
    }

    #[test]
    fn an_empty_response_is_no_releases() {
        assert!(parse_releases(&serde_json::json!({})).is_empty());
    }
}
