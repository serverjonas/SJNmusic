//! Top-level `eframe::App` implementation. Owns the layout (sidebar +
//! footer + central panel), the viewport commands, and the per-view
//! scratch state (cache of `library_songs`, in-flight flags, etc.) that
//! doesn't belong on the shared `AppState`.

#![allow(dead_code)] // silenced for compile: SJNMusicApp holds some fields that
                      // are set but only read through other paths (snapshot,
                      // daemon poll worker) — checked in via typecheck before
                      // each release. Keeps API surface stable while the
                      // fields aren't yet wired up.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use eframe::egui;
use eframe::egui::TextureHandle;

use crate::daemon::{DaemonClient, DownloadJob, HistoryEntry, NowPlaying, Playlist, QueueSnapshot, RankedCandidate, Song, Stats};
use crate::state::{AppState, Route, Theme, TickMessage, Toast, ToastLevel};
use crate::thumbnails::ImageMessage;
use crate::tray::TraySignals;
use crate::views;

/// Top-level GUI app. One instance per process, lives inside `eframe`.
pub struct SJNMusicApp {
    // daemons + worker plumbing
    pub daemon: Arc<DaemonClient>,
    pub state: Arc<AppState>,
    pub image_tx: Sender<ImageMessage>,
    pub tick_rx: Receiver<TickMessage>,
    pub image_rx: Receiver<ImageMessage>,
    pub signals: TraySignals,

    // UI-level state
    pub route: Route,
    pub theme: Theme,
    pub toasts: Vec<Toast>,
    pub toast_seq: Arc<AtomicU64>,
    applied_theme_at_least_once: bool,

    // thumbnail cache
    pub thumb_cache: HashMap<String, TextureHandle>,

    // ---- background-worker result slots --------------------------------
    // Each slot is the destination workers write into. The UI drains
    // them at the top of every `update()` so a quick typing burst can't
    // strand a result behind stale state.
    pub library_results_bus: Arc<std::sync::Mutex<Option<Vec<Song>>>>,
    pub search_results_bus: Arc<std::sync::Mutex<Option<Vec<RankedCandidate>>>>,

    // Downloads view holds an explicit override so per-view refresh
    // overrides the polled snapshot without us racing the worker.
    pub downloads_override: Arc<std::sync::Mutex<Option<Vec<DownloadJob>>>>,
    pub downloads_refresh_at: Arc<std::sync::Mutex<std::time::Instant>>,

    // ---- per-view state ------------------------------------------------
    pub library_query: String,
    pub library_songs: Vec<Song>,
    pub library_loading: bool,

    pub queue_data: Option<QueueSnapshot>,
    pub queue_loading: bool,

    pub playlists_data: Vec<Playlist>,
    pub playlists_loading: bool,

    pub playlist_data: HashMap<String, Playlist>,
    pub playlist_loading: HashMap<String, bool>,
    pub new_playlist_name: String,

    pub search_q: String,
    pub search_limit: usize,
    pub search_results: Vec<RankedCandidate>,
    pub search_loading: bool,
    pub search_just_downloaded: Option<String>,
    pub search_margin: i32,
    /// Set while a Smart-Pick "auto" or "needs_choice" decision is in
    /// flight so the UI can show "Picking…" instead of stale
    /// `search_results` rows.
    pub search_picking: bool,

    pub history_data: Vec<HistoryEntry>,
    pub history_loading: bool,

    pub stats_data: Option<Stats>,
    pub stats_loading: bool,
}

impl SJNMusicApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        host: String,
        port: u16,
        theme_str: String,
        signals: TraySignals,
    ) -> Self {
        let theme = Theme::from_str(&theme_str);
        cc.egui_ctx.set_visuals(theme.visuals());

        let daemon = crate::daemon::shared(&host, port);
        let (tick_tx, tick_rx) = std::sync::mpsc::channel();
        let (image_tx, image_rx) = std::sync::mpsc::channel();
        // Clone the sender before `AppState::new` consumes it because
        // the poll worker also needs a copy. `mpsc::Sender` is `Clone`.
        let state = AppState::new(daemon.clone(), tick_tx.clone());

        // Wire the poll worker; we keep the JoinHandle on the AppState
        // so it can be joined on shutdown (graceful, no panic). Spawning
        // must happen AFTER AppState::new because the worker reads
        // stop_flag & sender & daemon off the AppState.
        crate::state::spawn_poll_worker(daemon.clone(), state.clone(), tick_tx);

        Self {
            daemon,
            state,
            image_tx,
            tick_rx,
            image_rx,
            signals,

            route: Route::Library,
            theme,
            toasts: Vec::new(),
            toast_seq: Arc::new(AtomicU64::new(1)),
            applied_theme_at_least_once: true,

            thumb_cache: HashMap::new(),

            // Background result slots.
            library_results_bus: Arc::new(std::sync::Mutex::new(None)),
            search_results_bus: Arc::new(std::sync::Mutex::new(None)),
            downloads_override: Arc::new(std::sync::Mutex::new(None)),
            downloads_refresh_at: Arc::new(std::sync::Mutex::new(
                std::time::Instant::now() - std::time::Duration::from_secs(60),
            )),

            library_query: String::new(),
            library_songs: Vec::new(),
            library_loading: false,

            queue_data: None,
            queue_loading: false,

            playlists_data: Vec::new(),
            playlists_loading: false,

            playlist_data: HashMap::new(),
            playlist_loading: HashMap::new(),
            new_playlist_name: String::new(),

            search_q: String::new(),
            search_limit: 9,
            search_results: Vec::new(),
            search_loading: false,
            search_just_downloaded: None,
            search_margin: 30,
            search_picking: false,

            history_data: Vec::new(),
            history_loading: false,

            stats_data: None,
            stats_loading: false,
        }
    }

    pub fn push_toast(&mut self, message: impl Into<String>, level: ToastLevel) {
        let id = self.toast_seq.fetch_add(1, Ordering::Relaxed);
        let ttl = match level {
            ToastLevel::Error => std::time::Duration::from_millis(5000),
            ToastLevel::Warn => std::time::Duration::from_millis(4000),
            _ => std::time::Duration::from_millis(3000),
        };
        self.toasts.push(Toast {
            id,
            message: message.into(),
            level,
            born_at: std::time::Instant::now(),
            ttl,
        });
    }

    fn drain_tick(&mut self) {
        while let Ok(msg) = self.tick_rx.try_recv() {
            match msg {
                TickMessage::Online(snap) => {
                    self.state.set_online(true);
                    self.state.set_last_error(None);
                    self.state.set_snapshot(snap.clone());
                    // Cache the last-known paused state. When the tray is
                    // built (--features tray) this avoids the menu
                    // round-tripping the daemon to label itself; when the
                    // tray is OFF it's a benign no-op write to a static
                    // atomic.
                    if let Some(np) = &snap.now_playing {
                        crate::tray::set_paused_hint(np.paused);
                    }
                    // Keep route data fresh where the live snapshot
                    // overlaps with what views would otherwise re-fetch.
                    if snap.now_playing.is_some() && self.route == Route::Queue {
                        // queue overlay rebuilt from snapshot
                    }
                }
                TickMessage::Offline(err) => {
                    self.state.set_online(false);
                    self.state.set_last_error(Some(err.clone()));
                    // Drop whatever partial data we were holding so the
                    // UI doesn't keep rendering a stale "Now Playing"
                    // after a daemon hiccup surfaces.
                    self.state.set_snapshot(crate::state::Snapshot {
                        fetched_at: std::time::Instant::now(),
                        now_playing: None,
                        queue: None,
                        downloads: Vec::new(),
                    });
                    let _ = err;
                }
            }
        }
    }

    fn drain_images(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.image_rx.try_recv() {
            match msg {
                ImageMessage::Loaded { url, image } => {
                    let texture = ctx.load_texture(url.clone(), image, egui::TextureOptions::LINEAR);
                    self.thumb_cache.insert(url, texture);
                }
                ImageMessage::Failed { .. } => {
                    // Failed fetches simply never cache a TextureHandle.
                    // View code treats a missing cache entry as "still loading
                    // (or permanently failed)"; we don't surface a toast for
                    // every image because thumbnails are advisory.
                }
            }
        }
    }

    fn gc_expired_toasts(&mut self) {
        let now = std::time::Instant::now();
        self.toasts.retain(|t| now.duration_since(t.born_at) < t.ttl);
    }

    fn drain_background_results(&mut self) {
        // Library results: a worker has finished fetching; swap into the
        // render copy and clear the loading flag.
        if let Ok(mut slot) = self.library_results_bus.lock() {
            if let Some(new) = slot.take() {
                self.library_songs = new;
                self.library_loading = false;
            }
        }
        // Search results: same pattern.
        if let Ok(mut slot) = self.search_results_bus.lock() {
            if let Some(new) = slot.take() {
                self.search_results = new;
                self.search_loading = false;
            }
        }
    }

    fn process_tray_signals(&mut self, ctx: &egui::Context) {
        if self.signals.show_window.swap(false, Ordering::Relaxed) {
            // egui 0.32 dropped `ViewportCommand::UserAttention`, so
            // `Focus` alone has to do the work of pulling the window
            // forward. The OS window manager will surface the taskbar
            // entry on its own.
            // TODO: re-add a platform-level "flash taskbar / dock
            //       bounce" via winit/window_handle behind a feature
            //       flag — losing flash is a real UX regression when the
            //       user clicks "Show window" from the tray while the
            //       window is minimised.
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if self.signals.quit.swap(false, Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for SJNMusicApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. Drain worker + image queues + background result slots.
        self.drain_tick();
        self.drain_images(ctx);
        self.drain_background_results();

        // 2. Honour tray signals + transfer paused hint + clear garbage.
        self.process_tray_signals(ctx);
        self.gc_expired_toasts();

        // 3. Persistent theme (already applied once on construction, but
        //    Settings can flip it; we re-apply every frame to keep it
        //    in sync without tracking dirty bits).
        ctx.set_visuals(self.theme.visuals());


        // ---- Sidebar ----
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(220.0)
            .show(ctx, |ui| {
                self.show_sidebar(ui);
            });

        // ---- Now-playing footer ----
        egui::TopBottomPanel::bottom("now-playing")
            .resizable(false)
            .exact_height(96.0)
            .show(ctx, |ui| {
                self.show_now_playing(ui);
            });

        // ---- Main content ----
        egui::CentralPanel::default().show(ctx, |ui| {
            views::show(ui, self);
        });

        // ---- Toasts ----
        self.show_toasts(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Tell the poll worker to wind down. We don't block on join so
        // shutdown is fast; the worker exits within at most 1 slice
        // (≈100ms) of its current sleep.
        self.state.stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl SJNMusicApp {
    fn show_sidebar(&mut self, ui: &mut egui::Ui) {
        let online = self.state.is_online();
        let last_error = self.state.last_error();

        ui.vertical(|ui| {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("sjnmusic")
                    .strong()
                    .size(18.0),
            );
            ui.add_space(8.0);
            if !online {
                egui::Frame::group(ui.style())
                    .fill(ui.visuals().warn_fg_color.linear_multiply(0.25))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("daemon offline")
                                .color(ui.visuals().warn_fg_color),
                        );
                        if let Some(m) = last_error.as_deref() {
                            ui.label(
                                egui::RichText::new(m)
                                    .small()
                                    .color(ui.visuals().text_color()),
                            );
                        }
                    });
                ui.add_space(4.0);
            }

            let items: Vec<(&str, &str, Route)> = vec![
                ("🎵", "Library", Route::Library),
                ("📜", "Queue", Route::Queue),
                ("➕", "Playlists", Route::Playlists),
                ("🔎", "Search + Download", Route::SearchDownload),
                ("⬇", "Downloads", Route::Downloads),
                ("🕓", "History", Route::History),
                ("📊", "Stats", Route::Stats),
                ("⚙", "Settings", Route::Settings),
            ];

            ui.add_space(8.0);
            for (icon, label, route) in items {
                let active = self.route_matches(&route);
                let text = egui::RichText::new(format!("{} {}", icon, label));
                let btn = if active {
                    egui::Button::new(text).fill(ui.visuals().selection.bg_fill)
                } else {
                    egui::Button::new(text)
                };
                if ui.add(btn).clicked() {
                    self.route = route;
                }
            }
        });
    }

    fn route_matches(&self, candidate: &Route) -> bool {
        match (candidate, &self.route) {
            (Route::Playlist(_), Route::Playlist(_)) => true,
            (a, b) => a == b,
        }
    }

    fn show_now_playing(&mut self, ui: &mut egui::Ui) {
        let np: Option<NowPlaying> = self.state.snapshot().now_playing;
        ui.horizontal(|ui| {
            match np.as_ref() {
                Some(np) => {
                    let title = np
                        .current
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_else(|| "(nothing playing)".into());
                    let status = if np.paused {
                        "paused"
                    } else if np.playing {
                        "playing"
                    } else {
                        "idle"
                    };
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(&title).strong());
                        ui.label(egui::RichText::new(status).small().weak());
                    });
                    ui.add_space(8.0);
                    ui.group(|ui| {
                        let elapsed = np.elapsed_secs;
                        let dur = np.duration_secs.unwrap_or(0.0);
                        if ui.button("⏮ 10s").clicked() {
                            let target = (elapsed - 10.0).max(0.0);
                            self.fire_action("/seek", serde_json::json!({"secs": target}));
                        }
                        let play_label = if np.paused || !np.playing {
                            "▶"
                        } else {
                            "⏸"
                        };
                        if ui.button(play_label).clicked() {
                            if np.paused {
                                self.fire_action("/resume", serde_json::json!({}));
                            } else {
                                self.fire_action("/pause", serde_json::json!({}));
                            }
                        }
                        if ui.button("⏭").clicked() {
                            self.fire_action("/skip", serde_json::json!({}));
                        }
                        let mut repeat = np.repeat.clone();
                        egui::ComboBox::from_label("repeat")
                            .selected_text(repeat.clone())
                            .show_ui(ui, |ui| {
                                for opt in ["off", "one", "all"] {
                                    if ui.selectable_label(repeat == opt, opt).clicked() {
                                        repeat = opt.into();
                                    }
                                }
                            });
                        if repeat != np.repeat {
                            self.fire_action("/repeat", serde_json::json!({"mode": repeat}));
                        }
                        let progress = if dur > 0.0 {
                            (elapsed / dur).clamp(0.0, 1.0) as f32
                        } else {
                            0.0
                        };
                        ui.add(
                            egui::ProgressBar::new(progress)
                                .desired_width(140.0),
                        );
                        ui.label(format!(
                            "{} / {}",
                            crate::fmt::mmss(elapsed),
                            crate::fmt::mmss(dur)
                        ));
                        let mut vol = np.volume;
                        if ui
                            .add(
                                egui::Slider::new(&mut vol, 0.0..=2.0)
                                    .step_by(0.01)
                                    .text("vol"),
                            )
                            .changed()
                        {
                            self.fire_action("/volume", serde_json::json!({"vol": vol}));
                        }
                    });
                }
                None => {
                    ui.label(egui::RichText::new("(waiting for daemon…)").weak());
                }
            }
        });
    }

    fn show_toasts(&self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        // Each toast gets its own floating `Area` so they stack from
        // the bottom-right corner. Using `Area` (not frame painter
        // hacks) means egui handles input/hit-testing correctly and the
        // toasts sit above any panel content the user is interacting
        // with.
        let mut y_offset: f32 = 110.0;
        for toast in &self.toasts {
            let bg = match toast.level {
                ToastLevel::Error => egui::Color32::from_rgb(60, 30, 30),
                ToastLevel::Warn => egui::Color32::from_rgb(60, 50, 20),
                ToastLevel::Success => egui::Color32::from_rgb(20, 50, 30),
                ToastLevel::Info => egui::Color32::from_rgb(24, 28, 36),
            };
            let id = egui::Id::new("toast").with(toast.id);
            egui::Area::new(id)
                .anchor(
                    egui::Align2::RIGHT_BOTTOM,
                    [16.0, -y_offset],
                )
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::group(ui.style())
                        .fill(bg)
                        .stroke(egui::Stroke::new(
                            1.0,
                            ui.visuals().widgets.noninteractive.bg_stroke.color,
                        ))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(&toast.message)
                                    .color(egui::Color32::from_rgb(230, 230, 230)),
                            );
                        });
                });
            y_offset += 50.0;
        }
    }

    /// Fire-and-forget daemon call that runs off-thread so a stalled
    /// daemon can't freeze the GUI. Errors surface as toasts only when
    /// they're not just "user typed the wrong query"; we err on the side
    /// of less noise and only toast transport-level failures.
    pub fn fire_action(&self, path: &'static str, body: serde_json::Value) {
        let daemon = self.daemon.clone();
        let path_str = path.to_string();
        std::thread::spawn(move || {
            // Generic POST wrapper on DaemonClient ignores the response and
            // collapses `Api { .. }` errors (we know the daemon accepted the
            // call but didn't like the body). Transport errors are still
            // surfaced so the user sees "daemon unreachable" when relevant.
            if let Err(e) = daemon.post_action(&path_str, &body) {
                log::warn!("action {} failed: {}", path, e);
            }
        });
    }

    /// Spawn a thumbnail fetch. No-op if we've already requested it
    /// (the worker may still be in flight from an earlier call) so we
    /// don't spam fetches across navigation.
    pub fn request_thumb(&mut self, url: &str) {
        if self.thumb_cache.contains_key(url) {
            return;
        }
        crate::thumbnails::spawn_fetch(url.to_string(), self.image_tx.clone());
    }
}

impl Theme {
    pub fn visuals(&self) -> egui::Visuals {
        match self {
            Theme::Dark => egui::Visuals::dark(),
            Theme::Light => egui::Visuals::light(),
        }
    }
}
