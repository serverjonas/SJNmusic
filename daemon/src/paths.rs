use std::path::PathBuf;

pub fn base_dir() -> PathBuf {
    let mut p = dirs::home_dir().unwrap();
    p.push(".sjn/music");
    p
}

pub fn songs_dir() -> PathBuf {
    let mut p = base_dir();
    p.push("songs");
    p
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
    let mut p = base_dir();
    p.push("songs.db");
    p.to_string_lossy().to_string()
}

pub fn config_path() -> String {
    let mut p = base_dir();
    p.push("config.toml");
    p.to_string_lossy().to_string()
}

pub fn ensure_dirs() {
    let _ = std::fs::create_dir_all(base_dir());
    let _ = std::fs::create_dir_all(songs_dir());
}
