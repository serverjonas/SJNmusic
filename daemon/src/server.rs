use std::sync::{Arc, Mutex};

use log::{debug, error, info, warn};
use serde::Serialize;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::audio::AudioCmd;
use crate::state::{init_async, init_batch, DaemonState, RepeatMode};
use crate::youtube::{pick_best, rank_query, AUTO_PICK_MARGIN};

#[derive(Serialize)]
struct ApiError {
    error: String,
}

fn json_response<T: Serialize>(status: u16, body: &T) -> Response<std::io::Cursor<Vec<u8>>> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|e| {
        error!("JSON serialization failed: {e}");
        br#"{"error":"serialization failed"}"#.to_vec()
    });
    Response::from_data(bytes)
        .with_status_code(StatusCode(status))
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
        )
}

fn error_response(msg: impl Into<String>) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(400, &ApiError { error: msg.into() })
}

fn parse_body(body: &str) -> serde_json::Result<serde_json::Value> {
    if body.trim().is_empty() {
        Ok(serde_json::Value::Null)
    } else {
        serde_json::from_str(body)
    }
}

pub fn start_server(state: Arc<Mutex<DaemonState>>, host: &str, port: u16) {
    let addr = format!("{host}:{port}");
    let server = match Server::http(&addr) {
        Ok(s) => {
            info!("HTTP server listening on http://{addr}");
            info!("Set RUST_LOG=debug for verbose logs");
            s
        }
        Err(e) => {
            error!("Failed to bind HTTP server on {addr}: {e}");
            return;
        }
    };

    for request in server.incoming_requests() {
        let state = state.clone();
        let url = request.url().to_string();
        let method = request.method().clone();
        debug!("{} {}", method, url);
        if let Err(e) = handle(request, state, &method, &url) {
            warn!("Failed to respond: {e}");
        }
    }
}

fn read_body(request: &mut Request) -> Result<String, std::io::Error> {
    let mut s = String::new();
    let _ = request.as_reader().read_to_string(&mut s)?;
    Ok(s)
}

fn handle(
    mut request: Request,
    state: Arc<Mutex<DaemonState>>,
    method: &Method,
    url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = url.split('?').next().unwrap_or(url);
    let route = route_for(method, path);

    if matches!(route, Route::Help) {
        let resp = json_response(200, &serde_json::json!({
            "endpoints": [
                "GET  /help",
                "GET  /songs?q=",
                "GET  /search?q=... (best match) or POST /search {query}",
                "GET  /search/all?q=... (every match above threshold)",
                "GET  /search/yt?q=...&limit=N (raw yt-dlp candidates for picker)",
                "GET  /search/yt/ranked?q=...&limit=N (scored + sorted candidates; re-ranks 3 query variants)",
                "GET  /pick?q=...&limit=N&margin=N (auto-pick when top score > runner-up by margin, otherwise returns candidates)",
                "GET  /queue",
                "GET  /now-playing",
                "GET  /downloads",
                "GET  /history?limit=N",
                "GET  /stats",
                "GET  /repeat",
                "GET  /volume",
                "POST /play {query}",
                "POST /add {query}",
                "POST /del {query}",
                "POST /init {song} (async, returns job id, status 202)",
                "POST /init/batch {songs:[...]}",
                "POST /skip",
                "POST /pause",
                "POST /resume",
                "POST /seek {secs}",
                "POST /volume {vol}",
                "POST /repeat {mode}",
                "POST /queue/shuffle",
                "POST /queue/clear",
                "GET  /playlists",
                "POST /playlists {name}",
                "GET  /playlists/{name}",
                "PATCH /playlists/{name} {name}",
                "DELETE /playlists/{name}",
                "POST /playlists/{name}/add {query}",
                "POST /playlists/{name}/play",
                "DELETE /playlists/{name}/songs/{id}",
                "POST /playlists/{name}/reorder {from, to}",
                "POST /playlists/{name}/duplicate {name}",
            ]
        }));
        return Ok(request.respond(resp)?);
    }

    let mut body_text = String::new();
    if matches!(method, Method::Post | Method::Delete | Method::Put | Method::Patch) {
        match read_body(&mut request) {
            Ok(s) => body_text = s,
            Err(e) => {
                warn!("Failed to read body: {e}");
                let resp = error_response("could not read request body");
                return Ok(request.respond(resp)?);
            }
        }
    }
    debug!("Body: {}", body_text);

    let query_params = parse_query(url);

    // Clone the Arc so async routes can spawn workers that retain their
    // own handle after we lock the original Arc into a MutexGuard.
    let arc_for_async = Arc::clone(&state);

    let mut state = match state.lock() {
        Ok(s) => s,
        Err(p) => {
            error!("State mutex poisoned: {p}");
            let resp = error_response("internal state error");
            return Ok(request.respond(resp)?);
        }
    };

    let response = match route {
        Route::Help => unreachable!(),

        Route::Songs => {
            let q = query_params.get("q").map(|s| s.as_str());
            json_response(200, &serde_json::json!({
                "songs": state.list_songs_filtered(q),
            }))
        }

        Route::Search => match query_or_body(&query_params, &body_text) {
            Ok(q) => match state.search(&q) {
                Some(song) => json_response(200, &song),
                None => json_response(404, &ApiError {
                    error: format!("song not found: {q}"),
                }),
            },
            Err(e) => error_response(e),
        },

        Route::SearchAll => match query_or_body(&query_params, &body_text) {
            Ok(q) => json_response(200, &serde_json::json!({
                "matches": state.search_all(&q),
            })),
            Err(e) => error_response(e),
        },

        Route::SearchYt => match query_or_body(&query_params, &body_text) {
            Ok(q) => {
                let limit = query_params
                    .get("limit")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(state.search_count);
                // Release the synchronous state guard BEFORE calling
                // search_yt_sync — yt-dlp can take 30+ seconds on a slow
                // network and holding the daemon mutex here would freeze
                // every other endpoint (including the GUI's `/init`,
                // `/downloads`, `/now-playing` polls) for the entire
                // duration. Mirrors the drop-then-call pattern that
                // `init_sync` already uses.
                drop(state);
                match DaemonState::search_yt_sync(&q, limit) {
                    Ok(results) => json_response(200, &serde_json::json!({
                        "query": q,
                        "limit": limit,
                        "results": results,
                    })),
                    Err(e) => error_response(e),
                }
            }
            Err(e) => error_response(e),
        },

        Route::SearchYtRanked => match query_or_body(&query_params, &body_text) {
            Ok(q) => {
                let limit = query_params
                    .get("limit")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(state.search_count);
                // Same lock-drop reasoning as `/search/yt` — parallel
                // yt-dlp invocations can reach 30s+ each before they
                // return, and we don't want to keep the state mutex for
                // the duration.
                drop(state);
                match rank_query(&q, limit) {
                    Ok(ranked) => json_response(200, &serde_json::json!({
                        "query": q,
                        "limit": limit,
                        "results": ranked,
                    })),
                    Err(e) => error_response(e),
                }
            }
            Err(e) => error_response(e),
        },

        Route::Pick => match query_or_body(&query_params, &body_text) {
            Ok(q) => {
                let limit = query_params
                    .get("limit")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(state.search_count);
                let margin = query_params
                    .get("margin")
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(AUTO_PICK_MARGIN);
                drop(state);
                match pick_best(&q, limit, margin) {
                    Ok(resp) => json_response(200, &resp),
                    Err(e) => error_response(e),
                }
            }
            Err(e) => error_response(e),
        },

        Route::Queue => json_response(200, &serde_json::json!({
            "queue": state.queue,
            "current": state.current,
            "repeat": state.repeat_mode.as_str(),
        })),

        Route::NowPlaying => json_response_now_playing(&state),

        Route::Downloads => json_response(200, &serde_json::json!({
            "downloads": state.list_downloads(),
        })),

        Route::History => {
            let limit: usize = query_params
                .get("limit")
                .and_then(|s| s.parse().ok())
                .unwrap_or(50);
            json_response(200, &serde_json::json!({
                "history": state.list_history(limit),
            }))
        }

        Route::Stats => json_response(200, &state.stats()),

        Route::Play => match query_or_body(&query_params, &body_text) {
            Ok(q) => match state.play(&q) {
                Ok(song) => json_response(200, &song),
                Err(e) => error_response(e),
            },
            Err(e) => error_response(e),
        },

        Route::Add => match query_or_body(&query_params, &body_text) {
            Ok(q) => match state.add(&q) {
                Ok(song) => json_response(200, &song),
                Err(e) => json_response(
                    if e.contains("queue full") { 413 } else { 400 },
                    &ApiError { error: e },
                ),
            },
            Err(e) => error_response(e),
        },

        Route::Del => match query_or_body(&query_params, &body_text) {
            Ok(q) => match state.delete(&q) {
                Ok(song) => json_response(200, &song),
                Err(e) => error_response(e),
            },
            Err(e) => error_response(e),
        },

        Route::InitAsync => match name_and_url_from_body(&body_text) {
            Ok((song, url)) => {
                let source = match url {
                    Some(u) => u,
                    None => format!("ytsearch1:{song}"),
                };
                // Release the synchronous state guard BEFORE calling
                // init_async — it re-locks the same `Arc<Mutex<DaemonState>>`
                // from this very thread to register the new job and spawn
                // the worker. `std::sync::Mutex` is not reentrant; without
                // this drop the second `lock()` hangs tiny_http's single
                // request thread, which in turn stalls every subsequent
                // request (including /downloads polling in the GUI).
                drop(state);
                let id = init_async(&arc_for_async, song, source);
                json_response(202, &serde_json::json!({
                    "job_id": id,
                    "status": "queued",
                }))
            }
            Err(e) => error_response(e),
        },

        Route::InitBatch => match songs_from_body(&body_text) {
            Ok(songs) => {
                // Same reasoning as `/init`: init_batch → init_async, so
                // the outer state guard must be released before we enter
                // the helper.
                drop(state);
                let ids = init_batch(&arc_for_async, songs);
                json_response(202, &serde_json::json!({
                    "job_ids": ids,
                }))
            }
            Err(e) => error_response(e),
        },

        Route::Skip => {
            state.audio_handle.send(AudioCmd::Stop);
            // Always clear current — a skip is an explicit decision to
            // abandon the playing song, so we don't want the daemon tick to
            // later treat it as a natural end-of-track and write a bogus
            // `history` row.
            state.current = None;
            match state.skip() {
                Ok(Some(s)) => json_response(200, &s),
                Ok(None) => json_response(200, &serde_json::json!({"skipped": null})),
                Err(e) => error_response(e),
            }
        }

        Route::QueueShuffle => match state.shuffle_queue() {
            Ok(()) => json_response(200, &serde_json::json!({"shuffled": true, "len": state.queue.len()})),
            Err(e) => error_response(e),
        },

        Route::QueueClear => match state.clear_queue() {
            Ok(()) => json_response(200, &serde_json::json!({"cleared": true})),
            Err(e) => error_response(e),
        },

        Route::Audio(method) => handle_audio_route(&state, method, &body_text),

        Route::RepeatGet => json_response(200, &serde_json::json!({
            "mode": state.repeat_mode.as_str(),
        })),

        Route::RepeatSet => match mode_from_body_or_default(&body_text) {
            Ok(mode) => {
                state.repeat_mode = mode;
                if matches!(mode, RepeatMode::All) && state.cycle_snapshot.is_empty() {
                    state.cycle_snapshot = state.queue.clone();
                }
                info!("Repeat mode set to {}", mode.as_str());
                json_response(200, &serde_json::json!({"mode": mode.as_str()}))
            }
            Err(e) => error_response(e),
        },

        Route::VolumeGet => {
            let snap = state.audio_handle.snapshot();
            json_response(200, &serde_json::json!({
                "volume": snap.volume,
                "playing": snap.playing,
                "paused": snap.paused,
            }))
        }

        Route::VolumeSet => match vol_from_body(&body_text) {
            Ok(v) => {
                state.audio_handle.send(AudioCmd::SetVolume(v));
                json_response(200, &serde_json::json!({
                    "volume": state.audio_handle.snapshot().volume,
                    "ok": true,
                }))
            }
            Err(e) => error_response(e),
        },

        Route::PlaylistsList => json_response(200, &state.list_playlists()),

        Route::PlaylistCreate => match name_from_body(&body_text) {
            Ok(name) => match state.create_playlist(&name) {
                Ok(pl) => json_response(200, &pl),
                Err(e) => error_response(e),
            },
            Err(e) => error_response(e),
        },

        Route::PlaylistGet(name) => match state.get_playlist(&name) {
            Ok(pl) => json_response(200, &pl),
            Err(e) => error_response(e),
        },

        Route::PlaylistRename(name) => match new_name_from_body(&body_text) {
            Ok(new_name) => match state.rename_playlist(&name, &new_name) {
                Ok(pl) => json_response(200, &pl),
                Err(e) => error_response(e),
            },
            Err(e) => error_response(e),
        },

        Route::PlaylistDelete(name) => match state.delete_playlist(&name) {
            Ok(()) => json_response(200, &serde_json::json!({"deleted": name})),
            Err(e) => error_response(e),
        },

        Route::PlaylistAdd(name) => match query_or_body(&query_params, &body_text) {
            Ok(q) => match state.add_to_playlist(&name, &q) {
                Ok((pl, song)) => json_response(200, &serde_json::json!({
                    "playlist": pl,
                    "added": song,
                })),
                Err(e) => error_response(e),
            },
            Err(e) => error_response(e),
        },

        Route::PlaylistPlay(name) => match state.play_playlist(&name) {
            Ok(pl) => {
                state.cycle_snapshot = pl.songs.clone();
                json_response(200, &pl)
            }
            Err(e) => error_response(e),
        },

        Route::PlaylistRemoveSong(name, id) => match state.remove_from_playlist(&name, id) {
            Ok(pl) => json_response(200, &pl),
            Err(e) => error_response(e),
        },

        Route::PlaylistReorder(name) => match reorder_from_body(&body_text) {
            Ok((from, to)) => match state.reorder_playlist(&name, from, to) {
                Ok(pl) => json_response(200, &pl),
                Err(e) => error_response(e),
            },
            Err(e) => error_response(e),
        },

        Route::PlaylistDuplicate(name) => match new_name_from_body(&body_text) {
            Ok(new_name) => match state.duplicate_playlist(&name, &new_name) {
                Ok(pl) => json_response(200, &pl),
                Err(e) => error_response(e),
            },
            Err(e) => error_response(e),
        },

        Route::NotFound => json_response(404, &ApiError {
            error: format!("no route for {method} {path}"),
        }),
    };

    Ok(request.respond(response)?)
}

fn json_response_now_playing(state: &DaemonState) -> Response<std::io::Cursor<Vec<u8>>> {
    let snap = state.audio_handle.snapshot();
    let current_duration = state.current.as_ref().and_then(|c| c.duration_secs);
    let duration = snap.duration_secs.or(current_duration);
    json_response(
        200,
        &serde_json::json!({
            "current": state.current,
            "queue_len": state.queue.len(),
            "elapsed_secs": snap.elapsed_secs(),
            "duration_secs": duration,
            "paused": snap.paused,
            "playing": snap.playing,
            "volume": snap.volume,
            "repeat": state.repeat_mode.as_str(),
        }),
    )
}

#[derive(Debug)]
enum AudioRoute {
    Pause,
    Resume,
    Seek,
}

fn handle_audio_route(
    state: &DaemonState,
    route: AudioRoute,
    body: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    match route {
        AudioRoute::Pause => {
            state.audio_handle.send(AudioCmd::Pause);
            json_response(200, &serde_json::json!({"ok": true, "label": "pause"}))
        }
        AudioRoute::Resume => {
            state.audio_handle.send(AudioCmd::Resume);
            json_response(200, &serde_json::json!({"ok": true, "label": "resume"}))
        }
        AudioRoute::Seek => match secs_from_body(body) {
            Ok(s) => {
                state.audio_handle.send(AudioCmd::Seek(s));
                json_response(200, &serde_json::json!({"ok": true, "label": "seek", "secs": s}))
            }
            Err(e) => error_response(e),
        },
    }
}

#[derive(Debug)]
enum Route {
    Help,
    Songs,
    Search,
    SearchAll,
    SearchYt,
    SearchYtRanked,
    Pick,
    Queue,
    NowPlaying,
    Downloads,
    History,
    Stats,
    Play,
    Add,
    Del,
    InitAsync,
    InitBatch,
    Skip,
    QueueShuffle,
    QueueClear,
    Audio(AudioRoute),
    RepeatGet,
    RepeatSet,
    VolumeGet,
    VolumeSet,
    PlaylistsList,
    PlaylistCreate,
    PlaylistGet(String),
    PlaylistRename(String),
    PlaylistDelete(String),
    PlaylistAdd(String),
    PlaylistPlay(String),
    PlaylistRemoveSong(String, i64),
    PlaylistReorder(String),
    PlaylistDuplicate(String),
    NotFound,
}

fn route_for(method: &Method, path: &str) -> Route {
    let p = path.trim_end_matches('/');
    match (method, p) {
        (Method::Get, "") | (Method::Get, "/help") => Route::Help,
        (Method::Get, "/songs") => Route::Songs,
        (m, "/search") if matches!(m, Method::Get | Method::Post) => Route::Search,
        (Method::Get, "/search/all") => Route::SearchAll,
        (Method::Get, "/search/yt/ranked") => Route::SearchYtRanked,
        (Method::Get, "/pick") => Route::Pick,
        (Method::Get, "/search/yt") => Route::SearchYt,
        (Method::Get, "/queue") => Route::Queue,
        (Method::Get, "/now-playing") => Route::NowPlaying,
        (Method::Get, "/downloads") => Route::Downloads,
        (Method::Get, "/history") => Route::History,
        (Method::Get, "/stats") => Route::Stats,
        (Method::Post, "/play") => Route::Play,
        (Method::Post, "/add") => Route::Add,
        (Method::Post, "/del") => Route::Del,
        (Method::Post, "/init") => Route::InitAsync,
        (Method::Post, "/init/batch") => Route::InitBatch,
        (Method::Post, "/skip") => Route::Skip,
        (Method::Post, "/queue/shuffle") => Route::QueueShuffle,
        (Method::Post, "/queue/clear") => Route::QueueClear,
        (Method::Post, "/pause") => Route::Audio(AudioRoute::Pause),
        (Method::Post, "/resume") => Route::Audio(AudioRoute::Resume),
        (Method::Post, "/seek") => Route::Audio(AudioRoute::Seek),
        (Method::Get, "/repeat") => Route::RepeatGet,
        (Method::Post, "/repeat") => Route::RepeatSet,
        (Method::Get, "/volume") => Route::VolumeGet,
        (Method::Post, "/volume") => Route::VolumeSet,
        (Method::Get, "/playlists") => Route::PlaylistsList,
        (Method::Post, "/playlists") => Route::PlaylistCreate,
        (Method::Get, path) if path.starts_with("/playlists/") => {
            playlist_get_route(path)
        }
        (Method::Patch, path) if path.starts_with("/playlists/") => {
            let rest = strip_prefix("/playlists/", path);
            let (name, suffix) = split_playlist_suffix(rest);
            match suffix {
                "" => Route::PlaylistRename(name),
                _ => Route::NotFound,
            }
        }
        (Method::Delete, path) if path.starts_with("/playlists/") => {
            let rest = strip_prefix("/playlists/", path);
            if let Some((name, id)) = split_song_path(rest) {
                return Route::PlaylistRemoveSong(name, id);
            }
            Route::PlaylistDelete(rest.to_string())
        }
        (Method::Post, path) if path.starts_with("/playlists/") => {
            let rest = strip_prefix("/playlists/", path);
            let (name, suffix) = split_playlist_suffix(rest);
            match suffix {
                "/add" => Route::PlaylistAdd(name),
                "/play" => Route::PlaylistPlay(name),
                "/reorder" => Route::PlaylistReorder(name),
                "/duplicate" => Route::PlaylistDuplicate(name),
                _ => Route::NotFound,
            }
        }
        _ => Route::NotFound,
    }
}

fn playlist_get_route(path: &str) -> Route {
    let rest = strip_prefix("/playlists/", path);
    let (name, suffix) = split_playlist_suffix(rest);
    match suffix {
        "/add" | "/play" | "/reorder" | "/duplicate" => Route::NotFound,
        "" => Route::PlaylistGet(name),
        _ => Route::NotFound,
    }
}

fn strip_prefix<'a>(prefix: &str, s: &'a str) -> &'a str {
    &s[prefix.len()..]
}

fn split_playlist_suffix(rest: &str) -> (String, &'static str) {
    for suf in ["/duplicate", "/reorder", "/add", "/play"] {
        if let Some(name) = rest.strip_suffix(suf) {
            return (name.to_string(), suf);
        }
    }
    (rest.to_string(), "")
}

fn split_song_path(rest: &str) -> Option<(String, i64)> {
    let idx = rest.find("/songs/")?;
    let name = &rest[..idx];
    let tail = &rest[idx + "/songs/".len()..];
    let id: i64 = tail.parse().ok()?;
    Some((name.to_string(), id))
}

fn parse_query(url: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if let Some(q) = url.split('?').nth(1) {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                out.insert(urldecode(k), urldecode(v));
            }
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    use percent_encoding::percent_decode_str;
    percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
        .replace('+', " ")
}

fn query_or_body(
    query_params: &std::collections::HashMap<String, String>,
    body: &str,
) -> Result<String, String> {
    if let Some(q) = query_params.get("q") {
        if !q.is_empty() {
            return Ok(q.clone());
        }
    }
    let v = parse_body(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    if let Some(obj) = v.as_object() {
        for key in ["query", "name", "song", "q"] {
            if let Some(s) = obj.get(key).and_then(|x| x.as_str()) {
                if !s.is_empty() {
                    return Ok(s.to_string());
                }
            }
        }
    }
    Err("missing 'query' parameter".to_string())
}

fn name_from_body(body: &str) -> Result<String, String> {
    let v = parse_body(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    if let Some(obj) = v.as_object() {
        for key in ["name", "song", "query"] {
            if let Some(s) = obj.get(key).and_then(|x| x.as_str()) {
                if !s.is_empty() {
                    return Ok(s.to_string());
                }
            }
        }
    }
    Err("missing 'name' or 'song' in body".to_string())
}

/// Parse `/init` bodies that may carry an explicit download URL selected via
/// the search picker. `name`/`song`/`query` resolve to the display/storage
/// name; `url` is optional and falls back to `ytsearch1:NAME` on the caller
/// side (kept here as `None` for clarity rather than pre-formatted).
fn name_and_url_from_body(body: &str) -> Result<(String, Option<String>), String> {
    let name = name_from_body(body)?;
    let v = parse_body(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    let url = v
        .as_object()
        .and_then(|obj| obj.get("url"))
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok((name, url))
}

fn songs_from_body(body: &str) -> Result<Vec<String>, String> {
    let v = parse_body(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    if let Some(arr) = v.get("songs").and_then(|x| x.as_array()) {
        let mut out = Vec::with_capacity(arr.len());
        for s in arr {
            if let Some(s) = s.as_str() {
                if !s.is_empty() {
                    out.push(s.to_string());
                }
            }
        }
        if !out.is_empty() {
            return Ok(out);
        }
    }
    Err("missing 'songs' array in body".to_string())
}

fn new_name_from_body(body: &str) -> Result<String, String> {
    let v = parse_body(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    if let Some(obj) = v.as_object() {
        for key in ["name", "new_name"] {
            if let Some(s) = obj.get(key).and_then(|x| x.as_str()) {
                if !s.is_empty() {
                    return Ok(s.to_string());
                }
            }
        }
    }
    Err("missing 'name' in body".to_string())
}

fn secs_from_body(body: &str) -> Result<f64, String> {
    let v = parse_body(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    if let Some(obj) = v.as_object() {
        for key in ["secs", "seconds", "pos", "position"] {
            if let Some(s) = obj.get(key) {
                if let Some(n) = s.as_f64() {
                    return Ok(n);
                }
                if let Some(n) = s.as_i64() {
                    return Ok(n as f64);
                }
            }
        }
    }
    Err("missing 'secs' in body".to_string())
}

fn vol_from_body(body: &str) -> Result<f32, String> {
    let v = parse_body(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    let obj = v.as_object().ok_or_else(|| "expected JSON object".to_string())?;
    for key in ["vol", "volume", "v"] {
        if let Some(s) = obj.get(key) {
            if let Some(n) = s.as_f64() {
                return Ok(n as f32);
            }
            if let Some(n) = s.as_i64() {
                return Ok(n as f32);
            }
        }
    }
    Err("missing 'volume' in body".to_string())
}

fn mode_from_body_or_default(body: &str) -> Result<RepeatMode, String> {
    let v = parse_body(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "expected JSON object".to_string())?;
    for key in ["mode", "name"] {
        if let Some(s) = obj.get(key).and_then(|x| x.as_str()) {
            return RepeatMode::parse(s).ok_or_else(|| format!("invalid mode: {s}"));
        }
    }
    Err("missing 'mode' in body".to_string())
}

fn reorder_from_body(body: &str) -> Result<(usize, usize), String> {
    let v = parse_body(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "expected JSON object".to_string())?;
    let from = obj
        .get("from")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| "missing integer 'from'".to_string())?;
    let to = obj
        .get("to")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| "missing integer 'to'".to_string())?;
    Ok((from as usize, to as usize))
}
