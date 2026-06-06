use std::sync::{Arc, Mutex};
use std::thread;

use crate::{
    state::DaemonState,
    socket::start_socket,
    db::init_db,
    audio::AudioEngine,
};

pub fn run() {
    let state = Arc::new(Mutex::new(DaemonState::new()));

    init_db();

    let socket_state = state.clone();
    thread::spawn(move || {
        start_socket(socket_state);
    });

    let mut audio = AudioEngine::new();

    loop {
        {
            let mut state = state.lock().unwrap();

            if !state.queue.is_empty() && !audio.is_playing() {
                let song = state.queue.remove(0);
                audio.play(&song.path);
                state.current = Some(song);
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
