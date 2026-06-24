//! MPRIS v2 service exposing the daemon's playback state and accepting
//! basic transport commands. Lives on the session D-Bus under
//! `org.mpris.MediaPlayer2.sjnmusic`.
//!
//! CURRENT STATE (v1): a working `org.mpris.MediaPlayer2` root interface
//! is registered so Linux desktops can discover the player (media-key
//! daemons will see "sjnmusic" exists in the system). The full Player
//! interface with playback_status / loop_status / volume / metadata is
//! tracked as a follow-up; the auto-property naming-detection in zbus 4
//! plus the OwnedValue/Path APIs need additional iteration. The HTTP
//! `/now-playing` endpoint already exposes full state to the GUI.
//!
//! Built on top of `zbus` 4 and a single-threaded tokio runtime hosted
//! inside this thread — the rest of the daemon stays purely synchronous.
//! Linux-only. If no session bus is reachable (no DBus, container, etc.),
//! the thread logs a warning and exits without disturbing anything else.

use std::sync::{Arc, Mutex};
use std::thread;

use log::{info, warn};
use zbus::ConnectionBuilder;
use zbus_macros::interface;

use crate::audio::AudioHandle;
use crate::state::DaemonState;

const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";

pub fn spawn(state: Arc<Mutex<DaemonState>>, audio: Arc<AudioHandle>) {
    thread::Builder::new()
        .name("sjnmusic-mpris".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    warn!("MPRIS: failed to build tokio runtime: {e}");
                    return;
                }
            };
            if let Err(e) = rt.block_on(run(Arc::clone(&state), Arc::clone(&audio))) {
                warn!("MPRIS: thread exited with error: {e:?}");
                // Don't panic the daemon: failure to register MPRIS must
                // never take down the audio engine.
            }
        })
        .expect("failed to spawn mpris thread");
}

async fn run(state: Arc<Mutex<DaemonState>>, audio: Arc<AudioHandle>) -> zbus::Result<()> {
    let _ = state;
    let _ = audio;

    let conn = ConnectionBuilder::session()?
        .name("org.mpris.MediaPlayer2.sjnmusic")?
        .build()
        .await?;

    // Root interface only so desktops discover the player identity. The
    // org.mpris.MediaPlayer2.Player interface (with playback_status,
    // volume, loop_status, metadata) was started in this module but
    // pulled back to a stub: zbus 4.4's auto-property-detection naming
    // convention plus the OwnedValue/ObjectPath constructors need more
    // careful work than fits in v1. We keep the type in the tree so the
    // GUI and HTTP endpoints stay the canonical control surface, and we
    // re-add the Player interface in the follow-up.
    let root = Root {};
    conn.object_server().at(OBJECT_PATH, root).await?;

    info!("MPRIS: registered at {OBJECT_PATH} (root interface only)");

    // Park the mpris thread in a tokio loop so system messages get
    // dispatched. We don't emit property changes from this thread; the
    // HTTP /now-playing endpoint is the GUI's source of truth.
    let keepalive = conn.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let _ = keepalive; // keep the connection alive
        }
    });

    // Park this task too; the connection is owned by `_conn`. zbus
    // handles messages on its own threads once registered.
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    }
}

#[derive(Clone)]
struct Root {}

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    async fn identity(&self) -> String {
        String::from("sjnmusic")
    }

    async fn desktop_entry(&self) -> String {
        String::from("sjnmusic")
    }

    async fn supported_uri_schemes(&self) -> Vec<String> {
        vec![
            String::from("file"),
            String::from("http"),
            String::from("https"),
        ]
    }

    async fn supported_mime_types(&self) -> Vec<String> {
        vec![String::from("audio/mpeg"), String::from("audio/mp3")]
    }

    async fn quit(&self) -> zbus::fdo::Result<()> {
        Ok(())
    }

    async fn raise(&self) -> zbus::fdo::Result<()> {
        Ok(())
    }

    /// We don't expose a playlist of multiple tracks (yet); saying `false`
    /// here keeps MPRIS clients from asking for one.
    async fn has_track_list(&self) -> bool {
        false
    }

    /// Real metadata lives behind the HTTP /now-playing endpoint; static
    /// placeholder keeps the introspection tree well-formed.
    async fn loop_status(&self) -> String {
        String::from("None")
    }

    async fn rate(&self) -> f64 {
        1.0
    }

    async fn minimum_rate(&self) -> f64 {
        1.0
    }

    async fn maximum_rate(&self) -> f64 {
        1.0
    }
}
