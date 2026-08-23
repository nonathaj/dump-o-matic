//! Configuration loading and validation.
//!
//! Two rules shape this module:
//!
//! 1. **No storage path is ever defaulted.** If staging or a destination is not
//!    configured, the tool refuses to run rather than inventing a location. Guessing
//!    where to put someone's media is exactly the kind of surprise this project exists
//!    to avoid.
//! 2. **No credential is ever bundled or committed.** Secrets come from the environment,
//!    or from a config file the user owns. They are redacted everywhere they are printed.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Environment variable naming an explicit config file.
pub const ENV_CONFIG: &str = "DUMO_CONFIG";
/// Environment override for the staging root.
pub const ENV_STAGING_ROOT: &str = "DUMO_STAGING_ROOT";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no config file found (looked at {searched}); write one or set {ENV_CONFIG}")]
    NotFound { searched: String },

    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// Where ripped content is written before it is identified and migrated.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StagingConfig {
    /// Root of the staging area. Required — never defaulted.
    pub root: PathBuf,

    /// Refuse to begin a stage unless at least this much space would remain free
    /// afterwards. Guards against filling the disk that also holds your library.
    #[serde(default = "default_headroom_gb")]
    pub min_free_headroom_gb: u64,

    /// Whether to reclaim staged files as soon as a migration is verified, or keep them
    /// until space is needed. Both are safe; this is a space/convenience trade-off.
    #[serde(default)]
    pub reclaim_after_migrate: bool,
}

fn default_headroom_gb() -> u64 {
    20
}

impl StagingConfig {
    /// Directory holding all in-flight jobs.
    pub fn jobs_dir(&self) -> PathBuf {
        self.root.join("jobs")
    }

    /// Working directory for a single job.
    pub fn job_dir(&self, job_id: &str) -> PathBuf {
        self.jobs_dir().join(job_id)
    }

    /// Raw, unmodified backend output for a job — the bits straight off the disc.
    pub fn job_raw_dir(&self, job_id: &str) -> PathBuf {
        self.job_dir(job_id).join("raw")
    }

    /// Content that has been identified and renamed, awaiting migration.
    pub fn ready_dir(&self) -> PathBuf {
        self.root.join("ready")
    }

    /// Ready-tree subdirectory for a media category, e.g. `ready/games`.
    ///
    /// The category is the routing key that decides which destination a file goes to,
    /// so it is part of the staging path rather than inferred later.
    pub fn ready_category_dir(&self, category: &str) -> PathBuf {
        self.ready_dir().join(category)
    }

    /// Per-job logs from ripping backends, kept for troubleshooting.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
}

/// A permanent storage destination.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DestinationConfig {
    /// Short name used to refer to this destination, e.g. `nas`.
    pub name: String,

    /// Filesystem path to the destination.
    ///
    /// For network shares this is the **OS mount point**, not a `smb://` URL:
    /// dump-o-matic does not implement an SMB client, so the share must be mounted by
    /// the system (cifs/mount.smb3, autofs, or a systemd mount unit). That keeps share
    /// credentials in the OS mount config rather than in this tool's secret surface.
    pub root: PathBuf,

    /// True if this destination is a network share, which enables the stricter
    /// verification and interruption handling that network targets require.
    #[serde(default)]
    pub network: bool,

    /// Media categories this destination accepts, e.g. `["games"]`.
    ///
    /// Categories correspond to the top level of the staging `ready/` tree, so
    /// `ready/games/ps2/Title.iso` is routed to whichever destination accepts `games`.
    /// An empty list accepts everything, which is the right default for a single
    /// destination and wrong the moment there are several.
    #[serde(default)]
    pub media: Vec<String>,
}

impl DestinationConfig {
    /// Whether this destination should receive content of the given category.
    pub fn accepts(&self, category: &str) -> bool {
        self.media.is_empty() || self.media.iter().any(|m| m == category)
    }
}

/// How a TMDB credential authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmdbAuth {
    /// v4 Read Access Token, sent as an `Authorization: Bearer` header. Preferred:
    /// a header keeps the secret out of URLs, and therefore out of logs, shell
    /// history, proxies and error messages.
    BearerToken,
    /// v3 API key, sent as an `api_key` query parameter.
    QueryKey,
}

/// Credentials for external metadata services.
///
/// Every field is optional. A missing credential disables that lookup source and is
/// reported as unavailable — it never fails the run outright.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ApiConfig {
    /// TMDB credential: either a v4 Read Access Token (preferred) or a v3 API key.
    /// Which one it is is detected from its shape, so either can be pasted here.
    #[serde(default)]
    pub tmdb_api_key: Option<String>,
    #[serde(default)]
    pub tvdb_api_key: Option<String>,
    #[serde(default)]
    pub igdb_client_id: Option<String>,
    #[serde(default)]
    pub igdb_client_secret: Option<String>,
}

impl ApiConfig {
    /// Overlay credentials found in the environment. Environment wins over file, so a
    /// key can be supplied without ever being written to disk.
    fn apply_env(&mut self) {
        fn env(name: &str) -> Option<String> {
            std::env::var(name).ok().filter(|s| !s.trim().is_empty())
        }
        // Either name works, so the variable can be called after whichever credential
        // the user copied from TMDB's settings page.
        if let Some(v) = env("DUMO_TMDB_TOKEN").or_else(|| env("DUMO_TMDB_API_KEY")) {
            self.tmdb_api_key = Some(v);
        }
        if let Some(v) = env("DUMO_TVDB_API_KEY") {
            self.tvdb_api_key = Some(v);
        }
        if let Some(v) = env("DUMO_IGDB_CLIENT_ID") {
            self.igdb_client_id = Some(v);
        }
        if let Some(v) = env("DUMO_IGDB_CLIENT_SECRET") {
            self.igdb_client_secret = Some(v);
        }
    }

    /// Work out how a TMDB credential should be sent.
    ///
    /// A v4 Read Access Token is a JWT: three dot-separated base64 segments beginning
    /// `eyJ`. A v3 API key is 32 hexadecimal characters. Detecting the shape means a
    /// user can paste either without having to know which field it belongs in — TMDB's
    /// own settings page offers both, adjacent, with similar names.
    pub fn tmdb_auth(&self) -> Option<(TmdbAuth, &str)> {
        let raw = self.tmdb_api_key.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        if raw.starts_with("eyJ") && raw.matches('.').count() == 2 {
            Some((TmdbAuth::BearerToken, raw))
        } else {
            Some((TmdbAuth::QueryKey, raw))
        }
    }

    /// Names of the services that have credentials, for display. Never the values.
    pub fn configured_services(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.tmdb_api_key.is_some() {
            v.push("tmdb");
        }
        if self.tvdb_api_key.is_some() {
            v.push("tvdb");
        }
        if self.igdb_client_id.is_some() && self.igdb_client_secret.is_some() {
            v.push("igdb");
        }
        v
    }
}

/// Locations of identification databases.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DatfileConfig {
    /// Directory of Redump `.dat` files, supplied by the user.
    #[serde(default)]
    pub redump_dir: Option<PathBuf>,
    /// Directory of No-Intro `.dat` files, supplied by the user.
    #[serde(default)]
    pub nointro_dir: Option<PathBuf>,
}

/// How identified game discs are packaged for the emulator library.
///
/// The archival form of a dump — a Redump-exact `.iso`, or a `.cue` plus per-track
/// `.bin` — is not always the form a front-end wants. Which transformation is right
/// depends on the platform, so this is configuration rather than a rule baked into the
/// code.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GamesConfig {
    /// Platform slugs whose disc images are packed into a single CHD.
    ///
    /// Per-platform rather than a blanket on/off, because the right container genuinely
    /// differs by system. CHD is the right answer for the disc consoles listed below: it
    /// is one file, read natively by PCSX2, DuckStation and the RetroArch disc cores, and
    /// losslessly reversible. It is the *wrong* answer for GameCube and Wii, where
    /// Dolphin's RVZ is format-aware and compresses considerably better — which is why
    /// `gc` and `wii` are deliberately absent.
    ///
    /// Both CD and DVD media are packed for a listed platform. That is a measured
    /// decision, not an assumption: on a PS2 DVD (Lord of the Rings, 4.1 GB) CHD saved
    /// 28.6%, against 27% on a PS2 CD — the same ratio, and far more in absolute terms.
    #[serde(default = "default_chd_platforms")]
    pub chd_platforms: Vec<String>,
}

fn default_chd_platforms() -> Vec<String> {
    [
        "psx",
        "ps2",
        "segacd",
        "saturn",
        "dreamcast",
        "3do",
        "pcenginecd",
        "neogeocd",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for GamesConfig {
    fn default() -> Self {
        Self {
            chd_platforms: default_chd_platforms(),
        }
    }
}

impl GamesConfig {
    /// Whether images for this ES-DE platform slug should be packed into a CHD.
    pub fn packs_chd(&self, slug: &str) -> bool {
        self.chd_platforms.iter().any(|p| p == slug)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DrivesConfig {
    /// Explicit device list. Empty means autodetect.
    #[serde(default)]
    pub devices: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub staging: StagingConfig,

    /// Permanent destinations. May be empty while only staging is in use.
    #[serde(default, rename = "destination")]
    pub destinations: Vec<DestinationConfig>,

    #[serde(default)]
    pub drives: DrivesConfig,

    #[serde(default)]
    pub datfiles: DatfileConfig,

    #[serde(default)]
    pub games: GamesConfig,

    #[serde(default)]
    pub api: ApiConfig,

    /// Path this config was loaded from. Not part of the file itself.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

impl Config {
    /// Candidate config locations, in priority order.
    pub fn search_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Ok(p) = std::env::var(ENV_CONFIG) {
            if !p.trim().is_empty() {
                paths.push(PathBuf::from(p));
            }
        }
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.trim().is_empty() {
                paths.push(PathBuf::from(xdg).join("dump-o-matic/config.toml"));
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            paths.push(PathBuf::from(&home).join(".config/dump-o-matic/config.toml"));
        }
        paths.push(PathBuf::from("dump-o-matic.toml"));
        paths
    }

    /// Load configuration, applying environment overrides.
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let candidates: Vec<PathBuf> = match explicit {
            Some(p) => vec![p.to_path_buf()],
            None => Self::search_paths(),
        };

        let found = candidates.iter().find(|p| p.is_file());
        let Some(path) = found else {
            return Err(ConfigError::NotFound {
                searched: candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        };

        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Read {
            path: path.clone(),
            source: e,
        })?;
        let mut cfg: Config = toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.clone(),
            source: e,
        })?;

        cfg.source_path = Some(path.clone());
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(root) = std::env::var(ENV_STAGING_ROOT) {
            if !root.trim().is_empty() {
                self.staging.root = PathBuf::from(root);
            }
        }
        self.api.apply_env();
    }

    /// Serialize with every secret replaced, for safe display and logging.
    pub fn redacted(&self) -> Config {
        let mut c = self.clone();
        fn redact(v: &mut Option<String>) {
            if v.is_some() {
                *v = Some("<set>".to_string());
            }
        }
        redact(&mut c.api.tmdb_api_key);
        redact(&mut c.api.tvdb_api_key);
        redact(&mut c.api.igdb_client_id);
        redact(&mut c.api.igdb_client_secret);
        c
    }

    pub fn find_destination(&self, name: &str) -> Option<&DestinationConfig> {
        self.destinations.iter().find(|d| d.name == name)
    }
}

/// Severity of a configuration finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckLevel {
    Ok,
    Warning,
    Error,
}

/// One result from validating the configuration.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub level: CheckLevel,
    pub subject: String,
    pub detail: String,
}

/// Free and total bytes on the filesystem containing `path`.
///
/// Returns `None` when the filesystem cannot report usage — which network shares
/// sometimes do. That is deliberately distinct from reporting zero, because "unknown"
/// must never be mistaken for "full" or "empty".
pub fn filesystem_free_bytes(path: &Path) -> Option<(u64, u64)> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut st) };
    if rc != 0 {
        return None;
    }
    let block = if st.f_frsize > 0 {
        st.f_frsize as u64
    } else {
        st.f_bsize as u64
    };
    if block == 0 {
        return None;
    }
    let total = (st.f_blocks as u64).checked_mul(block)?;
    let free = (st.f_bavail as u64).checked_mul(block)?;
    if total == 0 {
        return None;
    }
    Some((free, total))
}

/// Check whether a directory is writable, without leaving anything behind.
fn is_writable(path: &Path) -> bool {
    let probe = path.join(".dumo-write-test");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Validate a configuration against the actual filesystem.
///
/// This is what `config check` reports. It never modifies anything except a transient
/// write-probe file, which is removed immediately.
pub fn check(cfg: &Config) -> Vec<CheckResult> {
    let mut out = Vec::new();
    let root = &cfg.staging.root;

    if !root.exists() {
        out.push(CheckResult {
            level: CheckLevel::Error,
            subject: "staging.root".into(),
            detail: format!("{} does not exist", root.display()),
        });
    } else if !root.is_dir() {
        out.push(CheckResult {
            level: CheckLevel::Error,
            subject: "staging.root".into(),
            detail: format!("{} is not a directory", root.display()),
        });
    } else if !is_writable(root) {
        out.push(CheckResult {
            level: CheckLevel::Error,
            subject: "staging.root".into(),
            detail: format!("{} is not writable", root.display()),
        });
    } else {
        let space = match filesystem_free_bytes(root) {
            Some((free, total)) => {
                let free_gb = free / 1_000_000_000;
                let headroom = cfg.staging.min_free_headroom_gb;
                let level = if free_gb <= headroom {
                    CheckLevel::Warning
                } else {
                    CheckLevel::Ok
                };
                out.push(CheckResult {
                    level,
                    subject: "staging.space".into(),
                    detail: format!(
                        "{} GB free of {} GB (headroom {} GB){}",
                        free_gb,
                        total / 1_000_000_000,
                        headroom,
                        if level == CheckLevel::Warning {
                            " — at or below headroom, rips will be refused"
                        } else {
                            ""
                        }
                    ),
                });
                true
            }
            None => {
                out.push(CheckResult {
                    level: CheckLevel::Warning,
                    subject: "staging.space".into(),
                    detail: "filesystem does not report free space; pre-flight checks \
                             cannot guarantee a rip will fit"
                        .into(),
                });
                false
            }
        };
        let _ = space;
        out.push(CheckResult {
            level: CheckLevel::Ok,
            subject: "staging.root".into(),
            detail: format!("{} exists and is writable", root.display()),
        });
    }

    if cfg.destinations.is_empty() {
        out.push(CheckResult {
            level: CheckLevel::Warning,
            subject: "destination".into(),
            detail: "no permanent destination configured; migration is unavailable".into(),
        });
    }

    for d in &cfg.destinations {
        if !d.root.exists() {
            out.push(CheckResult {
                level: CheckLevel::Warning,
                subject: format!("destination.{}", d.name),
                detail: format!(
                    "{} is not present{}",
                    d.root.display(),
                    if d.network {
                        " (share not mounted? staged content will simply wait)"
                    } else {
                        ""
                    }
                ),
            });
            continue;
        }
        match filesystem_free_bytes(&d.root) {
            Some((free, total)) => out.push(CheckResult {
                level: CheckLevel::Ok,
                subject: format!("destination.{}", d.name),
                detail: format!(
                    "{} available, {} GB free of {} GB",
                    d.root.display(),
                    free / 1_000_000_000,
                    total / 1_000_000_000
                ),
            }),
            None => out.push(CheckResult {
                level: CheckLevel::Warning,
                subject: format!("destination.{}", d.name),
                detail: format!(
                    "{} present but reports no free space information",
                    d.root.display()
                ),
            }),
        }
    }

    for (label, dir) in [
        ("datfiles.redump_dir", &cfg.datfiles.redump_dir),
        ("datfiles.nointro_dir", &cfg.datfiles.nointro_dir),
    ] {
        if let Some(p) = dir {
            let level = if p.is_dir() {
                CheckLevel::Ok
            } else {
                CheckLevel::Warning
            };
            out.push(CheckResult {
                level,
                subject: label.into(),
                detail: format!(
                    "{}{}",
                    p.display(),
                    if level == CheckLevel::Ok {
                        ""
                    } else {
                        " does not exist; game identification will be unavailable"
                    }
                ),
            });
        }
    }

    let services = cfg.api.configured_services();
    out.push(CheckResult {
        level: CheckLevel::Ok,
        subject: "api".into(),
        detail: if services.is_empty() {
            "no credentials configured; online lookups unavailable".into()
        } else {
            format!("credentials present for: {}", services.join(", "))
        },
    });

    // Say which TMDB credential style was detected, so a mis-paste is visible before a
    // request fails with an opaque 401.
    if let Some((auth, _)) = cfg.api.tmdb_auth() {
        out.push(CheckResult {
            level: CheckLevel::Ok,
            subject: "api.tmdb".into(),
            detail: match auth {
                TmdbAuth::BearerToken => {
                    "v4 Read Access Token detected; sent as an Authorization header".into()
                }
                TmdbAuth::QueryKey => {
                    "v3 API key detected; sent as a query parameter. A v4 Read Access \
                     Token is preferred, since it keeps the secret out of URLs"
                        .to_string()
                }
            },
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        toml::from_str(
            r#"
            [staging]
            root = "/tmp/dumo-staging"

            [[destination]]
            name = "nas"
            root = "/mnt/nas"
            network = true
            "#,
        )
        .unwrap()
    }

    #[test]
    fn parses_minimal_config() {
        let c = sample();
        assert_eq!(c.staging.root, PathBuf::from("/tmp/dumo-staging"));
        assert_eq!(c.staging.min_free_headroom_gb, 20);
        assert_eq!(c.destinations.len(), 1);
        assert!(c.destinations[0].network);
    }

    #[test]
    fn staging_root_is_required() {
        let err = toml::from_str::<Config>("[staging]\n").unwrap_err();
        assert!(err.to_string().contains("root"), "got: {err}");
    }

    #[test]
    fn staging_layout_is_under_root() {
        let c = sample();
        assert!(c.staging.job_raw_dir("job1").starts_with(&c.staging.root));
        assert!(c.staging.ready_dir().starts_with(&c.staging.root));
        assert_eq!(
            c.staging.job_raw_dir("job1"),
            PathBuf::from("/tmp/dumo-staging/jobs/job1/raw")
        );
    }

    #[test]
    fn destination_accepts_by_category() {
        let mut d = DestinationConfig {
            name: "nas-games".into(),
            root: PathBuf::from("/mnt/nas/emulation/roms"),
            network: true,
            media: vec!["games".into()],
        };
        assert!(d.accepts("games"));
        assert!(!d.accepts("movies"));
        // An empty list is a catch-all, for the single-destination case.
        d.media.clear();
        assert!(d.accepts("movies"));
    }

    #[test]
    fn ready_category_dir_is_under_ready() {
        let c = sample();
        assert_eq!(
            c.staging.ready_category_dir("games"),
            PathBuf::from("/tmp/dumo-staging/ready/games")
        );
    }

    #[test]
    fn secrets_are_redacted_not_echoed() {
        let mut c = sample();
        c.api.tmdb_api_key = Some("super-secret-value".into());
        let r = c.redacted();
        let rendered = toml::to_string(&r).unwrap();
        assert!(!rendered.contains("super-secret-value"));
        assert!(rendered.contains("<set>"));
        // The real config is untouched.
        assert_eq!(c.api.tmdb_api_key.as_deref(), Some("super-secret-value"));
    }

    #[test]
    fn detects_a_v4_read_access_token() {
        let mut c = sample();
        // Shape of a real TMDB v4 token: three dot-separated base64 segments.
        c.api.tmdb_api_key = Some("eyJhbGciOiJIUzI1NiJ9.eyJhdWQiOiJhYmMifQ.c2lnbmF0dXJl".into());
        assert_eq!(c.api.tmdb_auth().unwrap().0, TmdbAuth::BearerToken);
    }

    #[test]
    fn detects_a_v3_api_key() {
        let mut c = sample();
        c.api.tmdb_api_key = Some("0123456789abcdef0123456789abcdef".into());
        assert_eq!(c.api.tmdb_auth().unwrap().0, TmdbAuth::QueryKey);
    }

    #[test]
    fn blank_or_missing_tmdb_credential_is_none() {
        let mut c = sample();
        assert!(c.api.tmdb_auth().is_none());
        c.api.tmdb_api_key = Some("   ".into());
        assert!(c.api.tmdb_auth().is_none());
    }

    #[test]
    fn tmdb_credential_is_trimmed() {
        let mut c = sample();
        c.api.tmdb_api_key = Some("  0123456789abcdef0123456789abcdef \n".into());
        assert_eq!(c.api.tmdb_auth().unwrap().1, "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn configured_services_lists_names_only() {
        let mut c = sample();
        c.api.tmdb_api_key = Some("k".into());
        assert_eq!(c.api.configured_services(), vec!["tmdb"]);
        // IGDB needs both halves before it counts as usable.
        c.api.igdb_client_id = Some("id".into());
        assert_eq!(c.api.configured_services(), vec!["tmdb"]);
        c.api.igdb_client_secret = Some("secret".into());
        assert_eq!(c.api.configured_services(), vec!["tmdb", "igdb"]);
    }

    #[test]
    fn check_reports_missing_staging_root_as_error() {
        let mut c = sample();
        c.staging.root = PathBuf::from("/nonexistent/dumo/staging");
        let results = check(&c);
        assert!(results
            .iter()
            .any(|r| r.level == CheckLevel::Error && r.subject == "staging.root"));
    }

    #[test]
    fn free_space_reports_for_a_real_path() {
        let (free, total) = filesystem_free_bytes(Path::new("/")).expect("root reports usage");
        assert!(total > 0);
        assert!(free <= total);
    }

    /// The platform list is a set of deliberate decisions, not a convenience default.
    #[test]
    fn chd_applies_to_disc_consoles_but_not_gamecube_or_wii() {
        let g = GamesConfig::default();
        for slug in ["psx", "ps2", "segacd", "saturn", "dreamcast"] {
            assert!(g.packs_chd(slug), "{slug} should be packed as CHD");
        }
        // Dolphin's RVZ is format-aware and beats CHD substantially on these, so packing
        // them as CHD would be a downgrade dressed up as consistency.
        for slug in ["gc", "wii"] {
            assert!(!g.packs_chd(slug), "{slug} must not be packed as CHD");
        }
        // Nothing is packed for a platform nobody listed.
        assert!(!g.packs_chd("switch"));
        assert!(!g.packs_chd(""));
    }

    /// An explicitly empty list must mean "pack nothing", not "fall back to defaults" —
    /// otherwise there is no way to turn the feature off.
    #[test]
    fn an_empty_platform_list_disables_packing() {
        let g: GamesConfig = toml::from_str("chd_platforms = []").unwrap();
        assert!(!g.packs_chd("ps2"));
    }

    #[test]
    fn omitting_the_key_keeps_the_defaults() {
        let g: GamesConfig = toml::from_str("").unwrap();
        assert!(g.packs_chd("ps2"));
    }
}
