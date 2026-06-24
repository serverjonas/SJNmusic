use std::sync::{Arc, Mutex};
use std::thread;

use log::{debug, error, info, warn};

use crate::{
    audio::spawn_audio_thread,
    config::Config,
    db::open_db,
    server::start_server,
    state::{DaemonState, RepeatMode, Song},
};

pub fn run(cfg: Config) {
    info!("Starting sjnmusicd");

    let conn = open_db();
    let audio_handle = Arc::new(spawn_audio_thread());

    let default_repeat = RepeatMode::parse(&cfg.library.default_repeat).unwrap_or(RepeatMode::Off);
    let state = Arc::new(Mutex::new(DaemonState::new(
        conn,
        cfg.search.fuzzy_threshold,
        cfg.library.max_queue_size,
        default_repeat,
        audio_handle,
    )));

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

    info!("Playback loop ready");

    loop {
        tick(&state);
        thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// One iteration of the playback loop. Pulled out so it can be unit-tested
/// without a sleep or a network server.
fn tick(state: &Arc<Mutex<DaemonState>>) {
    let mut state = state.lock().unwrap();

    // Single snapshot per tick — cheaper (one lock acquisition) and avoids
    // races where the audio thread flips `playing` between two reads.
    let snap = state.audio_handle.snapshot();

    // 1) End-of-track / failed-play handling. We only decide what happened
    // AFTER the audio thread has finished deciding things itself, i.e.
    // `audio_pending_play == false`. The two outcomes we differentiate now:
    //   - `snap.last_play_failed == true` ⇒ the `Play` command errored. We
    //     drop `state.current` and pop the queue row, but we DON'T write a
    //     history row (the song was never actually heard).
    //   - `snap.last_play_failed == false` & `!snap.playing` & some time
    //     elapsed ⇒ natural end-of-track. Record history with the elapsed
    //     time and apply repeat-mode rules.
    if state.current.is_some() && !snap.audio_pending_play {
        if snap.last_play_failed {
            let prev = state.current.take();
            state.popped_for_current = false;
            let _ = state.pop_played(None); // drop queue row, no history
            if let Some(p) = prev {
                debug!("audio: {} never actually played (setup failed)", p.name);
                apply_repeat_after_end(&mut state, &p);
            }
        } else if !snap.playing {
            if let Some(prev) = state.current.take() {
                let played_secs = snap.elapsed_secs();
                debug!(
                    "Playback finished for {} (~{played_secs:.1}s)",
                    prev.name
                );
                let _ = state.pop_played(Some(played_secs));
                apply_repeat_after_end(&mut state, &prev);
                state.popped_for_current = false;
            }
        }
    }

    // 2) Pop the queue row for the song we just kicked off, but only once
    //    the audio thread confirmed it actually got the sink running. Until
    //    then, we risk a double-pop if the play failed.
    if state.current.is_some() && snap.playing && !state.popped_for_current {
        let _ = state.pop_played(None); // drop queue row only, no history
        state.popped_for_current = true;
    }

    // 3) Start the next song if the queue has one and the audio engine is
    //    idle (idle = not playing AND not mid-setup).
    if !state.queue.is_empty()
        && !snap.playing
        && !snap.audio_pending_play
        && state.current.is_none()
    {
        let next = state.queue[0].clone();
        debug!("Starting playback: {} ({})", next.name, next.path);
        state.current = Some(next.clone());
        state.popped_for_current = false;
        state.audio_handle.send(crate::audio::AudioCmd::Play(next.path.clone()));
    }
}

/// Apply repeat-mode rules after a track has ended and `pop_played` has
/// already removed it from the queue.
fn apply_repeat_after_end(state: &mut DaemonState, prev: &Song) {
    match state.repeat_mode {
        RepeatMode::Off => {
            // `cycle_snapshot` is unused for Off; no need to update it.
        }
        RepeatMode::One => {
            state.queue.push(prev.clone());
            if let Err(e) = append_queue_row(state, prev.id) {
                warn!("failed to re-queue {} for repeat-one: {e}", prev.name);
            }
            state.cycle_snapshot = state.queue.clone();
        }
        RepeatMode::All => {
            if state.queue.is_empty() && !state.cycle_snapshot.is_empty() {
                info!(
                    "Repeat-all: refilling queue from snapshot ({} songs)",
                    state.cycle_snapshot.len()
                );
                let snapshot = state.cycle_snapshot.clone();
                if let Err(e) = refill_queue(state, &snapshot) {
                    warn!("failed to refill queue for repeat-all: {e}");
                }
                state.queue = snapshot;
            } else {
                state.cycle_snapshot = state.queue.clone();
            }
        }
    }
}

fn append_queue_row(state: &DaemonState, song_id: i64) -> rusqlite::Result<()> {
    let conn = state.conn.lock().unwrap();
    let next: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(position), 0) + 1 FROM queue",
            [],
            |row| row.get(0),
        )
        .unwrap_or(1);
    conn.execute(
        "INSERT INTO queue (position, song_id) VALUES (?1, ?2)",
        rusqlite::params![next, song_id],
    )?;
    Ok(())
}

fn refill_queue(state: &DaemonState, snapshot: &[Song]) -> rusqlite::Result<()> {
    let conn = state.conn.lock().unwrap();
    conn.execute("DELETE FROM queue", [])?;
    for s in snapshot {
        let next: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(position), 0) + 1 FROM queue",
                [],
                |row| row.get(0),
            )
            .unwrap_or(1);
        conn.execute(
            "INSERT INTO queue (position, song_id) VALUES (?1, ?2)",
            rusqlite::params![next, s.id],
        )?;
    }
    Ok(())
}
