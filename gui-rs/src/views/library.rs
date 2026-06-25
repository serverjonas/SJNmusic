//! Library view: song list + fuzzy filter box. The filter input is
//! debounced; a worker fetches via the daemon and writes the result
//! into `AppState.library_results_bus`, which the UI drains at the
//! start of every repaint.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::app::SJNMusicApp;
use crate::daemon::{DaemonClient, Song};

const DEBOUNCE: Duration = Duration::from_millis(180);

pub fn show(ui: &mut egui::Ui, app: &mut SJNMusicApp) {
    ui.horizontal(|ui| {
        ui.heading("Library");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut app.library_query)
                    .hint_text("Filter by name (fuzzy)")
                    .desired_width(f32::INFINITY),
            );
            if response.changed() && !app.library_loading {
                app.library_loading = true;
                let q = app.library_query.clone();
                let daemon: Arc<DaemonClient> = app.daemon.clone();
                let bus = app.library_results_bus.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(DEBOUNCE);
                    let outcome = if q.is_empty() {
                        daemon.songs()
                    } else {
                        daemon.search_all(&q)
                    };
                    let mut slot = bus.lock().unwrap();
                    // Push an empty Vec on Err so the drain path clears
                    // the loading spinner instead of leaving it stuck.
                    *slot = Some(outcome.unwrap_or_default());
                });
            }
            if ui.button("Refresh").clicked() {
                refresh_now(app);
            }
        });
    });
    ui.separator();

    // First-paint hydration when the cache AND the slot AND no in-flight.
    if app.library_songs.is_empty()
        && !app.library_loading
        && app.library_results_bus.lock().unwrap().is_none()
    {
        refresh_now(app);
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if app.library_songs.is_empty() {
                let msg = if !app.state.is_online() {
                    "(daemon offline — can't load library)"
                } else if app.library_loading {
                    "(loading…)"
                } else {
                    "No songs yet — head to Search + Download."
                };
                ui.label(egui::RichText::new(msg).weak());
                return;
            }
            let snapshot = app.library_songs.clone();
            for song in &snapshot {
                row(ui, song, app);
            }
        });
}

fn row(ui: &mut egui::Ui, song: &Song, app: &mut SJNMusicApp) {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("▶");
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(&song.name).strong());
                    let meta = match song.duration_secs {
                        Some(d) => format!("id {} · {}", song.id, crate::fmt::mmss(d)),
                        None => format!("id {}", song.id),
                    };
                    ui.label(egui::RichText::new(meta).small().weak());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("delete").clicked() {
                        app.fire_action("/del", serde_json::json!({"query": song.name}));
                        app.library_songs.retain(|s| s.id != song.id);
                        app.push_toast(
                            format!("Deleted \"{}\"", song.name),
                            crate::state::ToastLevel::Info,
                        );
                    }
                    if ui.button("+queue").clicked() {
                        app.fire_action("/add", serde_json::json!({"query": song.name}));
                        app.push_toast(
                            format!("Queued \"{}\"", song.name),
                            crate::state::ToastLevel::Info,
                        );
                    }
                    if ui.button("▶ play").clicked() {
                        app.fire_action("/play", serde_json::json!({"query": song.name}));
                    }
                });
            });
        });
}

/// First-paint fetch. We set `library_loading` BEFORE the call so a
/// fast double-click doesn't enter twice. We always push SOMETHING into
/// the bus (empty list on Err) so `drain_background_results` can clear
/// the loading spinner.
fn refresh_now(app: &mut SJNMusicApp) {
    if app.library_loading {
        return;
    }
    app.library_loading = true;
    let daemon = app.daemon.clone();
    let bus = app.library_results_bus.clone();
    let outcome = daemon.songs();
    let mut slot = bus.lock().unwrap();
    *slot = Some(outcome.unwrap_or_default());
    app.library_loading = false;
    let _ = Instant::now(); // silence "unused" if compiler complains
}
