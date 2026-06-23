use std::sync::Mutex;

use log::{debug, error, info, warn};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use strsim::jaro_winkler;

use crate::paths::song_path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Song {
    pub id: i64,
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub songs: Vec<Song>,
}

/// The in-memory mirror of the SQLite-backed state. Wrapped in a Mutex by
/// the daemon so HTTP request handlers and the playback loop can share it.
pub struct DaemonState {
    pub conn: Mutex<Connection>,
    /// Songs pending play, in order. The first element is the next to play.
    pub queue: Vec<Song>,
    /// Currently playing song, if any.
    pub current: Option<Song>,
}

const FUZZY_THRESHOLD: f64 = 0.65;

impl DaemonState {
    pub fn new(conn: Connection) -> Self {
        let queue = Self::load_queue_from_db(&conn);
        info!("Restored queue with {} song(s) from DB", queue.len());
        Self {
            conn: Mutex::new(conn),
            queue,
            current: None,
        }
    }

    fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        match self.conn.lock() {
            Ok(g) => g,
            Err(p) => {
                warn!("DB mutex was poisoned, recovering");
                p.into_inner()
            }
        }
    }

    // ------------------------------------------------------------------
    // DB -> memory helpers
    // ------------------------------------------------------------------

    fn load_queue_from_db(conn: &Connection) -> Vec<Song> {
        let mut stmt = match conn.prepare(
            "SELECT s.id, s.name, s.path
             FROM queue q JOIN songs s ON s.id = q.song_id
             ORDER BY q.position ASC",
        ) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to prepare queue load: {e}");
                return Vec::new();
            }
        };

        stmt.query_map([], |row| {
            Ok(Song {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
            })
        })
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    /// Public wrapper around `all_songs`: returns every song in the DB, sorted
    /// by name. Used by `DaemonState::search` (in-memory) and the `/songs`
    /// HTTP route.
    pub fn all_songs(&self) -> Vec<Song> {
        Self::list_songs(&self.lock_conn())
    }

    fn list_songs(conn: &Connection) -> Vec<Song> {
        let mut stmt = match conn.prepare("SELECT id, name, path FROM songs ORDER BY name") {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to list songs: {e}");
                return Vec::new();
            }
        };

        stmt.query_map([], |row| {
            Ok(Song {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
            })
        })
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    fn insert_song(conn: &Connection, name: &str, path: &str) -> rusqlite::Result<i64> {
        conn.execute(
            "INSERT OR IGNORE INTO songs (name, path) VALUES (?1, ?2)",
            params![name, path],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM songs WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    fn queue_append_db(conn: &Connection, song_id: i64) -> rusqlite::Result<()> {
        // Append at the end. Explicit max+1 so ordering survives deletes.
        let next: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(position), 0) + 1 FROM queue",
                [],
                |row| row.get(0),
            )
            .unwrap_or(1);
        conn.execute(
            "INSERT INTO queue (position, song_id) VALUES (?1, ?2)",
            params![next, song_id],
        )?;
        Ok(())
    }

    fn queue_clear_db(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM queue", [])?;
        Ok(())
    }

    fn playlist_append_db(
        conn: &Connection,
        playlist_id: i64,
        song_id: i64,
    ) -> rusqlite::Result<()> {
        let next: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(position), 0) + 1 FROM playlist_songs WHERE playlist_id = ?1",
                params![playlist_id],
                |row| row.get(0),
            )
            .unwrap_or(1);
        conn.execute(
            "INSERT INTO playlist_songs (playlist_id, position, song_id) VALUES (?1, ?2, ?3)",
            params![playlist_id, next, song_id],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Public API used by both HTTP server and playback loop
    // ------------------------------------------------------------------

    /// Search the songs DB by name. Uses an exact (case-insensitive) match
    /// first, then falls back to a Jaro-Winkler fuzzy match.
    pub fn search(&self, query: &str) -> Option<Song> {
        debug!("Searching DB for: {query}");
        let query_lower = query.to_lowercase();
        let songs = self.all_songs();

        // Exact match wins outright.
        if let Some(s) = songs.iter().find(|s| s.name.to_lowercase() == query_lower) {
            debug!("Exact DB match: {} (id={})", s.name, s.id);
            return Some(s.clone());
        }

        // Best fuzzy match above threshold.
        let best = songs
            .iter()
            .map(|s| (s.clone(), jaro_winkler(&s.name.to_lowercase(), &query_lower)))
            .filter(|(_, score)| *score > FUZZY_THRESHOLD)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        if let Some((song, score)) = best {
            debug!("Fuzzy DB match: {} (score={:.3})", song.name, score);
            return Some(song);
        }

        warn!("No DB match for query: {query}");
        None
    }

    /// Insert a song at the front of the queue and start playing it.
    pub fn play(&mut self, query: &str) -> Result<Song, String> {
        let song = self
            .search(query)
            .ok_or_else(|| format!("song not found: {query}"))?;
        self.queue.insert(0, song.clone());
        let conn = self.lock_conn();
        // Reset persisted queue to keep DB and memory consistent.
        Self::queue_clear_db(&conn).map_err(|e| e.to_string())?;
        for s in &self.queue {
            Self::queue_append_db(&conn, s.id).map_err(|e| e.to_string())?;
        }
        info!("Play: {} (id={})", song.name, song.id);
        Ok(song)
    }

    /// Append a song to the end of the queue.
    pub fn add(&mut self, query: &str) -> Result<Song, String> {
        let song = self
            .search(query)
            .ok_or_else(|| format!("song not found: {query}"))?;
        self.queue.push(song.clone());
        Self::queue_append_db(&self.lock_conn(), song.id).map_err(|e| e.to_string())?;
        info!("Added to queue: {} (id={})", song.name, song.id);
        Ok(song)
    }

    /// Delete a song from the DB, filesystem, and any queue entries.
    pub fn delete(&mut self, query: &str) -> Result<Song, String> {
        let song = self
            .search(query)
            .ok_or_else(|| format!("song not found: {query}"))?;

        {
            let conn = self.lock_conn();
            // Foreign keys remove queue & playlist entries automatically.
            conn.execute("DELETE FROM songs WHERE id = ?1", params![song.id])
                .map_err(|e| e.to_string())?;
        }

        if let Err(e) = std::fs::remove_file(&song.path) {
            warn!("Could not delete file {}: {e}", song.path);
        }
        self.queue.retain(|s| s.id != song.id);
        if self
            .current
            .as_ref()
            .map(|c| c.id == song.id)
            .unwrap_or(false)
        {
            self.current = None;
        }
        info!("Deleted song {} (id={})", song.name, song.id);
        Ok(song)
    }

    /// Download a song via yt-dlp and register it in the DB.
    pub fn init(&self, name: String) -> Result<Song, String> {
        let path = song_path(&name);
        debug!("yt-dlp target path: {path}");

        let output = std::process::Command::new("yt-dlp")
            .args([
                "-x",
                "--audio-format",
                "mp3",
                "-o",
                &path,
                &format!("ytsearch1:{name}"),
            ])
            .output();

        match output {
            Ok(out) => {
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    error!("yt-dlp failed for {name}: {stderr}");
                    return Err(format!("yt-dlp failed: {stderr}"));
                }
                info!("yt-dlp finished for {name}");
            }
            Err(e) => {
                error!("yt-dlp not available: {e}");
                return Err(format!("yt-dlp unavailable: {e}"));
            }
        }

        if !std::path::Path::new(&path).exists() {
            return Err(format!("download finished but file missing: {path}"));
        }

        let conn = self.lock_conn();
        let id = Self::insert_song(&conn, &name, &path).map_err(|e| e.to_string())?;
        info!("Registered song {name} (id={id}) at {path}");
        Ok(Song {
            id,
            name,
            path,
        })
    }

    /// Clear the queue (memory + DB).
    pub fn clear_queue(&mut self) -> Result<(), String> {
        self.queue.clear();
        Self::queue_clear_db(&self.lock_conn()).map_err(|e| e.to_string())?;
        info!("Queue cleared");
        Ok(())
    }

    /// Pop the song at the head of the queue as having been played. Removes
    /// the same row from the in-memory queue and the `queue` table so that
    /// a daemon restart does not put already-played songs back into rotation.
    /// The DB is updated first — if the DELETE fails the in-memory queue
    /// is left untouched and the error is returned to the caller, keeping
    /// memory and DB consistent.
    pub fn pop_played(&mut self) -> Result<Option<Song>, String> {
        if self.queue.is_empty() {
            return Ok(None);
        }
        let id = self.queue[0].id;
        // Scope the DB guard so it is dropped before mutating `self.queue`.
        let result = {
            let conn = self.lock_conn();
            conn.execute(
                "DELETE FROM queue WHERE song_id = ?1 ORDER BY position ASC LIMIT 1",
                params![id],
            )
        };
        result.map_err(|e| format!("failed to delete queue row from DB: {e}"))?;
        Ok(Some(self.queue.remove(0)))
    }

    /// Skip the song currently at the front of the queue. Thin wrapper around
    /// `pop_played` so the queue-pop SQL lives in exactly one place.
    pub fn skip(&mut self) -> Result<Option<Song>, String> {
        if let Some(skipped) = self.pop_played()? {
            info!("Skipped {}", skipped.name);
            Ok(Some(skipped))
        } else {
            Ok(None)
        }
    }

    // ------------------------------------------------------------------
    // Playlists
    // ------------------------------------------------------------------

    pub fn list_playlists(&self) -> Vec<Playlist> {
        let conn = self.lock_conn();
        let mut stmt = match conn.prepare("SELECT id, name FROM playlists ORDER BY name") {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to list playlists: {e}");
                return Vec::new();
            }
        };

        let ids: Vec<(i64, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        drop(stmt);

        ids.into_iter()
            .map(|(id, name)| {
                let songs = Self::load_playlist_songs(&conn, id);
                Playlist { id, name, songs }
            })
            .collect()
    }

    fn load_playlist_songs(conn: &Connection, playlist_id: i64) -> Vec<Song> {
        let mut stmt = match conn.prepare(
            "SELECT s.id, s.name, s.path
             FROM playlist_songs ps JOIN songs s ON s.id = ps.song_id
             WHERE ps.playlist_id = ?1
             ORDER BY ps.position ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(params![playlist_id], |row| {
            Ok(Song {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
            })
        })
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn create_playlist(&self, name: &str) -> Result<Playlist, String> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT OR IGNORE INTO playlists (name) VALUES (?1)",
            params![name],
        )
        .map_err(|e| e.to_string())?;
        let id: i64 = conn
            .query_row(
                "SELECT id FROM playlists WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        info!("Created playlist {name} (id={id})");
        Ok(Playlist {
            id,
            name: name.to_string(),
            songs: Vec::new(),
        })
    }

    pub fn delete_playlist(&self, name: &str) -> Result<(), String> {
        let conn = self.lock_conn();
        let affected = conn
            .execute("DELETE FROM playlists WHERE name = ?1", params![name])
            .map_err(|e| e.to_string())?;
        if affected == 0 {
            return Err(format!("playlist not found: {name}"));
        }
        info!("Deleted playlist {name}");
        Ok(())
    }

    pub fn add_to_playlist(
        &self,
        playlist: &str,
        query: &str,
    ) -> Result<(Playlist, Song), String> {
        let song = self
            .search(query)
            .ok_or_else(|| format!("song not found: {query}"))?;
        let conn = self.lock_conn();
        let playlist_id: i64 = conn
            .query_row(
                "SELECT id FROM playlists WHERE name = ?1",
                params![playlist],
                |row| row.get(0),
            )
            .map_err(|_| format!("playlist not found: {playlist}"))?;
        Self::playlist_append_db(&conn, playlist_id, song.id).map_err(|e| e.to_string())?;
        info!("Added {} to playlist {}", song.name, playlist);
        let pl = Playlist {
            id: playlist_id,
            name: playlist.to_string(),
            songs: Self::load_playlist_songs(&conn, playlist_id),
        };
        Ok((pl, song))
    }

    /// Load a playlist into the active queue (replacing the current queue).
    pub fn play_playlist(&mut self, name: &str) -> Result<Playlist, String> {
        let (playlist_id, songs) = {
            let conn = self.lock_conn();
            let playlist_id: i64 = conn
                .query_row(
                    "SELECT id FROM playlists WHERE name = ?1",
                    params![name],
                    |row| row.get(0),
                )
                .map_err(|_| format!("playlist not found: {name}"))?;
            let songs = Self::load_playlist_songs(&conn, playlist_id);
            Self::queue_clear_db(&conn).map_err(|e| e.to_string())?;
            for s in &songs {
                Self::queue_append_db(&conn, s.id).map_err(|e| e.to_string())?;
            }
            (playlist_id, songs)
        };
        // Guard is dropped; safe to mutate self.queue without borrow conflict.
        self.queue = songs.clone();
        info!("Playing playlist {name} ({} songs)", songs.len());
        Ok(Playlist {
            id: playlist_id,
            name: name.to_string(),
            songs,
        })
    }

    pub fn get_playlist(&self, name: &str) -> Result<Playlist, String> {
        let conn = self.lock_conn();
        let playlist_id: i64 = conn
            .query_row(
                "SELECT id FROM playlists WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .map_err(|_| format!("playlist not found: {name}"))?;
        Ok(Playlist {
            id: playlist_id,
            name: name.to_string(),
            songs: Self::load_playlist_songs(&conn, playlist_id),
        })
    }
}
