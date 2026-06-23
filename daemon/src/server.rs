use std::sync::{Arc, Mutex};

use log::{debug, error, info, warn};
use serde::Serialize;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::state::DaemonState;

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

/// Owns the request so we can call `request.respond()` at the end. Reads the
/// body (for POST/PUT/DELETE), routes, and replies with JSON.
fn handle(
    mut request: Request,
    state: Arc<Mutex<DaemonState>>,
    method: &Method,
    url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = url.split('?').next().unwrap_or(url);
    let route = route_for(method, path);

    // /help short-circuit: no body, no state.
    if matches!(route, Route::Help) {
        let resp = json_response(200, &serde_json::json!({
            "endpoints": [
                "GET  /help",
                "GET  /songs",
                "GET  /search?q=... or POST /search {query}",
                "GET  /queue",
                "GET  /now-playing",
                "POST /play {query}",
                "POST /add {query}",
                "POST /del {query}",
                "POST /init {song}",
                "POST /skip",
                "POST /queue/clear",
                "GET  /playlists",
                "POST /playlists {name}",
                "GET  /playlists/{name}",
                "POST /playlists/{name}/add {query}",
                "POST /playlists/{name}/play",
                "DELETE /playlists/{name}",
            ]
        }));
        return Ok(request.respond(resp)?);
    }

    let mut body_text = String::new();
    if matches!(method, Method::Post | Method::Delete | Method::Put) {
        let _ = request.as_reader().read_to_string(&mut body_text)?;
    }
    debug!("Body: {}", body_text);

    let query_params = parse_query(url);

    let mut state = match state.lock() {
        Ok(s) => s,
        Err(p) => {
            error!("State mutex poisoned: {p}");
            let resp = error_response("internal state error");
            return Ok(request.respond(resp)?);
        }
    };

    let response = match route {
        Route::Help => unreachable!("handled above"),

        Route::Songs => json_response(200, &serde_json::json!({
            "songs": state.all_songs(),
        })),

        Route::Search => match query_or_body(&query_params, &body_text) {
            Ok(q) => match state.search(&q) {
                Some(song) => json_response(200, &song),
                None => json_response(404, &ApiError {
                    error: format!("song not found: {q}"),
                }),
            },
            Err(e) => error_response(e),
        },

        Route::Queue => json_response(200, &serde_json::json!({
            "queue": state.queue,
            "current": state.current,
        })),

        Route::NowPlaying => json_response(200, &serde_json::json!({
            "current": state.current,
            "queue_len": state.queue.len(),
        })),

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
                Err(e) => error_response(e),
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

        Route::Init => match name_from_body(&body_text) {
            Ok(song) => match state.init(song) {
                Ok(s) => json_response(200, &s),
                Err(e) => error_response(e),
            },
            Err(e) => error_response(e),
        },

        Route::Skip => match state.skip() {
            Ok(Some(s)) => json_response(200, &s),
            Ok(None) => json_response(200, &serde_json::json!({"skipped": null})),
            Err(e) => error_response(e),
        },

        Route::QueueClear => match state.clear_queue() {
            Ok(()) => json_response(200, &serde_json::json!({"cleared": true})),
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
            Ok(pl) => json_response(200, &pl),
            Err(e) => error_response(e),
        },

        Route::PlaylistDelete(name) => match state.delete_playlist(&name) {
            Ok(()) => json_response(200, &serde_json::json!({"deleted": name})),
            Err(e) => error_response(e),
        },

        Route::NotFound => json_response(404, &ApiError {
            error: format!("no route for {method} {path}"),
        }),
    };

    Ok(request.respond(response)?)
}

enum Route {
    Help,
    Songs,
    Search,
    Queue,
    NowPlaying,
    Play,
    Add,
    Del,
    Init,
    Skip,
    QueueClear,
    PlaylistsList,
    PlaylistCreate,
    PlaylistGet(String),
    PlaylistAdd(String),
    PlaylistPlay(String),
    PlaylistDelete(String),
    NotFound,
}

fn route_for(method: &Method, path: &str) -> Route {
    let p = path.trim_end_matches('/');
    match (method, p) {
        (Method::Get, "") | (Method::Get, "/help") => Route::Help,
        (Method::Get, "/songs") => Route::Songs,
        (m, "/search") if matches!(m, Method::Get | Method::Post) => Route::Search,
        (Method::Get, "/queue") => Route::Queue,
        (Method::Get, "/now-playing") => Route::NowPlaying,
        (Method::Post, "/play") => Route::Play,
        (Method::Post, "/add") => Route::Add,
        (Method::Post, "/del") => Route::Del,
        (Method::Post, "/init") => Route::Init,
        (Method::Post, "/skip") => Route::Skip,
        (Method::Post, "/queue/clear") => Route::QueueClear,
        (Method::Get, "/playlists") => Route::PlaylistsList,
        (Method::Post, "/playlists") => Route::PlaylistCreate,
        (Method::Get, path) if path.starts_with("/playlists/") => {
            let rest = &path["/playlists/".len()..];
            let (name, suffix) = split_playlist_route(rest);
            match suffix {
                "/add" => Route::PlaylistAdd(name),
                "/play" => Route::PlaylistPlay(name),
                "" => Route::PlaylistGet(name),
                _ => Route::NotFound,
            }
        }
        (Method::Delete, path) if path.starts_with("/playlists/") => {
            let name = path["/playlists/".len()..].to_string();
            Route::PlaylistDelete(name)
        }
        _ => Route::NotFound,
    }
}

fn split_playlist_route(rest: &str) -> (String, &'static str) {
    if let Some(name) = rest.strip_suffix("/add") {
        return (name.to_string(), "/add");
    }
    if let Some(name) = rest.strip_suffix("/play") {
        return (name.to_string(), "/play");
    }
    (rest.to_string(), "")
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

/// Percent-decodes using `url` crate's `percent_encoding` so that UTF-8
/// sequences survive intact.
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
