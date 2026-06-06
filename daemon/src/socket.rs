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
                handle(cmd, state);
            }

            let _ = stream.write_all(b"OK");
        });
    }
}

fn handle(cmd: Command, state: Arc<Mutex<DaemonState>>) {
    let mut state = state.lock().unwrap();

    match cmd {
        Command::Play(song) => state.play(song),
        Command::Add(song) => state.add(song),
        Command::Del(song) => state.delete(song),
        Command::Init(song) => state.init(song),
    }
}
