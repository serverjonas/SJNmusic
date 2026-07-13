use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{debug, error, info, warn};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use strsim::jaro_winkler;

use crate::audio::{probe_duration_secs, AudioHandle};
use crate::paths::song_path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Song {
    pub id: i64,
    pub name: String,
    pub path: String,
    /// Optional playback duration in seconds (filled in on download probe or
    /// before first play). `None` until the daemon has had a chance to look
    /// it up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
}

/// One row of the result set returned by `/search/yt`. Built directly from
/// `yt-dlp -j ytsearchN:QUERY` JSON lines, so field names match what the
/// GUI and the CLI consume. The GUI uses `thumbnail` to render cover art in
/// the picker; the CLI ignores unknown fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct YtCandidate {
    pub id: String,
    pub title: String,
    /// YouTube channel / uploader / artist — whichever yt-dlp fills in. The
    /// CLI displays this verbatim; the daemon never tries to canonicalise it.
    pub uploader: String,
    /// yt-dlp's reported duration in seconds. `0.0` when unknown (livestreams,
    /// shorts, etc.) so the CLI formatter still produces a sensible string.
    pub duration_secs: f64,
    pub url: String,
    /// yt-dlp's reported thumbnail URL (highest-resolution pick if `thumbnails`
    /// is a list of sizes). `None` when yt-dlp didn't include any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub songs: Vec<Song>,
}

/// Repeat behaviour: `Off` is the default; `One` re-queues the same track when
/// it ends; `All` reloads the latest snapshot of songs once the queue empties.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    Off,
    One,
    All,
}

impl Default for RepeatMode {
    fn default() -> Self {
        RepeatMode::Off
    }
}

impl RepeatMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "off" | "0" | "false" => Some(RepeatMode::Off),
            "one" | "1" | "single" => Some(RepeatMode::One),
            "all" | "loop" => Some(RepeatMode::All),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            RepeatMode::Off => "off",
            RepeatMode::One => "one",
            RepeatMode::All => "all",
        }
    }
}

/// One in-flight or completed yt-dlp job spawned by /init or /init/batch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadJob {
    pub id: i64,
    pub name: String,
    pub status: String, // "queued" | "running" | "done" | "failed"
    pub song_id: Option<i64>,
    pub error: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// Source URL or `ytsearch1:NAME` string the worker will pass to yt-dlp.
    /// Useful for debugging and for `/downloads` output once a picker is
    /// involved. `None` for legacy `/init` callers that pass only a name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// The in-memory mirror of the SQLite-backed state. Wrapped in a Mutex by
/// the daemon so HTTP request handlers and the playback loop can share it.
pub struct DaemonState {
    pub conn: Mutex<Connection>,
    /// Songs pending play, in order. The first element is the next to play.
    pub queue: Vec<Song>,
    /// Currently playing song, if any.
    pub current: Option<Song>,
    /// `Send + Sync` handle to the audio thread (which owns rodio's
    /// `OutputStream`, a `!Send` platform type). We never touch rodio
    /// directly from HTTP or the playback loop — we enqueue commands.
    pub audio_handle: Arc<AudioHandle>,
    /// User-selected repeat mode.
    pub repeat_mode: RepeatMode,
    /// Snapshot taken when entering repeat-all mode. When the queue drains in
    /// this mode we restore this snapshot so playback loops without the user
    /// having to refill the queue manually.
    pub cycle_snapshot: Vec<Song>,
    /// `true` once the queue row for `current` has been popped at start of
    /// play. Reset to `false` whenever we move on (success, failure, skip,
    /// or natural end). Prevents double-removal of the queue row while the
    /// audio thread transitions from "pending" to "playing".
    pub popped_for_current: bool,
    /// In-flight and recently completed download jobs.
    pub download_jobs: Mutex<HashMap<i64, DownloadJob>>,
    pub next_job_id: AtomicI64,
    /// Fuzzy match threshold; configurable so users can tune strictness.
    pub fuzzy_threshold: f64,
    /// 0 means unlimited.
    pub max_queue_size: usize,
    /// Default number of yt-dlp candidates to return from `/search/yt` when
    /// the caller doesn't specify `?limit=N`. Used by the CLI's interactive
    /// picker.
    pub search_count: usize,
}

impl DaemonState {
    pub fn new(
        conn: Connection,
        fuzzy_threshold: f64,
        max_queue_size: usize,
        repeat_mode: RepeatMode,
        audio_handle: Arc<AudioHandle>,
        search_count: usize,
    ) -> Self {
        let queue = Self::load_queue_from_db(&conn);
        info!("Restored queue with {} song(s) from DB", queue.len());
        Self {
            conn: Mutex::new(conn),
            queue,
            current: None,
            audio_handle,
            repeat_mode,
            cycle_snapshot: Vec::new(),
            popped_for_current: false,
            download_jobs: Mutex::new(HashMap::new()),
            next_job_id: AtomicI64::new(1),
            fuzzy_threshold,
            max_queue_size,
            search_count,
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
            "SELECT s.id, s.name, s.path, s.duration_secs
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
                duration_secs: row.get::<_, Option<f64>>(3).ok().flatten(),
            })
        })
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    /// All songs sorted by name (used internally and by `/songs`).
    pub fn all_songs(&self) -> Vec<Song> {
        Self::list_songs(&self.lock_conn(), None)
    }

    /// Used by `/songs?q=...` to filter without a separate search round-trip.
    pub fn list_songs_filtered(&self, q: Option<&str>) -> Vec<Song> {
        let pattern = q.map(|q| {
            // Escape `%`, `_`, and `\` so users can search for those literally.
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        });
        Self::list_songs(&self.lock_conn(), pattern.as_deref())
    }

    fn list_songs(conn: &Connection, filter: Option<&str>) -> Vec<Song> {
        let (sql, has_filter) = match filter {
            Some(_) => (
                "SELECT id, name, path, duration_secs FROM songs \
                 WHERE name LIKE ? ESCAPE '\\' ORDER BY name",
                true,
            ),
            None => ("SELECT id, name, path, duration_secs FROM songs ORDER BY name", false),
        };
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to list songs: {e}");
                return Vec::new();
            }
        };

        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Song> {
            Ok(Song {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
                duration_secs: row.get::<_, Option<f64>>(3).ok().flatten(),
            })
        };
        let mapped = if has_filter {
            stmt.query_map(params![format!("%{}%", filter.unwrap_or(""))], map_row)
        } else {
            stmt.query_map([], map_row)
        };
        mapped
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

    fn update_duration_secs(
        conn: &Connection,
        song_id: i64,
        duration_secs: f64,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE songs SET duration_secs = ?1 WHERE id = ?2",
            params![duration_secs, song_id],
        )?;
        Ok(())
    }

    fn queue_append_db(conn: &Connection, song_id: i64) -> rusqlite::Result<()> {
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

    fn write_history(
        conn: &Connection,
        song_id: i64,
        played_at: i64,
        duration_secs_played: i64,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO history (song_id, played_at, duration_secs_played) VALUES (?1, ?2, ?3)",
            params![song_id, played_at, duration_secs_played],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Public API used by both HTTP server and playback loop
    // ------------------------------------------------------------------

    /// Best single match. Exact (case-insensitive) wins, then Jaro-Winkler
    /// above threshold.
    pub fn search(&self, query: &str) -> Option<Song> {
        let q = query.to_lowercase();
        let songs = self.all_songs();

        if let Some(s) = songs.iter().find(|s| s.name.to_lowercase() == q) {
            debug!("Exact DB match: {} (id={})", s.name, s.id);
            return Some(s.clone());
        }

        let best = songs
            .iter()
            .map(|s| (s.clone(), jaro_winkler(&s.name.to_lowercase(), &q)))
            .filter(|(_, score)| *score > self.fuzzy_threshold)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        if let Some((song, score)) = best {
            debug!("Fuzzy DB match: {} (score={:.3})", song.name, score);
            return Some(song);
        }

        warn!("No DB match for query: {query}");
        None
    }

    /// All matches above the fuzzy threshold, sorted by score descending.
    pub fn search_all(&self, query: &str) -> Vec<Song> {
        let q = query.to_lowercase();
        let songs = self.all_songs();
        let mut scored: Vec<(Song, f64)> = Vec::new();

        for s in songs.iter() {
            let s_lower = s.name.to_lowercase();
            if s_lower == q {
                scored.retain(|x| x.0.id != s.id);
                scored.insert(0, (s.clone(), 1.0));
            } else {
                let sc = jaro_winkler(&s_lower, &q);
                if sc > self.fuzzy_threshold {
                    scored.push((s.clone(), sc));
                }
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().map(|(s, _)| s).collect()
    }

    /// Insert a song at the front of the queue and start playing it.
    pub fn play(&mut self, query: &str) -> Result<Song, String> {
        let song = self
            .search(query)
            .ok_or_else(|| format!("song not found: {query}"))?;
        self.queue.insert(0, song.clone());
        // DB writes scoped so the lock guard drops before we mutate
        // `cycle_snapshot` (which would otherwise re-borrow `self`).
        {
            let conn = self.lock_conn();
            Self::queue_clear_db(&conn).map_err(|e| e.to_string())?;
            for s in &self.queue {
                Self::queue_append_db(&conn, s.id).map_err(|e| e.to_string())?;
            }
        }
        self.cycle_snapshot = self.queue.clone();
        info!("Play: {} (id={})", song.name, song.id);
        Ok(song)
    }

    /// Append a song to the end of the queue. Honours `max_queue_size`.
    pub fn add(&mut self, query: &str) -> Result<Song, String> {
        if self.max_queue_size > 0 && self.queue.len() >= self.max_queue_size {
            return Err(format!(
                "queue full ({} of {})",
                self.queue.len(),
                self.max_queue_size
            ));
        }
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

    /// Replace the queue with a shuffled copy of the entire library.
    ///
    /// Mirrors `/playlists/{name}/play`: clear the queue, re-insert in
    /// random order, persist to the DB, and refresh `cycle_snapshot` so
    /// `RepeatMode::All` keeps looping over the same shuffle. The daemon
    /// tick loop picks up the new head of queue and starts playback
    /// automatically once the audio engine is idle (same model as
    /// `/play`); calling `play_all_random` while a song is playing only
    /// queues the rest of the library behind the current track.
    ///
    /// Returns the number of songs queued. An empty library surfaces
    /// `"library is empty"` to the HTTP layer so the GUI/CLI can render
    /// a useful toast instead of silently doing nothing.
    pub fn play_all_random(&mut self) -> Result<usize, String> {
        let mut shuffled = self.all_songs();
        if shuffled.is_empty() {
            return Err("library is empty".to_string());
        }
        use rand::seq::SliceRandom;
        shuffled.shuffle(&mut rand::thread_rng());
        {
            let conn = self.lock_conn();
            Self::queue_clear_db(&conn).map_err(|e| e.to_string())?;
            for s in &shuffled {
                Self::queue_append_db(&conn, s.id).map_err(|e| e.to_string())?;
            }
        }
        let len = shuffled.len();
        self.queue = shuffled;
        self.cycle_snapshot = self.queue.clone();
        info!("Play-all: queued {len} songs in random order");
        Ok(len)
    }

    /// In-place shuffle of the current queue.
    pub fn shuffle_queue(&mut self) -> Result<(), String> {
        if self.queue.len() < 2 {
            return Ok(());
        }
        use rand::seq::SliceRandom;
        let mut rng = rand::thread_rng();
        self.queue.shuffle(&mut rng);
        {
            let conn = self.lock_conn();
            Self::queue_clear_db(&conn).map_err(|e| e.to_string())?;
            for s in &self.queue {
                Self::queue_append_db(&conn, s.id).map_err(|e| e.to_string())?;
            }
        }
        // Keep cycle_snapshot in sync so a later repeat-all still loops over
        // something sensible.
        self.cycle_snapshot = self.queue.clone();
        info!("Queue shuffled ({} songs)", self.queue.len());
        Ok(())
    }

    // ------------------------------------------------------------------
    // Downloads (synchronous helper used by worker thread)
    // ------------------------------------------------------------------

    /// Synchronous yt-dlp + DB insert. Runs in a worker thread spawned by
    /// `init_async` and does NOT hold the daemon state mutex for the
    /// blocking subprocess call.
    ///
    /// `name` is the canonical display/storage name; the file on disk is
    /// derived from it via `paths::song_path`. `source` is the literal
    /// argument handed to `yt-dlp` — either an explicit URL the user picked
    /// from the search picker, or a `ytsearch1:NAME` expression for the
    /// legacy auto-pick path.
    ///
    /// Lives as an associated function (not an `&self` method) so callers
    /// literally cannot keep the state mutex locked while `yt-dlp` runs.
    /// Earlier revisions used `&self` here and were invoked as
    /// `state.lock().unwrap().init_sync(...)`, which held the mutex for
    /// the entire subprocess and froze every HTTP request (including the
    /// GUI's `/now-playing` and `/downloads` polls and any subsequent
    /// `/init` calls) until the download finished.
    pub fn init_sync(
        state: &Arc<Mutex<DaemonState>>,
        name: &str,
        source: &str,
    ) -> Result<Song, String> {
        let path = song_path(name);
        debug!("yt-dlp target path: {path} | source: {source}");

        // yt-dlp runs lock-free — the state mutex is NOT held during the
        // blocking subprocess, so the HTTP server keeps serving other
        // routes (downloads polling, search, play, …) while a song
        // downloads in the background.
        let output = std::process::Command::new("yt-dlp")
            .args(["-x", "--audio-format", "mp3", "-o", &path, source])
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

        // Acquire the state lock only briefly, for the DB inserts. The
        // lock is never held across the `yt-dlp` call above. `.unwrap()`
        // matches the rest of `state.rs`/`daemon.rs` and a poisoning
        // here would mean the daemon is already unrecoverable.
        let daemon = state.lock().unwrap();
        let conn = daemon.lock_conn();
        let id = Self::insert_song(&conn, name, &path).map_err(|e| e.to_string())?;
        if let Some(secs) = probe_duration_secs(&path) {
            if let Err(e) = Self::update_duration_secs(&conn, id, secs) {
                warn!("Could not update duration_secs for {name}: {e}");
            }
            info!("Registered song {name} (id={id}, {secs:.1}s) at {path}");
        } else {
            info!("Registered song {name} (id={id}) at {path}");
        }
        Ok(Song {
            id,
            name: name.to_string(),
            path,
            duration_secs: None,
        })
    }

    /// Fetch `limit` yt-dlp search candidates for `query` without downloading.
    /// Returns parsed `YtCandidate` records. Bad JSON lines and yt-dlp
    /// non-zero exit are tolerated as much as possible: an empty list is
    /// returned when nothing parsed, while a yt-dlp startup failure surfaces
    /// a descriptive error string so the CLI can show it to the user.
    ///
    /// Lives as an associated function (no `&self`) so the HTTP handler can
    /// release the daemon state mutex before spawning yt-dlp. Earlier
    /// revisions invoked this as `state.search_yt_sync(...)` *while*
    /// holding the single-threaded `tiny_http` request handler's state
    /// guard, which froze the entire daemon (including `/init`, `/queue`,
    /// `/now-playing`) for the full duration of a yt-dlp search — typically
    /// 30+ seconds on a slow network. Mirrors the lock-free pattern already
    /// used for `init_sync` below.
    pub fn search_yt_sync(query: &str, limit: usize) -> Result<Vec<YtCandidate>, String> {
        let limit = limit.max(1);
        let src = format!("ytsearch{limit}:{query}");
        debug!("yt-dlp search: {src}");
        let output = std::process::Command::new("yt-dlp")
            .args(["--no-warnings", "-j", &src])
            .output();
        let out = match output {
            Ok(o) => o,
            Err(e) => {
                error!("yt-dlp not available: {e}");
                return Err(format!("yt-dlp unavailable: {e}"));
            }
        };
        let mut results = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => {
                    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or_default().to_string();
                    let title = v
                        .get("title")
                        .and_then(|x| x.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let uploader = pick_uploader(&v);
                    let duration_secs = v.get("duration").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let url = v
                        .get("webpage_url")
                        .and_then(|x| x.as_str())
                        .or_else(|| v.get("url").and_then(|x| x.as_str()))
                        .unwrap_or_default()
                        .to_string();
                    let thumbnail = pick_thumbnail(&v);
                    if id.is_empty() || url.is_empty() {
                        continue;
                    }
                    results.push(YtCandidate {
                        id,
                        title,
                        uploader,
                        duration_secs,
                        url,
                        thumbnail,
                    });
                }
                Err(e) => {
                    warn!("yt-dlp: skipping malformed JSON line: {e}");
                }
            }
        }
        // yt-dlp prints one JSON object per result. If it returned 0 objects
        // AND a non-zero exit code, surface stderr so the CLI can show it.
        if results.is_empty() && !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = stderr.trim();
            if !msg.is_empty() {
                warn!("yt-dlp search produced no results and exited non-zero: {msg}");
            }
        }
        Ok(results)
    }

    /// Clear the queue (memory + DB).
    pub fn clear_queue(&mut self) -> Result<(), String> {
        self.queue.clear();
        Self::queue_clear_db(&self.lock_conn()).map_err(|e| e.to_string())?;
        info!("Queue cleared");
        Ok(())
    }

    /// Pop the song at the head of the queue. Only writes history when
    /// `duration_secs_played` is `Some(_)`. Pass `None` for purely
    /// housekeeping pops (start-of-song, /skip, etc).
    pub fn pop_played(
        &mut self,
        duration_secs_played: Option<f64>,
    ) -> Result<Option<Song>, String> {
        if self.queue.is_empty() {
            return Ok(None);
        }
        let id = self.queue[0].id;
        let conn = self.lock_conn();
        conn.execute(
            "DELETE FROM queue
             WHERE song_id = ?1
               AND position = (
                   SELECT MIN(position) FROM queue WHERE song_id = ?1
               )",
            params![id],
        )
        .map_err(|e| format!("failed to delete queue row from DB: {e}"))?;
        if let Some(secs) = duration_secs_played {
            let secs_i = (secs.max(0.0)) as i64;
            if let Err(e) = Self::write_history(&conn, id, unix_secs(), secs_i) {
                warn!("failed to write history for song {id}: {e}");
            }
        }
        drop(conn);
        Ok(Some(self.queue.remove(0)))
    }

    pub fn skip(&mut self) -> Result<Option<Song>, String> {
        if let Some(skipped) = self.pop_played(None)? {
            self.current = None;
            info!("Skipped {}", skipped.name);
            Ok(Some(skipped))
        } else {
            Ok(None)
        }
    }

    // ------------------------------------------------------------------
    // History / stats
    // ------------------------------------------------------------------

    pub fn list_history(&self, limit: usize) -> Vec<HistoryEntry> {
        let conn = self.lock_conn();
        let limit = limit.max(1) as i64;
        let mut stmt = match conn.prepare(
            "SELECT h.id, h.song_id, s.name, h.played_at, h.duration_secs_played
             FROM history h LEFT JOIN songs s ON s.id = h.song_id
             ORDER BY h.played_at DESC
             LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to list history: {e}");
                return Vec::new();
            }
        };
        stmt.query_map(params![limit], |row| {
            Ok(HistoryEntry {
                id: row.get(0)?,
                song_id: row.get(1)?,
                song_name: row.get(2)?,
                played_at: row.get(3)?,
                duration_secs_played: row.get(4)?,
            })
        })
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    /// Snapshot of the current jobs (most-recently-started first).
    pub fn list_downloads(&self) -> Vec<DownloadJob> {
        let mut jobs: Vec<DownloadJob> = self
            .download_jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect();
        jobs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        jobs
    }

    pub fn stats(&self) -> Stats {
        let conn = self.lock_conn();
        let total_plays: i64 = conn
            .query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))
            .unwrap_or(0);
        let total_secs: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(duration_secs_played), 0) FROM history",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let mut stmt = match conn.prepare(
            "SELECT h.song_id, s.name, COUNT(*) AS plays,
                    COALESCE(SUM(h.duration_secs_played), 0) AS total_secs
             FROM history h LEFT JOIN songs s ON s.id = h.song_id
             GROUP BY h.song_id
             ORDER BY plays DESC, total_secs DESC
             LIMIT 10",
        ) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to compute stats: {e}");
                return Stats {
                    total_plays,
                    total_secs,
                    top_songs: Vec::new(),
                };
            }
        };
        let top_songs: Vec<TopSong> = stmt
            .query_map([], |row| {
                Ok(TopSong {
                    song_id: row.get(0)?,
                    name: row.get(1)?,
                    plays: row.get(2)?,
                    total_secs: row.get(3)?,
                })
            })
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();

        Stats {
            total_plays,
            total_secs,
            top_songs,
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
            "SELECT s.id, s.name, s.path, s.duration_secs
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
                duration_secs: row.get::<_, Option<f64>>(3).ok().flatten(),
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

    pub fn remove_from_playlist(&self, playlist: &str, song_id: i64) -> Result<Playlist, String> {
        let conn = self.lock_conn();
        let playlist_id: i64 = conn
            .query_row(
                "SELECT id FROM playlists WHERE name = ?1",
                params![playlist],
                |row| row.get(0),
            )
            .map_err(|_| format!("playlist not found: {playlist}"))?;

        let affected = conn
            .execute(
                "DELETE FROM playlist_songs WHERE playlist_id = ?1 AND song_id = ?2",
                params![playlist_id, song_id],
            )
            .map_err(|e| e.to_string())?;
        if affected == 0 {
            return Err(format!("song {song_id} not in playlist {playlist}"));
        }
        Self::compact_playlist_positions(&conn, playlist_id).map_err(|e| e.to_string())?;
        info!("Removed song {song_id} from playlist {playlist}");
        Ok(Playlist {
            id: playlist_id,
            name: playlist.to_string(),
            songs: Self::load_playlist_songs(&conn, playlist_id),
        })
    }

    pub fn reorder_playlist(
        &self,
        playlist: &str,
        from: usize,
        to: usize,
    ) -> Result<Playlist, String> {
        if from == 0 || to == 0 {
            return Err("positions are 1-based".to_string());
        }
        let conn = self.lock_conn();
        let playlist_id: i64 = conn
            .query_row(
                "SELECT id FROM playlists WHERE name = ?1",
                params![playlist],
                |row| row.get(0),
            )
            .map_err(|_| format!("playlist not found: {playlist}"))?;

        let songs = Self::load_playlist_songs(&conn, playlist_id);
        if from > songs.len() || to > songs.len() {
            return Err(format!(
                "position out of range (playlist has {} songs)",
                songs.len()
            ));
        }
        let mut order = songs.clone();
        let moved = order.remove(from - 1);
        order.insert(to - 1, moved);

        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        for (idx, s) in order.iter().enumerate() {
            tx.execute(
                "UPDATE playlist_songs SET position = ?1
                 WHERE playlist_id = ?2 AND song_id = ?3",
                params![idx as i64 + 1, playlist_id, s.id],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        info!("Reordered playlist {playlist}: {from} -> {to}");
        Ok(Playlist {
            id: playlist_id,
            name: playlist.to_string(),
            songs: Self::load_playlist_songs(&conn, playlist_id),
        })
    }

    fn compact_playlist_positions(conn: &Connection, playlist_id: i64) -> rusqlite::Result<()> {
        let songs = Self::load_playlist_songs(conn, playlist_id);
        for (idx, s) in songs.iter().enumerate() {
            conn.execute(
                "UPDATE playlist_songs SET position = ?1
                 WHERE playlist_id = ?2 AND song_id = ?3",
                params![idx as i64 + 1, playlist_id, s.id],
            )?;
        }
        Ok(())
    }

    pub fn rename_playlist(&self, old: &str, new: &str) -> Result<Playlist, String> {
        let conn = self.lock_conn();
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        let affected = tx
            .execute(
                "UPDATE playlists SET name = ?1 WHERE name = ?2",
                params![new, old],
            )
            .map_err(|e| e.to_string())?;
        if affected == 0 {
            tx.rollback().ok();
            return Err(format!("playlist not found: {old}"));
        }
        let playlist_id: i64 = match tx.query_row(
            "SELECT id FROM playlists WHERE name = ?1",
            params![new],
            |row| row.get(0),
        ) {
            Ok(id) => id,
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                tx.rollback().ok();
                return Err(format!("playlist name already exists: {new}"));
            }
            Err(e) => {
                tx.rollback().ok();
                return Err(e.to_string());
            }
        };
        tx.commit().map_err(|e| e.to_string())?;
        info!("Renamed playlist {old} -> {new}");
        Ok(Playlist {
            id: playlist_id,
            name: new.to_string(),
            songs: Self::load_playlist_songs(&conn, playlist_id),
        })
    }

    pub fn duplicate_playlist(&self, src: &str, dest: &str) -> Result<Playlist, String> {
        let conn = self.lock_conn();
        let playlist_id: i64 = match conn.query_row(
            "SELECT id FROM playlists WHERE name = ?1",
            params![src],
            |row| row.get(0),
        ) {
            Ok(id) => id,
            Err(_) => return Err(format!("playlist not found: {src}")),
        };
        let songs = Self::load_playlist_songs(&conn, playlist_id);
        let new_id = match self.create_playlist(dest) {
            Ok(pl) => pl.id,
            Err(e) => return Err(e),
        };
        for s in &songs {
            Self::playlist_append_db(&conn, new_id, s.id).map_err(|e| e.to_string())?;
        }
        info!("Duplicated playlist {src} -> {dest} ({} songs)", songs.len());
        Ok(Playlist {
            id: new_id,
            name: dest.to_string(),
            songs,
        })
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
        // DB guard dropped; safe to mutate self.queue / cycle_snapshot
        // without re-borrowing self.
        self.queue = songs.clone();
        self.cycle_snapshot = songs.clone();
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub song_id: i64,
    pub song_name: Option<String>,
    pub played_at: i64,
    pub duration_secs_played: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stats {
    pub total_plays: i64,
    pub total_secs: i64,
    pub top_songs: Vec<TopSong>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TopSong {
    pub song_id: i64,
    pub name: Option<String>,
    pub plays: i64,
    pub total_secs: i64,
}

// --------------------------------------------------------------------
// Free helpers / async download dispatch (outside `impl DaemonState`
// because `&Arc<Mutex<Self>>` is not a valid method-receiver type).
// --------------------------------------------------------------------

fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Pick the best "artist / uploader" string for a yt-dlp JSON line.
/// Prefer explicit `artist`, then `channel`, then `uploader`, then `creator`
/// — falls back to an empty string so the CLI formatter doesn't have to
/// special-case missing data.
fn pick_uploader(v: &serde_json::Value) -> String {
    for key in ["artist", "channel", "uploader", "creator"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// Pick a thumbnail URL for a yt-dlp JSON line. Prefers the largest entry in
/// the `thumbnails[]` array (when yt-dlp emits per-resolution variants);
/// falls back to the singular `thumbnail` field. `None` is returned when
/// yt-dlp shipped no cover art at all (rare, but possible for short-lived
/// uploads or age-restricted items).
fn pick_thumbnail(v: &serde_json::Value) -> Option<String> {
    if let Some(arr) = v.get("thumbnails").and_then(|x| x.as_array()) {
        let mut best: Option<(i64, &str)> = None;
        for entry in arr {
            let url = entry.get("url").and_then(|x| x.as_str());
            let h = entry.get("height").and_then(|x| x.as_i64()).unwrap_or(0);
            if let Some(u) = url {
                match best {
                    Some((bh, _)) if bh >= h => {}
                    _ => best = Some((h, u)),
                }
            }
        }
        if let Some((_, url)) = best {
            return Some(url.to_string());
        }
    }
    v.get("thumbnail")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

/// Spawn a worker thread that downloads a song and updates the matching
/// `DownloadJob`. Returns the assigned job id synchronously.
///
/// `name` is the display/storage name under which the song ends up in the
/// library. `source` is the literal argument to pass to `yt-dlp` — either an
/// explicit URL the user picked from `/search/yt` or `ytsearch1:NAME` for the
/// legacy auto-pick path.
pub fn init_async(state: &Arc<Mutex<DaemonState>>, name: String, source: String) -> i64 {
    let job_id = state
        .lock()
        .unwrap()
        .next_job_id
        .fetch_add(1, Ordering::SeqCst);
    let now = unix_secs();
    let job = DownloadJob {
        id: job_id,
        name: name.clone(),
        status: "queued".to_string(),
        song_id: None,
        error: None,
        started_at: now,
        finished_at: None,
        source: Some(source.clone()),
    };
    // Scope the locks so both MutexGuards drop together at the end of the
    // block. A temporary `state.lock()` at statement boundary would be
    // dropped before the inner `download_jobs.lock()` finishes using it.
    {
        let guard = state.lock().unwrap();
        let mut jobs = guard.download_jobs.lock().unwrap();
        jobs.insert(job_id, job);
    }
    // Clone the name so the closure can take ownership while we still print
    // the original in the debug log below.
    let name_for_worker = name.clone();
    let arc = Arc::clone(state);
    std::thread::Builder::new()
        .name(format!("sjnmusic-dl-{job_id}"))
        .spawn(move || {
            run_download_job(arc, job_id, name_for_worker, source);
        })
        .expect("failed to spawn download worker");
    debug!("Spawned download job {job_id} for {name}");
    job_id
}

/// Spawn one worker per name. Returns the assigned ids in the same order.
pub fn init_batch(state: &Arc<Mutex<DaemonState>>, names: Vec<String>) -> Vec<i64> {
    names
        .iter()
        .map(|n| {
            let source = format!("ytsearch1:{n}");
            init_async(state, n.clone(), source)
        })
        .collect()
}

fn run_download_job(
    state: Arc<Mutex<DaemonState>>,
    job_id: i64,
    name: String,
    source: String,
) {
    {
        let guard = state.lock().unwrap();
        let mut jobs = guard.download_jobs.lock().unwrap();
        if let Some(j) = jobs.get_mut(&job_id) {
            j.status = "running".to_string();
        }
    }
    // `init_sync` is now an associated function that manages its own
    // locking: it runs `yt-dlp` lock-free and only grabs the state mutex
    // for a brief DB insert at the end. Do NOT call it as
    // `state.lock().unwrap().init_sync(...)` — that would re-introduce
    // the freeze-while-downloading bug for both the CLI and the GUI.
    let result = DaemonState::init_sync(&state, &name, &source);
    {
        let guard = state.lock().unwrap();
        let mut jobs = guard.download_jobs.lock().unwrap();
        if let Some(j) = jobs.get_mut(&job_id) {
            j.finished_at = Some(unix_secs());
            match &result {
                Ok(song) => {
                    j.status = "done".to_string();
                    j.song_id = Some(song.id);
                }
                Err(e) => {
                    j.status = "failed".to_string();
                    j.error = Some(e.clone());
                }
            }
        }
    }
}
