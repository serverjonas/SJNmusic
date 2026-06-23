use std::sync::{Arc, Mutex};
use std::thread;

use log::{debug, error, info, warn};

use crate::{
    audio::AudioEngine,
    config::Config,
    db::open_db,
    server::start_server,
    state::DaemonState,
};

pub fn run(cfg: Config) {
    info!("Starting sjnmusicd");

    let conn = open_db();
    let state = Arc::new(Mutex::new(DaemonState::new(conn)));

    // HTTP server thread.
    let server_state = state.clone();
    let host = cfg.server.host.clone();
    let port = cfg.server.port;
    let server_handle = thread::Builder::new()
        .name("sjnmusic-http".into())
        .spawn(move || {
            start_server(server_state, &host, port);
        });
    if let Err(e) = server_handle {
        error!("Failed to spawn HTTP server thread: {e}");
    }

    // Audio playback loop (runs on the main thread).
    let mut audio = AudioEngine::new();
    info!("Playback loop ready");

    loop {
        {
            let mut state = state.lock().unwrap();

            // Clear "now playing" once the audio engine reports the song has
            // ended so /now-playing and /queue reflect reality while the
            // next iteration decides what's up next.
            if !audio.is_playing() {
                if let Some(prev) = state.current.take() {
                    debug!("Playback finished for {}", prev.name);
                }
            }

            if !state.queue.is_empty() && !audio.is_playing() {
                // Snapshot the next song first, attempt audio.play, and only
                // remove from the queue (memory + DB) when playback actually
                // starts so decode/init failures don't silently lose it.
                let next = state.queue[0].clone();
                debug!("Starting playback: {} ({})", next.name, next.path);
                match audio.play(&next.path) {
                    Ok(()) => match state.pop_played() {
                        Ok(Some(popped)) => {
                            let name = popped.name.clone();
                            state.current = Some(popped);
                            debug!("DB queue row removed for {name}");
                            info!("Now playing: {name}");
                        }
                        Ok(None) => {
                            // Should not happen — we peeked a non-empty queue.
                            state.current = Some(next.clone());
                            warn!(
                                "pop_played returned None after successful audio.play for {}",
                                next.name
                            );
                        }
                        Err(e) => {
                            // pop_played is DB-first; on Err the in-memory
                            // queue has NOT been mutated, so the song is
                            // already at index 0 — do not re-insert or we
                            // would duplicate it.
                            error!(
                                "Played {} but failed to update DB queue ({e}); \
                                 leaving in-memory queue untouched",
                                next.name
                            );
                            state.current = Some(next);
                        }
                    },
                    Err(e) => {
                        // The previous "now playing" is no longer accurate.
                        state.current = None;
                        error!(
                            "Failed to play {} ({e}); leaving in queue",
                            next.path
                        );
                    }
                }
            }
        }

        thread::sleep(std::time::Duration::from_millis(200));
    }
}
