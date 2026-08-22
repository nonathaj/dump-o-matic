//! TMDB client: search for shows and films, and fetch episode lists.
//!
//! Read-only. Credentials come from config or environment (never bundled), and a v4
//! Read Access Token is sent as an `Authorization` header so the secret stays out of
//! URLs — see [`crate::http`] for why that matters.

use crate::http;
use dumo_core::config::{ApiConfig, TmdbAuth};

const API_BASE: &str = "https://api.themoviedb.org/3";
const TIMEOUT_SECS: u32 = 20;

#[derive(Debug, thiserror::Error)]
pub enum TmdbError {
    #[error("no TMDB credential configured; set api.tmdb_api_key or DUMO_TMDB_TOKEN")]
    NoCredential,

    #[error("TMDB rejected the credential (HTTP 401). Check the token is a v4 Read Access Token or a v3 API key")]
    Unauthorized,

    #[error(transparent)]
    Http(#[from] http::HttpError),

    #[error("unexpected response from {endpoint}: {detail}")]
    Malformed { endpoint: String, detail: String },
}

type Result<T> = std::result::Result<T, TmdbError>;

/// A TV series search result.
#[derive(Debug, Clone, PartialEq)]
pub struct TvResult {
    pub id: u64,
    pub name: String,
    pub first_air_date: Option<String>,
    pub overview: Option<String>,
}

impl TvResult {
    /// Year of first broadcast, which is what naming conventions use.
    pub fn year(&self) -> Option<u32> {
        self.first_air_date
            .as_ref()
            .and_then(|d| d.get(0..4))
            .and_then(|y| y.parse().ok())
    }
}

/// A movie search result.
#[derive(Debug, Clone, PartialEq)]
pub struct MovieResult {
    pub id: u64,
    pub title: String,
    pub release_date: Option<String>,
    pub runtime_mins: Option<u32>,
}

impl MovieResult {
    pub fn year(&self) -> Option<u32> {
        self.release_date
            .as_ref()
            .and_then(|d| d.get(0..4))
            .and_then(|y| y.parse().ok())
    }
}

/// One episode of a season.
#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    pub season: u32,
    pub number: u32,
    pub name: String,
    pub runtime_mins: Option<u32>,
    pub air_date: Option<String>,
    /// Episode synopsis. Several sentences of distinctive text — the strongest
    /// reference we have to compare ripped dialogue against, since an episode title
    /// alone is only a few words.
    pub overview: Option<String>,
}

impl Episode {
    /// Text describing this episode specifically, for dialogue comparison.
    ///
    /// Title and synopsis together: the title carries proper nouns, the synopsis carries
    /// enough vocabulary for the overlap score to mean something.
    pub fn reference_text(&self) -> String {
        match &self.overview {
            Some(o) => format!("{} {}", self.name, o),
            None => self.name.clone(),
        }
    }
}

/// A season's episode list.
#[derive(Debug, Clone)]
pub struct Season {
    pub number: u32,
    pub episodes: Vec<Episode>,
}

pub struct TmdbClient {
    auth: TmdbAuth,
    credential: String,
}

/// Written by hand rather than derived: a derived `Debug` would print the credential,
/// and debug output has a habit of ending up in logs and bug reports.
impl std::fmt::Debug for TmdbClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TmdbClient")
            .field("auth", &self.auth)
            .field("credential", &"<redacted>")
            .finish()
    }
}

impl TmdbClient {
    /// Build a client from configuration, if a credential is present.
    pub fn from_config(api: &ApiConfig) -> Result<Self> {
        let (auth, credential) = api.tmdb_auth().ok_or(TmdbError::NoCredential)?;
        Ok(Self {
            auth,
            credential: credential.to_string(),
        })
    }

    /// Build the request URL and headers for an endpoint.
    ///
    /// With a v4 token the credential goes in a header. With a v3 key it has to go in
    /// the query string — TMDB offers no alternative for that credential type.
    fn request(&self, path: &str, query: &str) -> (String, Vec<(&'static str, String)>) {
        let mut headers = vec![("Accept", "application/json".to_string())];
        let url = match self.auth {
            TmdbAuth::BearerToken => {
                headers.push(("Authorization", format!("Bearer {}", self.credential)));
                format!("{API_BASE}{path}?{query}")
            }
            TmdbAuth::QueryKey => format!(
                "{API_BASE}{path}?{query}&api_key={}",
                http::urlencode(&self.credential)
            ),
        };
        (url, headers)
    }

    fn get_json(&self, path: &str, query: &str) -> Result<serde_json::Value> {
        let (url, headers) = self.request(path, query);
        let body = match http::get(&url, &headers, TIMEOUT_SECS) {
            Ok(b) => b,
            Err(http::HttpError::Status { status: 401, .. }) => {
                return Err(TmdbError::Unauthorized)
            }
            Err(e) => return Err(e.into()),
        };
        serde_json::from_str(&body).map_err(|e| TmdbError::Malformed {
            endpoint: path.to_string(),
            detail: e.to_string(),
        })
    }

    /// Confirm the credential works, without needing a real query.
    pub fn verify(&self) -> Result<()> {
        self.get_json("/configuration", "").map(|_| ())
    }

    pub fn search_tv(&self, query: &str) -> Result<Vec<TvResult>> {
        let v = self.get_json("/search/tv", &format!("query={}", http::urlencode(query)))?;
        Ok(parse_tv_results(&v))
    }

    pub fn search_movie(&self, query: &str) -> Result<Vec<MovieResult>> {
        let v = self.get_json("/search/movie", &format!("query={}", http::urlencode(query)))?;
        Ok(parse_movie_results(&v))
    }

    /// Fetch a season's episode list.
    pub fn season(&self, tv_id: u64, season: u32) -> Result<Season> {
        let v = self.get_json(&format!("/tv/{tv_id}/season/{season}"), "")?;
        Ok(Season {
            number: season,
            episodes: parse_episodes(&v, season),
        })
    }

    /// How many seasons a show has, for iterating them.
    pub fn tv_season_numbers(&self, tv_id: u64) -> Result<Vec<u32>> {
        let v = self.get_json(&format!("/tv/{tv_id}"), "")?;
        Ok(v.get("seasons")
            .and_then(|s| s.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.get("season_number")?.as_u64())
                    .map(|n| n as u32)
                    .collect()
            })
            .unwrap_or_default())
    }
}

// --- Response parsing, split out so it can be tested without network access ---

pub fn parse_tv_results(v: &serde_json::Value) -> Vec<TvResult> {
    v.get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some(TvResult {
                        id: r.get("id")?.as_u64()?,
                        name: r.get("name")?.as_str()?.to_string(),
                        first_air_date: r
                            .get("first_air_date")
                            .and_then(|d| d.as_str())
                            .filter(|d| !d.is_empty())
                            .map(str::to_string),
                        overview: r
                            .get("overview")
                            .and_then(|d| d.as_str())
                            .filter(|d| !d.is_empty())
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_movie_results(v: &serde_json::Value) -> Vec<MovieResult> {
    v.get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some(MovieResult {
                        id: r.get("id")?.as_u64()?,
                        title: r.get("title")?.as_str()?.to_string(),
                        release_date: r
                            .get("release_date")
                            .and_then(|d| d.as_str())
                            .filter(|d| !d.is_empty())
                            .map(str::to_string),
                        runtime_mins: r.get("runtime").and_then(|d| d.as_u64()).map(|d| d as u32),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_episodes(v: &serde_json::Value, season: u32) -> Vec<Episode> {
    v.get("episodes")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    Some(Episode {
                        season: e
                            .get("season_number")
                            .and_then(|s| s.as_u64())
                            .map(|s| s as u32)
                            .unwrap_or(season),
                        number: e.get("episode_number")?.as_u64()? as u32,
                        name: e
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string(),
                        // TMDB reports runtime as null for many episodes, so this is
                        // optional and matching must cope without it.
                        runtime_mins: e.get("runtime").and_then(|r| r.as_u64()).map(|r| r as u32),
                        air_date: e
                            .get("air_date")
                            .and_then(|d| d.as_str())
                            .filter(|d| !d.is_empty())
                            .map(str::to_string),
                        overview: e
                            .get("overview")
                            .and_then(|d| d.as_str())
                            .filter(|d| !d.trim().is_empty())
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_with(cred: &str) -> ApiConfig {
        ApiConfig {
            tmdb_api_key: Some(cred.to_string()),
            ..Default::default()
        }
    }

    /// The whole reason for preferring the v4 token: the secret must not be in the URL.
    #[test]
    fn bearer_token_goes_in_a_header_not_the_url() {
        let c = TmdbClient::from_config(&api_with(
            "eyJhbGciOiJIUzI1NiJ9.eyJhdWQiOiJ4In0.sig",
        ))
        .unwrap();
        let (url, headers) = c.request("/search/tv", "query=x");
        assert!(!url.contains("eyJ"), "token leaked into url: {url}");
        assert!(headers
            .iter()
            .any(|(k, v)| *k == "Authorization" && v.starts_with("Bearer eyJ")));
    }

    #[test]
    fn v3_key_falls_back_to_a_query_parameter() {
        let c = TmdbClient::from_config(&api_with("0123456789abcdef0123456789abcdef")).unwrap();
        let (url, headers) = c.request("/search/tv", "query=x");
        assert!(url.contains("api_key=0123456789abcdef0123456789abcdef"));
        assert!(!headers.iter().any(|(k, _)| *k == "Authorization"));
    }

    #[test]
    fn missing_credential_is_an_error() {
        let err = TmdbClient::from_config(&ApiConfig::default()).unwrap_err();
        assert!(matches!(err, TmdbError::NoCredential));
    }

    /// Debug output must never expose the credential.
    #[test]
    fn debug_redacts_the_credential() {
        let c = TmdbClient::from_config(&api_with("eyJhbGciOi.eyJhdWQi.sig")).unwrap();
        let rendered = format!("{c:?}");
        assert!(!rendered.contains("eyJhbGciOi"), "leaked: {rendered}");
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn parses_tv_search_results() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"results":[
                {"id":1408,"name":"30 for 30","first_air_date":"2009-10-06","overview":"Docs."},
                {"id":99,"name":"Other","first_air_date":""}
            ]}"#,
        )
        .unwrap();
        let r = parse_tv_results(&v);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "30 for 30");
        assert_eq!(r[0].year(), Some(2009));
        // An empty date must become None rather than an empty string.
        assert_eq!(r[1].first_air_date, None);
        assert_eq!(r[1].year(), None);
    }

    #[test]
    fn parses_episode_lists() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"episodes":[
                {"season_number":1,"episode_number":1,"name":"Kings Ransom","runtime":51,"air_date":"2009-10-06"},
                {"season_number":1,"episode_number":2,"name":"The Band That Wouldn't Die","runtime":null}
            ]}"#,
        )
        .unwrap();
        let e = parse_episodes(&v, 1);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].number, 1);
        assert_eq!(e[0].runtime_mins, Some(51));
        // TMDB frequently omits runtime; matching must tolerate it.
        assert_eq!(e[1].runtime_mins, None);
        assert_eq!(e[1].name, "The Band That Wouldn't Die");
    }

    #[test]
    fn parses_movie_results() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"results":[{"id":5,"title":"A Film","release_date":"1999-03-31"}]}"#,
        )
        .unwrap();
        let m = parse_movie_results(&v);
        assert_eq!(m[0].title, "A Film");
        assert_eq!(m[0].year(), Some(1999));
    }

    #[test]
    fn empty_or_missing_results_are_empty_not_an_error() {
        let v: serde_json::Value = serde_json::from_str(r#"{"results":[]}"#).unwrap();
        assert!(parse_tv_results(&v).is_empty());
        let v2: serde_json::Value = serde_json::from_str(r#"{}"#).unwrap();
        assert!(parse_tv_results(&v2).is_empty());
        assert!(parse_episodes(&v2, 1).is_empty());
    }
}
