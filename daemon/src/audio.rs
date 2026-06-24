//! Audio playback lives on a dedicated thread because rodio's `OutputStream`
//! is `!Send` (cpal wraps a non-Send platform stream). We expose a
//! `Send + Sync` `AudioHandle` to callers via:
//!   - `mpsc::Sender<AudioCmd>` for commands (Play, Pause, Resume, ...)
//!   - `Arc<Mutex<AudioStatus>>` for read snapshots (volume, paused,
//!     elapsed/duration, ...)
//!
//! The audio thread is the sole writer of `AudioStatus`; readers (daemon
//! tick, HTTP handlers) just clone or peek.

use std::fs::File;
use std::io::BufReader;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use rodio::{Decoder, OutputStream, Sink, Source};

/// Shared, `Send + Sync` view of the engine's state. Updated by the audio
/// thread on every command and on natural end-of-track detection. Readers
/// lock briefly to clone this struct.
#[derive(Default, Clone, Debug)]
pub struct AudioStatus {
    pub volume: f32,
    pub paused: bool,
    /// True iff an active `Sink` exists. Goes false on `Stop`, on natural
    /// end-of-track, or before the very first `Play`.
    pub playing: bool,
    /// Path of the file currently producing sound. `None` while idle and
    /// during the gap between two `Play` commands.
    pub current_path: Option<String>,
    /// Source-reported duration in seconds; `None` when unknown.
    pub duration_secs: Option<f64>,
    /// `Instant` of the most recent play-start or seek reset.
    pub play_started_at: Option<Instant>,
    /// Cumulative time spent paused during the current track, in seconds.
    pub paused_total_secs: f64,
    /// `Instant` of the current pause; `None` when not paused.
    pub paused_at: Option<Instant>,
    /// `true` between the moment a `Play` command is dequeued and the moment
    /// the audio thread finishes setting the sink up (success *or* failure).
    /// Lets the daemon tick distinguish "still working" from "decided".
    pub audio_pending_play: bool,
    /// Set `true` when a `Play` command completed by *failing* to open the
    /// output, file, decoder, etc. The daemon reads this to avoid writing a
    /// bogus 0-second history row for songs that never actually played.
    pub last_play_failed: bool,
}

impl AudioStatus {
    /// Time spent playing the current track so far, in seconds. Pauses are
    /// excluded via `paused_total_secs`.
    pub fn elapsed_secs(&self) -> f64 {
        let Some(started) = self.play_started_at else {
            return 0.0;
        };
        if let Some(paused_at) = self.paused_at {
            let raw = paused_at.duration_since(started).as_secs_f64();
            return (raw - self.paused_total_secs).max(0.0);
        }
        let raw = started.elapsed().as_secs_f64();
        (raw - self.paused_total_secs).max(0.0)
    }
}

/// Commands the audio thread processes FIFO.
#[derive(Debug)]
pub enum AudioCmd {
    Play(String),
    Pause,
    Resume,
    SetVolume(f32),
    Seek(f64),
    Stop,
    Shutdown,
}

/// Cheap, `Send + Sync` handle HTTP / daemon code use to talk to the audio
/// thread without holding the rodio `OutputStream` directly.
#[derive(Clone)]
pub struct AudioHandle {
    tx: Sender<AudioCmd>,
    status: Arc<Mutex<AudioStatus>>,
}

impl AudioHandle {
    /// Enqueue a command. Returns silently if the audio thread has already
    /// shut down — best-effort, idempotently safe because the daemon is
    /// shutting down in that case.
    pub fn send(&self, cmd: AudioCmd) {
        let _ = self.tx.send(cmd);
    }

    /// Take a consistent snapshot of the audio state. Cheap: the inner lock
    /// is held only long enough to clone a few primitives.
    pub fn snapshot(&self) -> AudioStatus {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Signal the audio thread to exit. Safe to call multiple times.
    pub fn shutdown(&self) {
        let _ = self.tx.send(AudioCmd::Shutdown);
    }
}

/// Spawn the audio thread and return its handle. Caller is responsible for
/// eventually calling `AudioHandle::shutdown` (or simply dropping the handle
/// sender end, which causes the thread to exit on its next `recv_timeout`).
pub fn spawn_audio_thread() -> AudioHandle {
    let (tx, rx) = mpsc::channel();
    let status = Arc::new(Mutex::new(AudioStatus {
        volume: 1.0,
        ..Default::default()
    }));
    let status_clone = Arc::clone(&status);
    thread::Builder::new()
        .name("sjnmusic-audio".into())
        .spawn(move || run_audio(rx, status_clone))
        .expect("failed to spawn audio thread");
    AudioHandle { tx, status }
}

/// Probe-only helper for download workers that aren't allowed to start
/// full playback. Returns `None` if the source doesn't expose duration.
pub fn probe_duration_secs(path: &str) -> Option<f64> {
    let file = File::open(path).ok()?;
    let source = Decoder::new(BufReader::new(file)).ok()?;
    source.total_duration().map(|d| d.as_secs_f64())
}

/// Audio thread body: owns the rodio `OutputStream` and `Sink`. Receives
/// commands from the channel and, when the channel is idle for 200 ms,
/// checks whether the track ended naturally so HTTP/daemon can react.
#[allow(unused_assignments)] // Some commands (Stop / Play) deliberately assign sink = None
                              // to drop the prior value before optionally reassigning; lint
                              // doesn't track Drop side effects.
fn run_audio(rx: Receiver<AudioCmd>, status: Arc<Mutex<AudioStatus>>) {
    let mut sink: Option<Sink> = None;
    let mut _stream: Option<OutputStream> = None;

    loop {
        // End-of-track detection. rodio's `Sink::empty()` is true iff the
        // source is exhausted AND playback is not paused. So while we are
        // paused, the check is harmless; once we resume and the buffer
        // drains, `empty()` flips to true and we clear `playing`.
        if !is_paused(&status) {
            let ended = sink.as_ref().map(|s| s.empty()).unwrap_or(false);
            if ended && status_snapshot_path(&status).is_some() {
                debug!("audio: track ended naturally");
                sink = None;
                _stream = None;
                let mut s = status.lock().unwrap();
                s.playing = false;
                s.paused = false;
                s.current_path = None;
                s.play_started_at = None;
                s.paused_at = None;
                s.paused_total_secs = 0.0;
            }
        }

        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(cmd) => match cmd {
                AudioCmd::Play(path) => {
                    // Drop any prior stream/sink before opening a new one
                    // so the audio device isn't held across calls.
                    sink = None;
                    _stream = None;
                    {
                        let mut s = status.lock().unwrap();
                        s.playing = false;
                        s.paused = false;
                        s.current_path = None;
                        s.play_started_at = None;
                        s.paused_at = None;
                        s.paused_total_secs = 0.0;
                        // Mark "we're working on a Play" so the daemon tick
                        // knows not to treat a transient `playing=false` as
                        // "song ended naturally".
                        s.audio_pending_play = true;
                        s.last_play_failed = false;
                    }

                    let vol = status.lock().unwrap().volume;
                    // Open device + sink + decoder in one place so success /
                    // failure book-keeping lives at the bottom of the block.
                    let result: Result<(), String> = (|| {
                        let (stream, handle) = OutputStream::try_default()
                            .map_err(|e| format!("output: {e}"))?;
                        let sink_obj =
                            Sink::try_new(&handle).map_err(|e| format!("sink: {e}"))?;
                        let file = File::open(&path)
                            .map_err(|e| format!("file: {e}"))?;
                        let source = Decoder::new(BufReader::new(file))
                            .map_err(|e| format!("decode: {e}"))?;
                        let dur = source
                            .total_duration()
                            .map(|d| d.as_secs_f64())
                            .unwrap_or(0.0);
                        sink_obj.set_volume(vol);
                        sink_obj.append(source);
                        sink = Some(sink_obj);
                        _stream = Some(stream);
                        let mut s = status.lock().unwrap();
                        s.current_path = Some(path.clone());
                        s.duration_secs = Some(dur);
                        s.playing = true;
                        s.paused = false;
                        s.play_started_at = Some(Instant::now());
                        s.paused_at = None;
                        s.paused_total_secs = 0.0;
                        debug!("audio: started {path} (~{dur:.1}s)");
                        Ok(())
                    })();

                    {
                        let mut s = status.lock().unwrap();
                        s.audio_pending_play = false;
                        if let Err(e) = result {
                            // Don't leave the daemon thinking a song is
                            // "playing" — flip the explicit failure flag so
                            // the next tick can clean up state.current
                            // without writing a bogus history row.
                            s.last_play_failed = true;
                            warn!("audio: setup failed for {path}: {e}");
                            s.playing = false;
                            s.current_path = None;
                            s.play_started_at = None;
                        }
                    }
                }
                AudioCmd::Pause => {
                    if let Some(s) = sink.as_ref() {
                        let already_paused = {
                            let st = status.lock().unwrap();
                            st.paused
                        };
                        if !already_paused {
                            s.pause();
                            let mut st = status.lock().unwrap();
                            st.paused = true;
                            st.paused_at = Some(Instant::now());
                            debug!("audio: paused");
                        }
                    } else {
                        warn!("audio: pause requested but nothing playing");
                    }
                }
                AudioCmd::Resume => {
                    if let Some(s) = sink.as_ref() {
                        let mut st = status.lock().unwrap();
                        if st.paused {
                            if let Some(p) = st.paused_at.take() {
                                st.paused_total_secs += p.elapsed().as_secs_f64();
                            }
                            drop(st);
                            s.play();
                            status.lock().unwrap().paused = false;
                            debug!("audio: resumed");
                        }
                    } else {
                        warn!("audio: resume requested but nothing playing");
                    }
                }
                AudioCmd::SetVolume(v) => {
                    let v_clamped = v.clamp(0.0, 2.0);
                    if let Some(s) = sink.as_ref() {
                        s.set_volume(v_clamped);
                    }
                    status.lock().unwrap().volume = v_clamped;
                    debug!("audio: volume -> {v_clamped}");
                }
                AudioCmd::Seek(secs) => {
                    if let Some(s) = sink.as_ref() {
                        if !secs.is_finite() || secs < 0.0 {
                            warn!("audio: invalid seek {secs}");
                        } else {
                            let d = Duration::from_secs_f64(secs);
                            match s.try_seek(d) {
                                Ok(()) => {
                                    let mut st = status.lock().unwrap();
                                    st.play_started_at = Some(Instant::now());
                                    st.paused_total_secs = 0.0;
                                    if st.paused {
                                        st.paused_at = Some(Instant::now());
                                    }
                                    debug!("audio: seeked to {secs:.2}s");
                                }
                                Err(e) => warn!("audio: seek failed: {e:?}"),
                            }
                        }
                    } else {
                        warn!("audio: seek requested but nothing playing");
                    }
                }
                AudioCmd::Stop => {
                    if sink.is_some() {
                        debug!("audio: stop");
                    }
                    sink = None;
                    _stream = None;
                    let mut s = status.lock().unwrap();
                    s.playing = false;
                    s.paused = false;
                    s.current_path = None;
                    s.play_started_at = None;
                    s.paused_at = None;
                    s.paused_total_secs = 0.0;
                }
                AudioCmd::Shutdown => {
                    info!("audio: shutdown");
                    sink = None;
                    _stream = None;
                    break;
                }
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                warn!("audio: command channel disconnected");
                break;
            }
        }
    }
}

fn is_paused(status: &Arc<Mutex<AudioStatus>>) -> bool {
    status.lock().unwrap().paused
}

fn status_snapshot_path(status: &Arc<Mutex<AudioStatus>>) -> Option<String> {
    status.lock().unwrap().current_path.clone()
}
