use std::path::PathBuf;
use std::sync::OnceLock;

use crate::config::Config;

/// Default base directory used when `paths::initialize` has not yet been
/// called. This is the same value the daemon used before paths became
/// configurable, so the daemon can still boot even when a config file is
/// missing or malformed.
fn default_base_dir() -> PathBuf {
    let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".sjn/music");
    p
}

fn default_songs_dir() -> PathBuf {
    default_base_dir().join("songs")
}

/// Library root OnceLock. Populated by `initialize(&cfg)`; readers fall back
/// to the default base dir if `initialize` has not yet run.
static BASE: OnceLock<PathBuf> = OnceLock::new();
static SONGS: OnceLock<PathBuf> = OnceLock::new();

/// Resolves paths from `cfg` and creates all required directories. Safe to
/// call multiple times — only the first call wins because OnceLocks can't be
/// overwritten; subsequent calls with a different config are logged but
/// ignored so the daemon can keep its existing on-disk layout.
pub fn initialize(cfg: &Config) {
    let base = match cfg.library.music_dir.as_deref() {
        Some(dir) if !dir.is_empty() => {
            let p = PathBuf::from(dir);
            if p.is_absolute() {
                p
            } else {
                let mut home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
                home.push(p);
                home
            }
        }
        _ => default_base_dir(),
    };

    let songs = base.join("songs");

    let _ = BASE.set(base.clone());
    let _ = SONGS.set(songs.clone());

    if let Err(e) = std::fs::create_dir_all(&base) {
        log::warn!("Failed to create base dir {base:?}: {e}");
    }
    if let Err(e) = std::fs::create_dir_all(&songs) {
        log::warn!("Failed to create songs dir {songs:?}: {e}");
    }
}

/// Library root: configured value if `initialize` ran, otherwise the default.
/// Never panics so callers from `config::load`, `db::open_db`, and
/// `state::init` all work even before initialization.
fn base_dir_inner() -> PathBuf {
    BASE.get().cloned().unwrap_or_else(default_base_dir)
}

fn songs_dir_inner() -> PathBuf {
    SONGS.get().cloned().unwrap_or_else(default_songs_dir)
}

pub fn songs_dir() -> PathBuf {
    songs_dir_inner()
}

pub fn song_path(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();

    let mut p = songs_dir();
    p.push(format!("{safe}.mp3"));
    p.to_string_lossy().to_string()
}

pub fn db_path() -> String {
    let mut p = base_dir_inner();
    p.push("songs.db");
    p.to_string_lossy().to_string()
}

pub fn config_path() -> String {
    let mut p = base_dir_inner();
    p.push("config.toml");
    p.to_string_lossy().to_string()
}

pub fn ensure_dirs() {
    let base = base_dir_inner();
    let _ = std::fs::create_dir_all(&base);
    let songs = base.join("songs");
    let _ = std::fs::create_dir_all(&songs);
}
