//! Sync HTTP client wrapping `ureq` plus typed responses for every daemon
//! endpoint. Keeps the surface tiny — every method maps onto exactly one
//! route, so adding endpoints usually means adding a struct + one method.
//!
//! The daemon returns JSON for every route, with errors shaped like
//! `{"error": "..."}` and a non-2xx status. We turn this into a
//! `DaemonError::Api { status, message }` so callers can render the
//! message in a toast without losing the HTTP code.

#![allow(dead_code)] // silenced for compile: the typed Response structs and
                      // many endpoint methods (help, search, volume_info,
                      // repeat_mode, play/add/delete/pause/resume/skip/
                      // seek/set_volume/set_repeat/shuffle/clear/init_batch)
                      // are kept as the canonical `DaemonClient` API even
                      // though the GUI's fire-and-forget path uses
                      // `post_action` instead. Removing them would force
                      // future callers to re-add typed responses.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ureq::{Agent, AgentBuilder};

const DEFAULT_TIMEOUT_MS: u64 = 8000;
const _: Duration = Duration::from_millis(DEFAULT_TIMEOUT_MS);

/// One row from `GET /songs` / search responses.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Song {
    pub id: i64,
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
}

/// One yt-dlp candidate from `GET /search/yt`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct YtCandidate {
    pub id: String,
    pub title: String,
    pub uploader: String,
    pub duration_secs: f64,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
}

/// One persistent playlist with its member songs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub songs: Vec<Song>,
}

/// `GET /playlists/{name}/add`/`reorder`/`duplicate`/`get` ack.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaylistWithAdded {
    pub playlist: Playlist,
    pub added: Option<Song>,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadJob {
    pub id: i64,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub song_id: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueSnapshot {
    pub queue: Vec<Song>,
    #[serde(default)]
    pub current: Option<Song>,
    pub repeat: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NowPlaying {
    #[serde(default)]
    pub current: Option<Song>,
    #[serde(default)]
    pub queue_len: usize,
    #[serde(default)]
    pub elapsed_secs: f64,
    #[serde(default)]
    pub duration_secs: Option<f64>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub playing: bool,
    #[serde(default = "default_volume")]
    pub volume: f32,
    pub repeat: String,
}

fn default_volume() -> f32 {
    1.0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub volume: f32,
    #[serde(default)]
    pub playing: bool,
    #[serde(default)]
    pub paused: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadsList {
    pub downloads: Vec<DownloadJob>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SongsList {
    pub songs: Vec<Song>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchAllResult {
    pub matches: Vec<Song>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchYtResult {
    pub query: String,
    pub limit: usize,
    pub results: Vec<YtCandidate>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryList {
    pub history: Vec<HistoryEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitResponse {
    pub job_id: i64,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitBatchResponse {
    pub job_ids: Vec<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShuffledResponse {
    pub shuffled: bool,
    pub len: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClearedResponse {
    pub cleared: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OkResponse {
    pub ok: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeletedResponse {
    pub deleted: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkipResponse {
    pub skipped: Option<Song>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepeatResponse {
    pub mode: String,
}

/// Error variants returned by the daemon HTTP layer. We surface both the
/// HTTP status and a human-readable message so the UI can render them.
#[derive(Debug, Clone)]
pub enum DaemonError {
    /// `{"error": "..."}` envelope from the daemon. Status preserved.
    Api { status: u16, message: String },
    /// Socket / DNS / connection failure.
    Transport(String),
    /// Response couldn't be parsed as JSON.
    Decode(String),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api { status, message } => write!(f, "[{}] {}", status, message),
            Self::Transport(m) => write!(f, "transport: {}", m),
            Self::Decode(m) => write!(f, "decode: {}", m),
        }
    }
}

impl std::error::Error for DaemonError {}

/// Thin `Clone`-friendly HTTP client. Wrap in `Arc` and share between the
/// background poller and the UI thread (one socket pool, no contention).
#[derive(Clone)]
pub struct DaemonClient {
    base: String,
    agent: Agent,
}

impl DaemonClient {
    pub fn new(host: &str, port: u16) -> Self {
        let agent = AgentBuilder::new()
            .timeout(Duration::from_millis(DEFAULT_TIMEOUT_MS))
            .build();
        Self {
            base: format!("http://{}:{}", host, port),
            agent,
        }
    }

    /// Public POST entry point for fire-and-forget UI actions. Returns the
    /// daemon's error message verbatim on a non-2xx response so callers
    /// (e.g. `fire_action` in `app.rs`) can surface it in toasts/logs.
    /// Transport failures are surfaced as `DaemonError::Transport`.
    pub fn post_action(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<(), DaemonError> {
        let url = format!("{}{}", self.base, path);
        let result = self.agent.post(&url).send_json(ureq::json!(body.clone()));
        match result {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(status, resp)) => Err(read_api_error(status, resp)),
            Err(ureq::Error::Transport(e)) => Err(DaemonError::Transport(e.to_string())),
        }
    }

    /// Endpoint to which requests are sent. Useful for error messages.
    pub fn base(&self) -> &str {
        &self.base
    }

    // ----------------------------------------------------------------
    // GET helpers
    // ----------------------------------------------------------------

    pub fn help(&self) -> Result<serde_json::Value, DaemonError> {
        self.get("/help")
    }

    pub fn now_playing(&self) -> Result<NowPlaying, DaemonError> {
        self.get("/now-playing")
    }

    pub fn queue(&self) -> Result<QueueSnapshot, DaemonError> {
        self.get("/queue")
    }

    pub fn downloads(&self) -> Result<Vec<DownloadJob>, DaemonError> {
        Ok(self.get::<DownloadsList>("/downloads")?.downloads)
    }

    pub fn songs(&self) -> Result<Vec<Song>, DaemonError> {
        Ok(self.get::<SongsList>("/songs")?.songs)
    }

    pub fn history(&self, limit: usize) -> Result<Vec<HistoryEntry>, DaemonError> {
        Ok(self.get::<HistoryList>(&format!("/history?limit={}", limit))?
            .history)
    }

    pub fn stats(&self) -> Result<Stats, DaemonError> {
        self.get("/stats")
    }

    pub fn playlists(&self) -> Result<Vec<Playlist>, DaemonError> {
        self.get("/playlists")
    }

    pub fn get_playlist(&self, name: &str) -> Result<Playlist, DaemonError> {
        self.get(&format!("/playlists/{}", urlencoding::encode(name)))
    }

    pub fn search(&self, q: &str) -> Result<Song, DaemonError> {
        self.get(&format!("/search?q={}", urlencoding::encode(q)))
    }

    pub fn search_all(&self, q: &str) -> Result<Vec<Song>, DaemonError> {
        Ok(self
            .get::<SearchAllResult>(&format!("/search/all?q={}", urlencoding::encode(q)))?
            .matches)
    }

    pub fn search_yt(&self, q: &str, limit: usize) -> Result<Vec<YtCandidate>, DaemonError> {
        // `/search/yt` shells out to `yt-dlp ytsearchN:...` on the daemon
        // side, which can easily take 30+ seconds on a slow network — well
        // past the agent's 8 s default. Override per-request so the picker
        // doesn't ghost-fail with a transport timeout. All other
        // endpoints keep the default; the daemon's state lock is no
        // longer held during the search (see daemon state.rs), so other
        // GETs (now-playing, downloads, queue) keep flowing while a
        // search is in flight.
        Ok(self
            .get_with_timeout::<SearchYtResult>(
                &format!(
                    "/search/yt?q={}&limit={}",
                    urlencoding::encode(q),
                    limit
                ),
                Duration::from_secs(120),
            )?
            .results)
    }

    pub fn volume_info(&self) -> Result<VolumeInfo, DaemonError> {
        self.get("/volume")
    }

    pub fn repeat_mode(&self) -> Result<RepeatResponse, DaemonError> {
        self.get("/repeat")
    }

    // ----------------------------------------------------------------
    // POST / PATCH / PUT / DELETE helpers
    // ----------------------------------------------------------------

    pub fn play(&self, query: &str) -> Result<Song, DaemonError> {
        self.post_json("/play", &serde_json::json!({"query": query}))
    }

    pub fn add(&self, query: &str) -> Result<Song, DaemonError> {
        self.post_json("/add", &serde_json::json!({"query": query}))
    }

    pub fn delete(&self, query: &str) -> Result<Song, DaemonError> {
        self.del_json("/del", &serde_json::json!({"query": query}))
    }

    pub fn pause(&self) -> Result<OkResponse, DaemonError> {
        self.post_json("/pause", &serde_json::json!({}))
    }

    pub fn resume(&self) -> Result<OkResponse, DaemonError> {
        self.post_json("/resume", &serde_json::json!({}))
    }

    pub fn skip(&self) -> Result<SkipResponse, DaemonError> {
        self.post_json("/skip", &serde_json::json!({}))
    }

    pub fn seek(&self, secs: f64) -> Result<OkResponse, DaemonError> {
        self.post_json("/seek", &serde_json::json!({"secs": secs}))
    }

    pub fn set_volume(&self, vol: f32) -> Result<OkResponse, DaemonError> {
        self.post_json("/volume", &serde_json::json!({"vol": vol}))
    }

    pub fn set_repeat(&self, mode: &str) -> Result<RepeatResponse, DaemonError> {
        self.post_json("/repeat", &serde_json::json!({"mode": mode}))
    }

    pub fn shuffle(&self) -> Result<ShuffledResponse, DaemonError> {
        self.post_json("/queue/shuffle", &serde_json::json!({}))
    }

    pub fn clear(&self) -> Result<ClearedResponse, DaemonError> {
        self.post_json("/queue/clear", &serde_json::json!({}))
    }

    pub fn init(&self, name: &str, url: Option<&str>) -> Result<InitResponse, DaemonError> {
        let mut body = serde_json::json!({"song": name, "name": name});
        if let Some(u) = url {
            body["url"] = serde_json::Value::String(u.to_string());
        }
        self.post_json("/init", &body)
    }

    pub fn init_batch(&self, songs: Vec<String>) -> Result<InitBatchResponse, DaemonError> {
        self.post_json("/init/batch", &serde_json::json!({"songs": songs}))
    }

    pub fn create_playlist(&self, name: &str) -> Result<Playlist, DaemonError> {
        self.post_json("/playlists", &serde_json::json!({"name": name}))
    }

    pub fn delete_playlist(&self, name: &str) -> Result<DeletedResponse, DaemonError> {
        self.del(&format!("/playlists/{}", urlencoding::encode(name)))
    }

    pub fn rename_playlist(
        &self,
        old: &str,
        new: &str,
    ) -> Result<Playlist, DaemonError> {
        self.patch(
            &format!("/playlists/{}", urlencoding::encode(old)),
            &serde_json::json!({"name": new}),
        )
    }

    pub fn duplicate_playlist(
        &self,
        src: &str,
        dest: &str,
    ) -> Result<Playlist, DaemonError> {
        self.post_json(
            &format!("/playlists/{}/duplicate", urlencoding::encode(src)),
            &serde_json::json!({"name": dest}),
        )
    }

    pub fn add_to_playlist(
        &self,
        name: &str,
        query: &str,
    ) -> Result<PlaylistWithAdded, DaemonError> {
        self.post_json(
            &format!("/playlists/{}/add", urlencoding::encode(name)),
            &serde_json::json!({"query": query}),
        )
    }

    pub fn remove_from_playlist(
        &self,
        name: &str,
        song_id: i64,
    ) -> Result<Playlist, DaemonError> {
        self.del(&format!(
            "/playlists/{}/songs/{}",
            urlencoding::encode(name),
            song_id
        ))
    }

    pub fn reorder_playlist(
        &self,
        name: &str,
        from: usize,
        to: usize,
    ) -> Result<Playlist, DaemonError> {
        self.post_json(
            &format!("/playlists/{}/reorder", urlencoding::encode(name)),
            &serde_json::json!({"from": from, "to": to}),
        )
    }

    pub fn play_playlist(&self, name: &str) -> Result<Playlist, DaemonError> {
        self.post_json(
            &format!("/playlists/{}/play", urlencoding::encode(name)),
            &serde_json::json!({}),
        )
    }

    // ----------------------------------------------------------------
    // Generic request plumbing
    // ----------------------------------------------------------------

    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, DaemonError> {
        self._request_json("GET", path, None, None)
    }

    /// Like `get` but lets callers override the per-request timeout. Used
    /// only for endpoints that back onto slow subprocess calls (currently
    /// just `/search/yt`, since yt-dlp's first network round-trip can blow
    /// past the 8 s agent default).
    fn get_with_timeout<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        timeout: Duration,
    ) -> Result<T, DaemonError> {
        self._request_json("GET", path, None, Some(timeout))
    }

    fn post_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T, DaemonError> {
        self._request_json("POST", path, Some(body), None)
    }

    fn patch<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T, DaemonError> {
        self._request_json("PATCH", path, Some(body), None)
    }

    fn del_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T, DaemonError> {
        self._request_json("DELETE", path, Some(body), None)
    }

    fn del<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, DaemonError> {
        self._request_json("DELETE", path, None, None)
    }

    fn _request_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        timeout: Option<Duration>,
    ) -> Result<T, DaemonError> {
        let url = format!("{}{}", self.base, path);
        let req = match method {
            "GET" => self.agent.get(&url),
            "POST" => self.agent.post(&url),
            "PATCH" => self.agent.patch(&url),
            "PUT" => self.agent.put(&url),
            "DELETE" => self.agent.delete(&url),
            other => {
                return Err(DaemonError::Transport(format!(
                    "unsupported method {}",
                    other
                )))
            }
        };
        let req = match timeout {
            Some(t) => req.timeout(t),
            None => req,
        };
        let req = match body {
            Some(b) => req.send_json(ureq::json!(b.clone())),
            None => req.call(),
        };
        match req {
            Ok(resp) => parse_response(resp),
            Err(ureq::Error::Status(status, resp)) => {
                Err(read_api_error(status, resp))
            }
            Err(ureq::Error::Transport(e)) => Err(DaemonError::Transport(e.to_string())),
        }
    }
}

/// Common entry point for any `T: Deserialize`. Lives at top level so we
/// can call it from every variant of `_request*` without duplicating the
/// result-matching logic.
fn parse_response<T: for<'de> Deserialize<'de>>(
    resp: ureq::Response,
) -> Result<T, DaemonError> {
    let body = resp
        .into_string()
        .map_err(|e| DaemonError::Decode(e.to_string()))?;
    serde_json::from_str(&body).map_err(|e| DaemonError::Decode(format!("{}: {}", e, body)))
}

/// Try to read an `{"error": "..."}` envelope from a non-2xx response and
/// turn it into a `DaemonError::Api` so the UI can render the daemon's own
/// message verbatim.
fn read_api_error(status: u16, resp: ureq::Response) -> DaemonError {
    let text = resp.into_string().unwrap_or_default();
    let msg = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| {
            if text.is_empty() {
                format!("HTTP {}", status)
            } else {
                text
            }
        });
    DaemonError::Api { status, message: msg }
}

/// Convenience: share one `Arc<DaemonClient>` between worker + UI thread.
pub fn shared(host: &str, port: u16) -> Arc<DaemonClient> {
    Arc::new(DaemonClient::new(host, port))
}
