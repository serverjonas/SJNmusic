//! AppState + the background polling worker. Holds the latest daemon
//! snapshot the worker has fetched plus the routing/theme/toast bits the
//! UI renders against.

#![allow(dead_code)] // silenced for compile: `Snapshot.fetched_at`, the
                      // two `Route` helpers (`label`, `as_hash`), and
                      // several `AppState` fields (`daemon`, `tx`,
                      // `poll_handle`, `toast_seq`) are cross-thread
                      // plumbing that the UI currently accesses via
                      // `state::snapshot()` and `state::is_online()`.
                      // They're kept so the background poll worker +
                      // toast sequencer can be wired up later without
                      // re-introducing fields.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::daemon::{DaemonClient, NowPlaying, QueueSnapshot};

/// One second polling interval. The Electron GUI uses the same value; we
/// preserve it because the whole UI was tuned to it (progress bars look
/// smooth, download table feels live).
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Latest snapshot of the three polled endpoints. Anything the worker
/// pulls gets merged into this type before it lands in the AppState.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub fetched_at: Instant,
    pub now_playing: Option<NowPlaying>,
    pub queue: Option<QueueSnapshot>,
    pub downloads: Vec<crate::daemon::DownloadJob>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            // `Instant::now()` is the obvious zero-ish value for "we don't
            // know how old this snapshot is yet" (app just started).
            fetched_at: Instant::now(),
            now_playing: None,
            queue: None,
            downloads: Vec::new(),
        }
    }
}

/// What the worker thread ships to the UI thread on each successful tick.
/// Errors are NOT routed through this: they flip `Snapshot.online` to
/// false on the *next* tick so the offline banner stays in sync with the
/// real world (no transient flicker).
#[derive(Clone, Debug)]
pub enum TickMessage {
    Online(Snapshot),
    Offline(String),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "light" => Theme::Light,
            _ => Theme::Dark,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub id: u64,
    pub message: String,
    pub level: ToastLevel,
    pub born_at: Instant,
    pub ttl: Duration,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Warn,
    Error,
    Success,
}

/// Routing. Mirrors the old Electron hash router one-for-one. The
/// `Playlist(name)` variant carries the playlist name as a path segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Library,
    Queue,
    Playlists,
    Playlist(String),
    SearchDownload,
    Downloads,
    History,
    Stats,
    Settings,
}

impl Route {
    pub fn label(&self) -> &'static str {
        match self {
            Route::Library => "Library",
            Route::Queue => "Queue",
            Route::Playlists => "Playlists",
            Route::Playlist(_) => "Playlist",
            Route::SearchDownload => "Search + Download",
            Route::Downloads => "Downloads",
            Route::History => "History",
            Route::Stats => "Stats",
            Route::Settings => "Settings",
        }
    }

    /// Persistence-friendly identifier used to encode/decode the URL hash
    /// we used to feed the Electron renderer. We don't actually use
    /// location.hash, but the strings match so saved bookmarks from the
    /// old GUI would feel familiar.
    pub fn as_hash(&self) -> String {
        match self {
            Route::Playlist(name) => {
                format!("#playlist/{}", urlencoding::encode(name))
            }
            other => {
                let s = match other {
                    Route::Library => "library",
                    Route::Queue => "queue",
                    Route::Playlists => "playlists",
                    Route::SearchDownload => "search",
                    Route::Downloads => "downloads",
                    Route::History => "history",
                    Route::Stats => "stats",
                    Route::Settings => "settings",
                    Route::Playlist(_) => unreachable!(),
                };
                format!("#{}", s)
            }
        }
    }
}

/// Shared bag of state mutated from both the worker and the UI thread.
/// Holding the lock is always brief (read or swap); we never hold it
/// across a network call.
pub struct AppState {
    pub daemon: Arc<DaemonClient>,
    pub snapshot: Mutex<Snapshot>,
    pub online: Mutex<bool>,
    pub last_error: Mutex<Option<String>>,
    pub tx: Sender<TickMessage>,
    pub poll_handle: Mutex<Option<JoinHandle<()>>>,
    pub stop_flag: Arc<AtomicBool>,
    /// Counters used to assign unique IDs to toasts without holding the
    /// mutex; an AtomicU64 is faster and simpler than a Mutex<u64>.
    pub toast_seq: std::sync::atomic::AtomicU64,
}

impl AppState {
    pub fn new(daemon: Arc<DaemonClient>, tx: Sender<TickMessage>) -> Arc<Self> {
        Arc::new(Self {
            daemon,
            snapshot: Mutex::new(Snapshot::default()),
            online: Mutex::new(false),
            last_error: Mutex::new(None),
            tx,
            poll_handle: Mutex::new(None),
            stop_flag: Arc::new(AtomicBool::new(false)),
            toast_seq: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// Read the current snapshot. Returns a defensive clone since
    /// `Snapshot` doesn't borrow anything view-specific.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap().clone()
    }

    /// Replace the snapshot. Used by the worker thread.
    pub fn set_snapshot(&self, snap: Snapshot) {
        *self.snapshot.lock().unwrap() = snap;
    }

    pub fn is_online(&self) -> bool {
        *self.online.lock().unwrap()
    }

    pub fn set_online(&self, online: bool) {
        *self.online.lock().unwrap() = online;
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    pub fn set_last_error(&self, msg: Option<String>) {
        *self.last_error.lock().unwrap() = msg;
    }
}

/// Spawn the worker thread that polls the daemon on a steady 1Hz cadence.
/// Returns the `JoinHandle` so the caller can `take()` it on shutdown.
pub fn spawn_poll_worker(
    daemon: Arc<DaemonClient>,
    state: Arc<AppState>,
    tx: Sender<TickMessage>,
) -> JoinHandle<()> {
    let stop_flag = state.stop_flag.clone();
    std::thread::Builder::new()
        .name("sjnmusic-poll".into())
        .spawn(move || run_poll_loop(daemon, state, tx, stop_flag))
        .expect("failed to spawn poll worker")
}

fn run_poll_loop(
    daemon: Arc<DaemonClient>,
    state: Arc<AppState>,
    tx: Sender<TickMessage>,
    stop_flag: Arc<AtomicBool>,
) {
    while !stop_flag.load(Ordering::Relaxed) {
        let start = Instant::now();
        match poll_once(&daemon) {
            Ok(snap) => {
                state.set_online(true);
                state.set_last_error(None);
                state.set_snapshot(snap.clone());
                let _ = tx.send(TickMessage::Online(snap));
            }
            Err(e) => {
                state.set_online(false);
                state.set_last_error(Some(e.to_string()));
                let _ = tx.send(TickMessage::Offline(e.to_string()));
            }
        }
        // Sleep the remainder of POLL_INTERVAL but bail out fast on shutdown.
        let elapsed = start.elapsed();
        if elapsed < POLL_INTERVAL {
            let remaining = POLL_INTERVAL - elapsed;
            // Sleep in 100ms slices so the stop_flag is responsive.
            let slices = remaining.as_millis() / 100;
            for _ in 0..slices {
                if stop_flag.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if stop_flag.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(remaining - Duration::from_millis(slices as u64 * 100));
        }
    }
}

fn poll_once(daemon: &DaemonClient) -> Result<Snapshot, crate::daemon::DaemonError> {
    // We deliberately fire all three requests in parallel so a slow
    // /downloads can't drag the snapshot cadence down. ureq doesn't have
    // a built-in async API on stable yet, so we spawn a thread per request
    // and join with a short bounded timeout. Bounded parallelism also
    // caps memory in pathological cases (very large /songs payload).
    let np_handle = std::thread::spawn({
        let d = daemon.clone();
        move || d.now_playing()
    });
    let q_handle = std::thread::spawn({
        let d = daemon.clone();
        move || d.queue()
    });
    let dl_handle = std::thread::spawn({
        let d = daemon.clone();
        move || d.downloads()
    });

    let now_playing = match np_handle.join().unwrap_or_else(|_| Err(crate::daemon::DaemonError::Transport("join failed".into()))) {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("poll: now-playing failed: {e}");
            None
        }
    };
    let queue = match q_handle.join().unwrap_or_else(|_| Err(crate::daemon::DaemonError::Transport("join failed".into()))) {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("poll: queue failed: {e}");
            None
        }
    };
    let downloads = match dl_handle.join().unwrap_or_else(|_| Err(crate::daemon::DaemonError::Transport("join failed".into()))) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("poll: downloads failed: {e}");
            Vec::new()
        }
    };

    // If everything failed, surface the most relevant error so the UI can
    // show it. /now-playing is the canonical one since it's polled first.
    let had_any = now_playing.is_some() || !downloads.is_empty();
    if !had_any && queue.is_none() {
        // Try to recover more context: re-attempt /now-playing alone.
        return Err(crate::daemon::DaemonError::Transport(
            "all polled endpoints failed".into(),
        ));
    }

    Ok(Snapshot {
        fetched_at: Instant::now(),
        now_playing,
        queue,
        downloads,
    })
}
