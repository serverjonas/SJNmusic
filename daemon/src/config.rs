use std::fs;

use log::{debug, info, warn};
use serde::{Deserialize, Serialize};

use crate::paths::config_path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind host. Use "127.0.0.1" for local-only, "0.0.0.0" to expose on the network.
    pub host: String,
    /// Port to bind the HTTP server on.
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 14567,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log filter: trace, debug, info, warn, error
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

/// Library / filesystem configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryConfig {
    /// Directory where downloaded songs live. Defaults to
    /// `$HOME/.sjn/music/songs`. May be absolute or relative to the config file.
    #[serde(default)]
    pub music_dir: Option<String>,
    /// Maximum number of songs allowed in the playback queue. 0 = unlimited.
    #[serde(default)]
    pub max_queue_size: usize,
    /// Default repeat mode used on daemon start: one of "off", "one", "all".
    #[serde(default)]
    pub default_repeat: String,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            music_dir: None,
            max_queue_size: 0,
            default_repeat: "off".to_string(),
        }
    }
}

/// Controls how fuzzy search behaves and which routes /search and /search/all use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    /// Jaro-Winkler score in [0.0, 1.0] below which fuzzy matches are rejected.
    #[serde(default = "default_fuzzy")]
    pub fuzzy_threshold: f64,
}

fn default_fuzzy() -> f64 {
    0.65
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            fuzzy_threshold: default_fuzzy(),
        }
    }
}

/// Controls how /init's interactive search picker behaves before downloading
/// a song. The full picker lives in the CLI; the daemon just exposes how many
/// candidates to fetch by default and how to invoke `yt-dlp`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadConfig {
    /// Default number of yt-dlp search candidates shown to the user before a
    /// download is started. Users can override per-call via `/search/yt?limit=N`.
    #[serde(default = "default_search_count")]
    pub search_count: usize,
}

fn default_search_count() -> usize {
    3
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            search_count: default_search_count(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub library: LibraryConfig,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub download: DownloadConfig,
    /// When `true`, the daemon requires `Authorization: Bearer <token>`
    /// for any request whose peer is NOT a loopback address. The matching
    /// `auth_token` is auto-generated at startup if it is still empty
    /// (see `daemon::run`), then persisted to disk via
    /// [`Config::save`] so subsequent restarts reuse the same token.
    #[serde(default)]
    pub auth_enabled: bool,
    /// Bearer credential compared against incoming `Authorization` headers
    /// when `auth_enabled = true`. Empty string disables auth entirely
    /// regardless of the `auth_enabled` flag — the daemon treats any
    /// `auth_token == ""` as "no token configured, allow all remote".
    #[serde(default)]
    pub auth_token: String,
}

impl Config {
    /// Persist the current `Config` back to its TOML file. Used by the
    /// daemon only after a new bearer token has been auto-generated at
    /// startup, so the same token is reusable across restarts. Falls
    /// back silently on serialization/write failures — the daemon will
    /// still boot with the in-memory token, it just won't survive a
    /// restart (and will log a warning about that).
    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        let serialized = toml::to_string_pretty(self).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        })?;
        fs::write(&path, serialized)
    }

    /// Load configuration from the TOML file. If the file does not exist, create
    /// it with sensible defaults so the daemon can be configured on first run.
    pub fn load() -> Self {
        let path = config_path();

        if !std::path::Path::new(&path).exists() {
            warn!("Config not found at {path}, creating default config");
            let cfg = Config::default();
            if let Ok(s) = toml::to_string_pretty(&cfg) {
                if let Err(e) = fs::write(&path, s) {
                    warn!("Failed to write default config: {e}");
                } else {
                    info!("Wrote default config to {path}");
                }
            }
            return cfg;
        }

        match fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(cfg) => {
                    debug!("Loaded config from {path}: {:?}", cfg);
                    cfg
                }
                Err(e) => {
                    warn!("Failed to parse config at {path}: {e}. Falling back to defaults.");
                    Config::default()
                }
            },
            Err(e) => {
                warn!("Failed to read config at {path}: {e}. Falling back to defaults.");
                Config::default()
            }
        }
    }
}
