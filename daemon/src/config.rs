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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl Config {
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
