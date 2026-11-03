//! Where a film's extras go, so that Plex finds them.
//!
//! Plex reads a film's extras from the film's own folder: the film beside a subfolder
//! per kind of extra, named from a fixed list. A film kept as a bare file has no folder
//! to put them in, so a film with extras is filed as
//!
//! ```text
//! Title (Year)/
//!   Title (Year).mkv
//!   Featurettes/The Making of Title.mkv
//!   Deleted Scenes/Cut Scene.mkv
//! ```
//!
//! The name of an extra's file is what Plex shows for it, and the subfolders cannot be
//! nested.

use std::fmt;

/// One of the subfolder names Plex recognises for a film's extras.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    BehindTheScenes,
    DeletedScenes,
    Featurettes,
    Interviews,
    Scenes,
    Shorts,
    Trailers,
    Other,
}

impl Category {
    pub const ALL: [Category; 8] = [
        Category::BehindTheScenes,
        Category::DeletedScenes,
        Category::Featurettes,
        Category::Interviews,
        Category::Scenes,
        Category::Shorts,
        Category::Trailers,
        Category::Other,
    ];

    /// The folder name Plex looks for.
    pub fn folder(self) -> &'static str {
        match self {
            Category::BehindTheScenes => "Behind The Scenes",
            Category::DeletedScenes => "Deleted Scenes",
            Category::Featurettes => "Featurettes",
            Category::Interviews => "Interviews",
            Category::Scenes => "Scenes",
            Category::Shorts => "Shorts",
            Category::Trailers => "Trailers",
            Category::Other => "Other",
        }
    }

    /// Read a category as a person would type it: any case, with or without spaces,
    /// singular or plural.
    pub fn parse(s: &str) -> Option<Category> {
        let key: String = s
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase();
        Some(match key.as_str() {
            "behindthescenes" | "bts" => Category::BehindTheScenes,
            "deletedscenes" | "deletedscene" | "deleted" => Category::DeletedScenes,
            "featurettes" | "featurette" => Category::Featurettes,
            "interviews" | "interview" => Category::Interviews,
            "scenes" | "scene" => Category::Scenes,
            "shorts" | "short" => Category::Shorts,
            "trailers" | "trailer" => Category::Trailers,
            "other" | "extra" | "extras" => Category::Other,
            _ => return None,
        })
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.folder())
    }
}

/// A title the operator has said is an extra: which title, of what kind, called what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extra {
    pub title_name: String,
    pub category: Category,
    pub name: String,
}

/// Read `TITLE=Category/Name`, e.g. `title_t03.mkv=Featurettes/The Making of Beavers`.
pub fn parse_spec(spec: &str) -> Result<Extra, String> {
    let (title, rest) = spec
        .split_once('=')
        .ok_or_else(|| format!("--extra {spec:?}: expected TITLE=Category/Name"))?;
    let (category, name) = rest
        .split_once('/')
        .ok_or_else(|| format!("--extra {spec:?}: expected a category and a name, as Category/Name"))?;
    let category = Category::parse(category).ok_or_else(|| {
        format!(
            "--extra {spec:?}: {:?} is not a Plex extras folder; use one of {}",
            category.trim(),
            Category::ALL.map(|c| c.folder()).join(", ")
        )
    })?;
    let (title, name) = (title.trim(), name.trim());
    if title.is_empty() || name.is_empty() {
        return Err(format!("--extra {spec:?}: the title and the name must not be empty"));
    }
    Ok(Extra {
        title_name: title.to_string(),
        category,
        name: name.to_string(),
    })
}

/// The film's folder, which is also the stem of the film's file: `Title (Year)`.
pub fn film_folder(title: &str, year: Option<u32>) -> String {
    match year {
        Some(y) => format!("{title} ({y})"),
        None => title.to_string(),
    }
}

/// Staging-relative path of a film filed in its own folder.
pub fn feature_path(folder: &str, ext: &str) -> String {
    format!("ready/movies/{folder}/{folder}.{ext}")
}

/// Staging-relative path of one extra of the film in `folder`.
pub fn extra_path(folder: &str, category: Category, name: &str, ext: &str) -> String {
    format!("ready/movies/{folder}/{}/{name}.{ext}", category.folder())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_category_however_it_is_typed() {
        for s in ["Featurettes", "featurette", " FEATURETTES "] {
            assert_eq!(Category::parse(s), Some(Category::Featurettes));
        }
        assert_eq!(Category::parse("behind the scenes"), Some(Category::BehindTheScenes));
        assert_eq!(Category::parse("Behind-The-Scenes"), Some(Category::BehindTheScenes));
        assert_eq!(Category::parse("deleted"), Some(Category::DeletedScenes));
        assert_eq!(Category::parse("trailer"), Some(Category::Trailers));
        assert_eq!(Category::parse("bloopers"), None);
    }

    #[test]
    fn every_category_round_trips_through_its_folder_name() {
        for c in Category::ALL {
            assert_eq!(Category::parse(c.folder()), Some(c));
        }
    }

    #[test]
    fn reads_an_extra() {
        let e = parse_spec("title_t03.mkv=Featurettes/The Making of Beavers").unwrap();
        assert_eq!(e.title_name, "title_t03.mkv");
        assert_eq!(e.category, Category::Featurettes);
        assert_eq!(e.name, "The Making of Beavers");
    }

    #[test]
    fn refuses_an_extra_it_cannot_place() {
        assert!(parse_spec("title_t03.mkv").is_err());
        assert!(parse_spec("title_t03.mkv=Featurettes").is_err());
        assert!(parse_spec("title_t03.mkv=Bloopers/Fails").unwrap_err().contains("Behind The Scenes"));
        assert!(parse_spec("=Featurettes/Name").is_err());
        assert!(parse_spec("t.mkv=Featurettes/ ").is_err());
    }

    #[test]
    fn lays_a_film_and_its_extras_out_as_plex_expects() {
        let folder = film_folder("Hundreds of Beavers", Some(2022));
        assert_eq!(folder, "Hundreds of Beavers (2022)");
        assert_eq!(
            feature_path(&folder, "mkv"),
            "ready/movies/Hundreds of Beavers (2022)/Hundreds of Beavers (2022).mkv"
        );
        assert_eq!(
            extra_path(&folder, Category::DeletedScenes, "Cut Scene", "mkv"),
            "ready/movies/Hundreds of Beavers (2022)/Deleted Scenes/Cut Scene.mkv"
        );
    }
}
