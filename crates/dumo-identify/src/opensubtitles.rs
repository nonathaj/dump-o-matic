//! OpenSubtitles client: find and download reference subtitles for a known episode or
//! film, to check a ripped title's own dialogue against.
//!
//! Read-only. Searching needs only the consumer API key; downloading also needs an
//! account, and is metered per day (a free account gets a small daily allowance), so
//! callers should cache what they download — a reference subtitle never changes.
//!
//! Credentials travel in headers and request bodies, both written to curl over stdin by
//! [`crate::http`], never in a URL or on a command line.

use crate::http;
use dumo_core::config::ApiConfig;
use std::cell::RefCell;

const API_BASE: &str = "https://api.opensubtitles.com/api/v1";
const TIMEOUT_SECS: u32 = 30;

#[derive(Debug, thiserror::Error)]
pub enum OpenSubtitlesError {
    #[error(
        "OpenSubtitles needs api.opensubtitles_api_key, api.opensubtitles_username and \
         api.opensubtitles_password (or DUMO_OPENSUBTITLES_API_KEY, _USERNAME, _PASSWORD)"
    )]
    NoCredential,

    #[error("OpenSubtitles rejected the credentials (HTTP {0})")]
    Unauthorized(u16),

    #[error("the OpenSubtitles download allowance for today is used up; try again later")]
    QuotaExhausted,

    #[error(transparent)]
    Http(#[from] http::HttpError),

    #[error("unexpected response from {endpoint}: {detail}")]
    Malformed { endpoint: String, detail: String },
}

type Result<T> = std::result::Result<T, OpenSubtitlesError>;

/// One downloadable subtitle file from a search.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleFile {
    pub file_id: u64,
    pub release: String,
    pub download_count: u64,
    pub hearing_impaired: bool,
    pub from_trusted: bool,
    /// Machine- or AI-translated: words chosen by a translator, not the ones spoken, so
    /// useless as a reference for what a title's dialogue should say.
    pub translated: bool,
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

pub struct OpenSubtitlesClient {
    api_key: String,
    username: String,
    password: String,
    /// The session token and the API host to use with it, once logged in. Login is
    /// lazy: a run that finds everything in the cache never needs it.
    session: RefCell<Option<(String, String)>>,
}

/// Written by hand rather than derived, so credentials never reach a log.
impl std::fmt::Debug for OpenSubtitlesClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenSubtitlesClient")
            .field("api_key", &"<redacted>")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl OpenSubtitlesClient {
    pub fn from_config(api: &ApiConfig) -> Result<Self> {
        fn field(v: &Option<String>) -> Option<String> {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        }
        Ok(Self {
            api_key: field(&api.opensubtitles_api_key).ok_or(OpenSubtitlesError::NoCredential)?,
            username: field(&api.opensubtitles_username)
                .ok_or(OpenSubtitlesError::NoCredential)?,
            password: field(&api.opensubtitles_password)
                .ok_or(OpenSubtitlesError::NoCredential)?,
            session: RefCell::new(None),
        })
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Api-Key", self.api_key.clone()),
            // OpenSubtitles refuses requests without an application User-Agent.
            ("User-Agent", user_agent()),
            ("Accept", "application/json".to_string()),
        ]
    }

    fn parse(endpoint: &str, body: &str) -> Result<serde_json::Value> {
        serde_json::from_str(body).map_err(|e| OpenSubtitlesError::Malformed {
            endpoint: endpoint.to_string(),
            detail: e.to_string(),
        })
    }

    fn get_json(&self, path: &str, query: &str) -> Result<serde_json::Value> {
        let url = format!("{API_BASE}{path}?{query}");
        let body = http::get(&url, &self.headers(), TIMEOUT_SECS).map_err(map_status)?;
        Self::parse(path, &body)
    }

    /// English (or any language's) subtitles for one episode of a series.
    pub fn search_episode(
        &self,
        parent_tmdb_id: u64,
        season: u32,
        episode: u32,
        language: &str,
    ) -> Result<Vec<SubtitleFile>> {
        let query = search_query(&[
            ("episode_number", episode.to_string()),
            ("languages", language.to_string()),
            ("parent_tmdb_id", parent_tmdb_id.to_string()),
            ("season_number", season.to_string()),
        ]);
        let v = self.get_json("/subtitles", &query)?;
        Ok(parse_search(&v)
            .into_iter()
            // The search matches loosely; keep only files that say they are this
            // episode, so a season pack or a neighbour is never taken for it.
            .filter(|f| f.season == Some(season) && f.episode == Some(episode))
            .collect())
    }

    /// Subtitles for a film.
    pub fn search_movie(&self, tmdb_id: u64, language: &str) -> Result<Vec<SubtitleFile>> {
        let query = search_query(&[
            ("languages", language.to_string()),
            ("tmdb_id", tmdb_id.to_string()),
        ]);
        let v = self.get_json("/subtitles", &query)?;
        Ok(parse_search(&v))
    }

    fn login(&self) -> Result<(String, String)> {
        if let Some(s) = self.session.borrow().as_ref() {
            return Ok(s.clone());
        }
        let body = serde_json::json!({ "username": self.username, "password": self.password })
            .to_string();
        let resp = http::post_json(&format!("{API_BASE}/login"), &self.headers(), &body, TIMEOUT_SECS)
            .map_err(map_status)?;
        let v = Self::parse("/login", &resp)?;
        let token = v
            .get("token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| OpenSubtitlesError::Malformed {
                endpoint: "/login".into(),
                detail: "no token in the response".into(),
            })?
            .to_string();
        // An account may be told to use a different host (VIP accounts are); the token
        // is only good there.
        let base = v
            .get("base_url")
            .and_then(|b| b.as_str())
            .filter(|b| !b.is_empty())
            .map(|b| format!("https://{}/api/v1", b.trim_start_matches("https://")))
            .unwrap_or_else(|| API_BASE.to_string());
        *self.session.borrow_mut() = Some((token.clone(), base.clone()));
        Ok((token, base))
    }

    /// Download one subtitle file as text. Counts against the day's allowance.
    pub fn download(&self, file_id: u64) -> Result<String> {
        let (token, base) = self.login()?;
        let mut headers = self.headers();
        headers.push(("Authorization", format!("Bearer {token}")));
        let body = serde_json::json!({ "file_id": file_id }).to_string();
        let resp = http::post_json(&format!("{base}/download"), &headers, &body, TIMEOUT_SECS)
            .map_err(map_status)?;
        let v = Self::parse("/download", &resp)?;
        let link = v
            .get("link")
            .and_then(|l| l.as_str())
            .ok_or_else(|| OpenSubtitlesError::Malformed {
                endpoint: "/download".into(),
                detail: "no link in the response".into(),
            })?;
        // The link is a short-lived signed URL; it needs no credentials, so none are sent.
        let bytes = http::get_bytes(link, &[("User-Agent", user_agent())], TIMEOUT_SECS)?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }
}

fn user_agent() -> String {
    format!("dump-o-matic v{}", env!("CARGO_PKG_VERSION"))
}

fn map_status(e: http::HttpError) -> OpenSubtitlesError {
    match e {
        http::HttpError::Status { status: s @ (401 | 403), .. } => OpenSubtitlesError::Unauthorized(s),
        // 406 is how /download says the allowance is spent.
        http::HttpError::Status { status: 406, .. } => OpenSubtitlesError::QuotaExhausted,
        e => e.into(),
    }
}

/// Build a search query string.
///
/// OpenSubtitles asks for parameters in alphabetical order and redirects any request
/// that is not, so they are sorted here rather than trusted to the caller.
fn search_query(params: &[(&str, String)]) -> String {
    let mut params = params.to_vec();
    params.sort_by(|a, b| a.0.cmp(b.0));
    params
        .iter()
        .map(|(k, v)| format!("{k}={}", http::urlencode(&v.to_lowercase())))
        .collect::<Vec<_>>()
        .join("&")
}

// --- Response parsing, split out so it can be tested without network access ---

pub fn parse_search(v: &serde_json::Value) -> Vec<SubtitleFile> {
    let Some(items) = v.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let Some(a) = item.get("attributes") else { continue };
        let flag = |k: &str| a.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
        let details = a.get("feature_details");
        let number = |k: &str| {
            details
                .and_then(|d| d.get(k))
                .and_then(|n| n.as_u64())
                .map(|n| n as u32)
        };
        let release = a
            .get("release")
            .and_then(|r| r.as_str())
            .unwrap_or_default()
            .to_string();
        let download_count = a.get("download_count").and_then(|n| n.as_u64()).unwrap_or(0);
        // A subtitle entry can hold several files (CD1/CD2 of an old rip); each is
        // downloadable on its own, but only a single-file entry is the whole thing.
        let files = a.get("files").and_then(|f| f.as_array());
        let Some([file]) = files.map(Vec::as_slice) else { continue };
        let Some(file_id) = file.get("file_id").and_then(|n| n.as_u64()) else {
            continue;
        };
        out.push(SubtitleFile {
            file_id,
            release: release.clone(),
            download_count,
            hearing_impaired: flag("hearing_impaired"),
            from_trusted: flag("from_trusted"),
            translated: flag("machine_translated") || flag("ai_translated"),
            season: number("season_number"),
            episode: number("episode_number"),
        });
    }
    out
}

/// The file most worth spending a download on.
///
/// Never a translation. Among the rest, a trusted uploader first, then whichever most
/// people downloaded — a subtitle that matched nobody's copy gets few downloads.
/// Hearing-impaired files are not preferred or avoided: a disc's own captions are
/// written for the hearing impaired, but the speaker labels and sound descriptions are
/// stripped from both sides before comparing, leaving the same dialogue either way.
pub fn pick_best(files: &[SubtitleFile]) -> Option<&SubtitleFile> {
    files
        .iter()
        .filter(|f| !f.translated)
        .max_by_key(|f| (f.from_trusted, f.download_count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: u64, downloads: u64, trusted: bool, translated: bool) -> SubtitleFile {
        SubtitleFile {
            file_id: id,
            release: String::new(),
            download_count: downloads,
            hearing_impaired: false,
            from_trusted: trusted,
            translated,
            season: Some(1),
            episode: Some(2),
        }
    }

    #[test]
    fn query_parameters_are_alphabetical_and_lowercase() {
        let q = search_query(&[
            ("season_number", "3".into()),
            ("languages", "EN".into()),
            ("episode_number", "31".into()),
            ("parent_tmdb_id", "3644".into()),
        ]);
        assert_eq!(
            q,
            "episode_number=31&languages=en&parent_tmdb_id=3644&season_number=3"
        );
    }

    #[test]
    fn parses_search_results() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"total_pages":1,"data":[
              {"id":"1","type":"subtitle","attributes":{
                "language":"en","download_count":812,"hearing_impaired":true,
                "machine_translated":false,"ai_translated":false,"from_trusted":true,
                "release":"Mighty Morphin Power Rangers S03E31 DVDRip",
                "feature_details":{"feature_type":"Episode","season_number":3,
                  "episode_number":31,"parent_tmdb_id":3644},
                "files":[{"file_id":4567,"cd_number":1,"file_name":"mmpr.s03e31.srt"}]}},
              {"id":"2","type":"subtitle","attributes":{
                "download_count":9,"machine_translated":true,
                "feature_details":{"season_number":3,"episode_number":31},
                "files":[{"file_id":4568}]}},
              {"id":"3","type":"subtitle","attributes":{
                "download_count":50,
                "feature_details":{"season_number":3,"episode_number":31},
                "files":[{"file_id":1},{"file_id":2}]}}
            ]}"#,
        )
        .unwrap();
        let files = parse_search(&v);
        assert_eq!(files.len(), 2, "a two-file entry is not one whole subtitle");
        assert_eq!(files[0].file_id, 4567);
        assert_eq!(files[0].download_count, 812);
        assert!(files[0].hearing_impaired && files[0].from_trusted && !files[0].translated);
        assert_eq!((files[0].season, files[0].episode), (Some(3), Some(31)));
        assert!(files[1].translated);
    }

    #[test]
    fn empty_or_missing_data_is_empty_not_an_error() {
        let v: serde_json::Value = serde_json::from_str(r#"{"data":[]}"#).unwrap();
        assert!(parse_search(&v).is_empty());
        let v: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert!(parse_search(&v).is_empty());
    }

    #[test]
    fn best_file_is_trusted_then_most_downloaded_and_never_translated() {
        let files = vec![
            file(1, 5000, false, true),
            file(2, 300, false, false),
            file(3, 40, true, false),
        ];
        assert_eq!(pick_best(&files).map(|f| f.file_id), Some(3));
        let untrusted = vec![file(1, 5000, false, true), file(2, 300, false, false)];
        assert_eq!(pick_best(&untrusted).map(|f| f.file_id), Some(2));
        assert_eq!(pick_best(&[file(1, 5000, false, true)]), None);
    }

    #[test]
    fn every_credential_is_required() {
        let mut api = ApiConfig {
            opensubtitles_api_key: Some("key".into()),
            opensubtitles_username: Some("user".into()),
            ..Default::default()
        };
        assert!(matches!(
            OpenSubtitlesClient::from_config(&api),
            Err(OpenSubtitlesError::NoCredential)
        ));
        api.opensubtitles_password = Some("  ".into());
        assert!(OpenSubtitlesClient::from_config(&api).is_err(), "blank is missing");
        api.opensubtitles_password = Some("pw".into());
        assert!(OpenSubtitlesClient::from_config(&api).is_ok());
    }

    #[test]
    fn debug_redacts_credentials() {
        let api = ApiConfig {
            opensubtitles_api_key: Some("the-key".into()),
            opensubtitles_username: Some("user".into()),
            opensubtitles_password: Some("the-password".into()),
            ..Default::default()
        };
        let shown = format!("{:?}", OpenSubtitlesClient::from_config(&api).unwrap());
        assert!(!shown.contains("the-key") && !shown.contains("the-password"));
    }
}
