use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::{protocol::Command, state::DaemonState};

pub fn start_socket(state: Arc<Mutex<DaemonState>>) {
    let _ = std::fs::remove_file("/tmp/sjnmusic.sock");

    let listener = UnixListener::bind("/tmp/sjnmusic.sock").unwrap();

    for stream in listener.incoming() {
        let state = state.clone();

        thread::spawn(move || {
            let mut stream = stream.unwrap();

            let mut buf = [0; 2048];
            let size = stream.read(&mut buf).unwrap();

            let msg = String::from_utf8_lossy(&buf[..size]);

            if let Ok(cmd) = serde_json::from_str::<Command>(&msg) {
                let response = handle(cmd, state);
                let _ = stream.write_all(response.as_bytes());
            } else {
                let _ = stream.write_all(b"ERROR: Invalid JSON");
            }
        });
    }
}

fn handle(cmd: Command, state: Arc<Mutex<DaemonState>>) -> String {
    let mut state = state.lock().unwrap();

    match cmd {
        Command::Play(query) => {
            state.play(&query);
            format!("OK: Playing {}", query)
        }
        Command::Add(query) => {
            state.add(&query);
            format!("OK: Added {}", query)
        }
        Command::Del(query) => {
            state.delete(&query);
            format!("OK: Deleted {}", query)
        }
        Command::Init(song) => {
            state.init(song.clone());
            format!("OK: Downloading {}", song)
        }
        Command::Search(query) => {
            if let Some(song) = state.search(&query) {
                serde_json::to_string(&song).unwrap_or_else(|_| "ERROR: Serialization failed".to_string())
            } else {
                "ERROR: Song not found".to_string()
            }
        }
    }
}
