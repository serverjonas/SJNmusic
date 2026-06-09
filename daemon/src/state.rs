use std::process::Command as SysCommand;
use strsim::jaro_winkler;
use serde::{Serialize, Deserialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Song {
    pub name: String,
    pub path: String,
}

pub struct DaemonState {
    pub queue: Vec<Song>,
    pub current: Option<Song>,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            queue: Vec::new(),
            current: None,
        }
    }

    pub fn play(&mut self, query: &str) {
        if let Some(song) = self.search(query) {
            self.queue.insert(0, song);
        } else {
            eprintln!("Song nicht gefunden: {}", query);
        }
    }

    pub fn add(&mut self, query: &str) {
        if let Some(song) = self.search(query) {
            self.queue.push(song);
        } else {
            eprintln!("Song nicht gefunden: {}", query);
        }
    }

    pub fn delete(&mut self, query: &str) {
        if let Some(song) = self.search(query) {
            let _ = std::fs::remove_file(&song.path);
            self.queue.retain(|s| s.name != song.name);
        } else {
            eprintln!("Song nicht gefunden: {}", query);
        }
    }

    pub fn init(&mut self, name: String) {
        let path = crate::paths::song_path(&name);

        let result = SysCommand::new("yt-dlp")
            .args([
                "-x",
                "--audio-format",
                "mp3",
                "-o",
                &path,
                &format!("ytsearch1:{}", name),
            ])
            .output();

        if result.is_err() {
            eprintln!("INIT failed: yt-dlp not available or download error");
            return;
        }

        self.queue.push(Song {
            name,
            path,
        });
    }

    pub fn search(&self, query: &str) -> Option<Song> {
        use std::fs;
        
        let songs_dir = crate::paths::songs_dir();
        let query_lower = query.to_lowercase();
        
        // Alle .mp3 Dateien aus songs_dir lesen
        let entries = match fs::read_dir(&songs_dir) {
            Ok(entries) => entries,
            Err(_) => return None,
        };

        let mut best_match: Option<(String, f64)> = None;
        const MIN_SCORE: f64 = 0.65; // Threshold - nicht zu aggressiv

        for entry in entries {
            if let Ok(entry) = entry {
                if let Ok(metadata) = entry.metadata() {
                    if metadata.is_file() {
                        if let Some(filename) = entry.file_name().to_str() {
                            // .mp3 entfernen
                            let song_name = filename.strip_suffix(".mp3")
                                .or_else(|| filename.strip_suffix(".webm"))
                                .unwrap_or(filename);
                            
                            let song_name_lower = song_name.to_lowercase();
                            
                            // Exakte Übereinstimmung (case-insensitive)
                            if song_name_lower == query_lower {
                                return Some(Song {
                                    name: song_name.to_string(),
                                    path: entry.path().to_string_lossy().to_string(),
                                });
                            }
                            
                            // Fuzzy match mit Jaro-Winkler
                            let score = jaro_winkler(&song_name_lower, &query_lower);
                            
                            if score > MIN_SCORE {
                                if best_match.is_none() || score > best_match.as_ref().unwrap().1 {
                                    best_match = Some((
                                        entry.path().to_string_lossy().to_string(),
                                        score
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Wenn fuzzy match gefunden, returne es
        if let Some((path, _)) = best_match {
            if let Some(filename) = std::path::Path::new(&path)
                .file_name()
                .and_then(|f| f.to_str()) {
                let song_name = filename.strip_suffix(".mp3")
                    .or_else(|| filename.strip_suffix(".webm"))
                    .unwrap_or(filename);
                return Some(Song {
                    name: song_name.to_string(),
                    path,
                });
            }
        }

        None
    }
}
