//! Library scanner.
//!
//! `scan` walks every song row, recognizes the ones with missing/partial
//! metadata, and leaves already-complete rows alone. `scan --force` runs the
//! full pipeline again for every song but still merges conservatively: no
//! reliable match means existing metadata and source data stay untouched.
//!
//! The scan runs on its own thread and reports live progress through a
//! process-global status struct. Cancelling sets a flag checked between
//! songs, so successfully processed songs are never rolled back. An aborted
//! scan can simply be restarted: rows already marked `recognized` are
//! skipped by the next non-force run.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::metadata::{self, RecognizeOutcome};
use crate::state::{song_from_row, Song};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanResult {
    pub total: i64,
    pub already_complete: i64,
    pub missing: i64,
    pub scanned: i64,
    pub recognized: i64,
    pub not_recognized: i64,
    pub skipped: i64,
    pub updated_database: i64,
    pub updated_files: i64,
    pub errors: i64,
    pub cancelled: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanStatus {
    pub running: bool,
    pub finished: bool,
    pub cancelled: bool,
    pub force: bool,
    pub total: i64,
    pub already_complete: i64,
    pub missing: i64,
    pub processed: i64,
    pub recognized: i64,
    pub not_recognized: i64,
    pub skipped: i64,
    pub updated_database: i64,
    pub updated_files: i64,
    pub errors: i64,
    pub current_song: Option<String>,
    pub current_step: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub result: Option<ScanResult>,
    pub message: Option<String>,
}

struct ScanHandle {
    status: Mutex<ScanStatus>,
    cancel: AtomicBool,
}

static SCAN: OnceLock<Arc<ScanHandle>> = OnceLock::new();

fn handle() -> &'static Arc<ScanHandle> {
    SCAN.get_or_init(|| {
        Arc::new(ScanHandle {
            status: Mutex::new(ScanStatus::default()),
            cancel: AtomicBool::new(false),
        })
    })
}

fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn lock_status(h: &ScanHandle) -> std::sync::MutexGuard<'_, ScanStatus> {
    h.status.lock().unwrap_or_else(|p| p.into_inner())
}

/// Current progress snapshot.
pub fn status() -> ScanStatus {
    lock_status(handle()).clone()
}

/// Ask a running scan to stop after the current song.
pub fn cancel() -> bool {
    let h = handle();
    let running = lock_status(h).running;
    if running {
        h.cancel.store(true, Ordering::SeqCst);
        let mut st = lock_status(h);
        st.current_step = Some("Cancelling…".to_string());
    }
    running
}

/// Start a scan. Returns the initial status. Errors only when a scan is
/// already running, the library cannot be read, or the worker cannot spawn.
pub fn start(force: bool) -> Result<ScanStatus, String> {
    let h = handle();
    {
        let st = lock_status(h);
        if st.running {
            return Err("a scan is already running".to_string());
        }
    }

    let conn = metadata::metadata_conn()?;
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM songs", [], |r| r.get(0))
        .map_err(|e| format!("DB count: {e}"))?;
    let complete: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM songs
             WHERE metadata_status = 'recognized'
               AND title IS NOT NULL AND TRIM(title) != ''
               AND artist IS NOT NULL AND TRIM(artist) != ''
               AND (musicbrainz_recording_id IS NOT NULL OR acoustid_id IS NOT NULL)",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("DB count: {e}"))?;
    drop(conn);

    h.cancel.store(false, Ordering::SeqCst);
    let initial = ScanStatus {
        running: true,
        force,
        total,
        already_complete: complete,
        missing: (total - complete).max(0),
        current_step: Some("Starting library scan…".to_string()),
        started_at: Some(unix_secs()),
        message: if force {
            Some("Force rescan: all songs are re-analyzed".to_string())
        } else {
            None
        },
        ..Default::default()
    };
    *lock_status(h) = initial.clone();

    let handle = Arc::clone(h);
    std::thread::Builder::new()
        .name("sjnmusic-scan".to_string())
        .spawn(move || run_scan(handle, force))
        .map_err(|e| {
            let mut st = lock_status(h);
            st.running = false;
            st.finished = true;
            st.message = Some(format!("could not start scan worker: {e}"));
            format!("could not start scan worker: {e}")
        })?;

    Ok(initial)
}

fn run_scan(h: Arc<ScanHandle>, force: bool) {
    let songs: Vec<Song> = match metadata::metadata_conn().and_then(|conn| {
        let mut stmt = conn
            .prepare("SELECT * FROM songs ORDER BY id")
            .map_err(|e| format!("DB prepare: {e}"))?;
        let rows = stmt
            .query_map([], song_from_row)
            .map_err(|e| format!("DB query: {e}"))?
            .filter_map(Result::ok)
            .collect();
        Ok(rows)
    }) {
        Ok(rows) => rows,
        Err(e) => {
            warn!("scan: {e}");
            let mut st = lock_status(&h);
            st.running = false;
            st.finished = true;
            st.finished_at = Some(unix_secs());
            st.message = Some(e);
            return;
        }
    };

    // Read both counters under one short-lived guard. Taking two
    // `lock_status` guards in the same expression would deadlock this
    // non-reentrant mutex (the first temporary lives until the end of the
    // statement), freezing every later `/metadata/scan` status request.
    let (already_complete, missing) = {
        let st = lock_status(&h);
        (st.already_complete, st.missing)
    };
    info!(
        "Library scan started: {} songs, {already_complete} already complete, {missing} missing{}",
        songs.len(),
        if force { " (force)" } else { "" }
    );

    let mut cancelled = false;
    for song in songs {
        if h.cancel.load(Ordering::SeqCst) {
            cancelled = true;
            break;
        }
        {
            let mut st = lock_status(&h);
            st.current_song = Some(song.name.clone());
            st.current_step = Some(if force {
                "Force re-analysis…".to_string()
            } else {
                "Checking metadata…".to_string()
            });
        }

        if !force && song.is_metadata_complete() {
            let mut st = lock_status(&h);
            st.processed += 1;
            st.skipped += 1;
            continue;
        }

        {
            let mut st = lock_status(&h);
            st.current_step = Some("Fingerprint / AcoustID / MusicBrainz…".to_string());
        }
        let outcome: Result<RecognizeOutcome, String> = metadata::recognize_song(song.id, force);
        let mut st = lock_status(&h);
        st.processed += 1;
        match outcome {
            Ok(o) => {
                if o.is_recognized() {
                    st.recognized += 1;
                } else if o.status == "error" {
                    st.errors += 1;
                } else if o.status == "skipped" {
                    st.skipped += 1;
                } else {
                    st.not_recognized += 1;
                }
                if o.database_updated {
                    st.updated_database += 1;
                }
                if o.file_updated {
                    st.updated_files += 1;
                }
                if o.file_error.is_some() {
                    st.errors += 1;
                }
            }
            Err(e) => {
                warn!("scan: song {} ({}) failed: {e}", song.id, song.name);
                st.errors += 1;
            }
        }
    }

    let finished_at = unix_secs();
    let mut st = lock_status(&h);
    st.running = false;
    st.finished = true;
    st.cancelled = cancelled;
    st.finished_at = Some(finished_at);
    st.current_song = None;
    st.current_step = Some(if cancelled {
        "Scan cancelled".to_string()
    } else {
        "Scan completed".to_string()
    });
    st.result = Some(ScanResult {
        total: st.total,
        already_complete: st.already_complete,
        missing: st.missing,
        scanned: st.processed,
        recognized: st.recognized,
        not_recognized: st.not_recognized,
        skipped: st.skipped,
        updated_database: st.updated_database,
        updated_files: st.updated_files,
        errors: st.errors,
        cancelled,
    });
    info!(
        "Library scan {}: scanned={} recognized={} not_recognized={} skipped={} errors={} db={} files={}",
        if cancelled { "cancelled" } else { "completed" },
        st.processed,
        st.recognized,
        st.not_recognized,
        st.skipped,
        st.errors,
        st.updated_database,
        st.updated_files
    );
}
