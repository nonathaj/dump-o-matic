//! Plex's names for shows and films, and the TMDB tag in them.
//!
//! Plex matches a show folder or a film by its name and year, and is told exactly which
//! entry it is by a tag in braces: `Carnivale (2003) {tmdb-185}`, or for a film
//! `The Two Towers (2002) {edition-Extended} {tmdb-121} Part 1.mkv`. A name may also be
//! bare, `Bones` or `300.mkv`, from before the library was kept this way.

/// A show folder or film file name, taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The title as written, without year or tags.
    pub title: String,
    pub year: Option<u32>,
    /// Tags other than the TMDB one, without braces and in order: `edition-Extended`.
    pub tags: Vec<String>,
    /// The ID in a `{tmdb-…}` tag.
    pub tmdb: Option<u64>,
    /// Whatever follows the year and tags, with its leading space: ` Part 1`.
    pub rest: String,
    /// The file extension, without the dot; `None` for a folder.
    pub ext: Option<String>,
}

/// Take a name apart. `is_file` says whether it ends in an extension.
pub fn parse(name: &str, is_file: bool) -> Entry {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if is_file && !s.is_empty() && !e.contains(' ') => (s, Some(e.to_string())),
        _ => (name, None),
    };

    let mut title = String::new();
    let mut year = None;
    let mut tags = Vec::new();
    let mut tmdb = None;
    let mut rest = String::new();
    // Text before the first year or tag is the title, and text after one is the rest.
    // Parentheses that hold no year, `(Unrated)`, are text like any other.
    let mut past_title = false;
    let mut s = stem;
    while !s.is_empty() {
        let (text, group, after) = match s.find(['(', '{']) {
            Some(at) => {
                let close = if s[at..].starts_with('(') { ')' } else { '}' };
                match s[at..].find(close) {
                    Some(len) => (&s[..at], Some(&s[at..=at + len]), &s[at + len + 1..]),
                    None => (s, None, ""),
                }
            }
            None => (s, None, ""),
        };
        let into = if past_title { &mut rest } else { &mut title };
        into.push_str(text);
        if let Some(group) = group {
            let inner = &group[1..group.len() - 1];
            let as_year = inner
                .parse::<u32>()
                .ok()
                .filter(|y| group.starts_with('(') && inner.len() == 4 && (1880..=2100).contains(y));
            let as_id = inner
                .strip_prefix("tmdb-")
                .filter(|_| group.starts_with('{'))
                .and_then(|id| id.parse::<u64>().ok());
            if let Some(y) = as_year {
                year = Some(y);
                past_title = true;
            } else if let Some(id) = as_id {
                tmdb = Some(id);
                past_title = true;
            } else if group.starts_with('{') {
                tags.push(inner.to_string());
                past_title = true;
            } else {
                into.push_str(group);
            }
        }
        s = after;
    }

    let rest = rest.split_whitespace().collect::<Vec<_>>().join(" ");
    Entry {
        title: title.trim().to_string(),
        year,
        tags,
        tmdb,
        rest: if rest.is_empty() { rest } else { format!(" {rest}") },
        ext,
    }
}

impl Entry {
    /// The name with `year` and the TMDB tag `id`, keeping the title as written and any
    /// other tags, after them in Plex's order: title, year, edition, ID, the rest.
    pub fn tagged(&self, year: u32, id: u64) -> String {
        let mut s = format!("{} ({year})", self.title);
        for t in &self.tags {
            s.push_str(&format!(" {{{t}}}"));
        }
        s.push_str(&format!(" {{tmdb-{id}}}"));
        s.push_str(&self.rest);
        if let Some(e) = &self.ext {
            s.push('.');
            s.push_str(e);
        }
        s
    }
}

/// A show folder or film name as filed: `Title (Year) {tmdb-ID}`.
pub fn folder_name(title: &str, year: Option<u32>, id: u64) -> String {
    match year {
        Some(y) => format!("{title} ({y}) {{tmdb-{id}}}"),
        None => format!("{title} {{tmdb-{id}}}"),
    }
}

/// The TMDB ID in a name's `{tmdb-…}` tag, if it has one.
pub fn tmdb_id(name: &str) -> Option<u64> {
    let at = name.find("{tmdb-")?;
    let digits: String = name[at + 6..].chars().take_while(|c| c.is_ascii_digit()).collect();
    if name[at + 6 + digits.len()..].starts_with('}') {
        digits.parse().ok()
    } else {
        None
    }
}

/// Whether two titles name the same thing once case, punctuation and spacing are set
/// aside: `Bobs Burgers` and `Bob's Burgers`, `Avatar The Last Airbender` and
/// `Avatar: The Last Airbender`, `Law & Order` and `Law and Order`.
pub fn same_title(a: &str, b: &str) -> bool {
    let a = fold(a);
    !a.is_empty() && a == fold(b)
}

fn fold(s: &str) -> String {
    s.to_lowercase()
        .replace('&', "and")
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_bare_name() {
        let e = parse("Bones", false);
        assert_eq!((e.title.as_str(), e.year, e.tmdb), ("Bones", None, None));
        assert_eq!(e.tagged(2005, 1911), "Bones (2005) {tmdb-1911}");
    }

    #[test]
    fn reads_a_name_with_a_year() {
        let e = parse("Robot Chicken (2005)", false);
        assert_eq!((e.title.as_str(), e.year), ("Robot Chicken", Some(2005)));
        assert_eq!(e.tagged(2005, 709), "Robot Chicken (2005) {tmdb-709}");
    }

    #[test]
    fn reads_a_tagged_name() {
        let e = parse("Carnivale (2003) {tmdb-185}", false);
        assert_eq!((e.title.as_str(), e.year, e.tmdb), ("Carnivale", Some(2003), Some(185)));
    }

    #[test]
    fn keeps_an_edition_and_a_part_in_their_places() {
        let name = "The Lord of the Rings - The Two Towers (2002) {edition-Extended} {tmdb-121} Part 1.mkv";
        let e = parse(name, true);
        assert_eq!(e.title, "The Lord of the Rings - The Two Towers");
        assert_eq!(e.year, Some(2002));
        assert_eq!(e.tags, vec!["edition-Extended".to_string()]);
        assert_eq!(e.tmdb, Some(121));
        assert_eq!(e.rest, " Part 1");
        assert_eq!(e.ext.as_deref(), Some("mkv"));
        assert_eq!(e.tagged(2002, 121), name);
    }

    #[test]
    fn an_untagged_film_file_gains_a_year_and_a_tag() {
        let e = parse("2 Fast 2 Furious.mkv", true);
        assert_eq!(e.title, "2 Fast 2 Furious");
        assert_eq!(e.tagged(2003, 584), "2 Fast 2 Furious (2003) {tmdb-584}.mkv");
        let e = parse("Hot Tub Time Machine (2010) {edition-Unrated}.mkv", true);
        assert_eq!(e.tagged(2010, 23048), "Hot Tub Time Machine (2010) {edition-Unrated} {tmdb-23048}.mkv");
    }

    #[test]
    fn parentheses_that_are_not_a_year_stay_in_the_title() {
        let e = parse("Dear Consumer (The Christmas Special) (2009).mkv", true);
        assert_eq!(e.title, "Dear Consumer (The Christmas Special)");
        assert_eq!(e.year, Some(2009));
        let e = parse("Number 23 (Unrated)", false);
        assert_eq!((e.title.as_str(), e.year), ("Number 23 (Unrated)", None));
    }

    #[test]
    fn a_dot_in_a_folder_name_is_not_an_extension() {
        let e = parse("Aldnoah.Zero", false);
        assert_eq!(e.title, "Aldnoah.Zero");
        assert_eq!(e.ext, None);
    }

    #[test]
    fn finds_the_id_in_a_name() {
        assert_eq!(tmdb_id("Sense8 (2015) {tmdb-61664}"), Some(61664));
        assert_eq!(tmdb_id("Sense8 (2015)"), None);
        assert_eq!(tmdb_id("x {tmdb-12"), None);
    }

    #[test]
    fn titles_compare_without_punctuation() {
        assert!(same_title("Bobs Burgers", "Bob's Burgers"));
        assert!(same_title("Avatar The Last Airbender", "Avatar: The Last Airbender"));
        assert!(same_title("Law & Order", "Law and Order"));
        assert!(!same_title("Cosmos", "Cosmos: A Spacetime Odyssey"));
        assert!(!same_title("", ""));
    }

    #[test]
    fn a_folder_name_puts_the_tag_last() {
        assert_eq!(folder_name("Robot Chicken", Some(2005), 709), "Robot Chicken (2005) {tmdb-709}");
    }
}
