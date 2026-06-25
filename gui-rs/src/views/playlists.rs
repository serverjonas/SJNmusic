//! Playlists list view. Create input + open/delete each row. We poll
//! /playlists once on view-entry; refresh button re-polls.

use std::sync::Arc;

use eframe::egui;

use crate::app::SJNMusicApp;
use crate::daemon::Playlist;
use crate::daemon::DaemonClient;
use crate::state::Route;

pub fn show(ui: &mut egui::Ui, app: &mut SJNMusicApp) {
    ui.heading("Playlists");
    ui.separator();

    ui.horizontal(|ui| {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut app.new_playlist_name)
                .hint_text("new playlist name")
                .desired_width(f32::INFINITY),
        );
        let can_create = !app.new_playlist_name.trim().is_empty();
        let enter_create = resp.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter))
            && can_create;
        if ui.add_enabled(can_create, egui::Button::new("Create")).clicked()
            || enter_create
        {
            let name = app.new_playlist_name.trim().to_string();
            app.push_toast(
                format!("Creating \"{}\"…", name),
                crate::state::ToastLevel::Info,
            );
            // Try synchronously so we only clear the input on success —
            // on duplicate-name failure we keep the user's text and show
            // an error toast, instead of navigating to a phantom route.
            let daemon: Arc<DaemonClient> = app.daemon.clone();
            match daemon.create_playlist(&name) {
                Ok(_) => {
                    app.new_playlist_name.clear();
                    // Refresh the list so the new row appears immediately.
                    match daemon.playlists() {
                        Ok(list) => app.playlists_data = list,
                        Err(e) => app.push_toast(
                            format!("Refresh failed: {}", e),
                            crate::state::ToastLevel::Warn,
                        ),
                    }
                }
                Err(e) => {
                    app.push_toast(
                        format!("Create failed: {}", e),
                        crate::state::ToastLevel::Error,
                    );
                    // Input retained so the user can fix and retry.
                }
            }
        }

        if ui.button("Refresh").clicked() {
            match app.daemon.playlists() {
                Ok(list) => app.playlists_data = list,
                Err(e) => app.push_toast(
                    format!("Refresh failed: {}", e),
                    crate::state::ToastLevel::Warn,
                ),
            }
        }
    });

    // First-paint hydration: fetch synchronously so the list isn't empty
    // when the user lands on this view. Only on the very first render.
    if app.playlists_data.is_empty() && app.playlists_data.is_empty() {
        if let Ok(list) = app.daemon.playlists() {
            app.playlists_data = list;
        }
    }

    ui.add_space(8.0);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if app.playlists_data.is_empty() {
                ui.label(egui::RichText::new("No playlists yet.").weak());
                return;
            }
            for pl in app.playlists_data.clone() {
                row(ui, &pl, app);
            }
        });
}

fn row(ui: &mut egui::Ui, pl: &Playlist, app: &mut SJNMusicApp) {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("▶");
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(&pl.name).strong());
                    ui.label(
                        egui::RichText::new(format!("{} songs", pl.songs.len()))
                            .small()
                            .weak(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("delete").clicked() {
                        let name = pl.name.clone();
                        let daemon: Arc<DaemonClient> = app.daemon.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = daemon.delete_playlist(&name) {
                                log::warn!("delete playlist failed: {e}");
                            }
                        });
                        app.playlists_data.retain(|p| p.name != pl.name);
                        app.push_toast(
                            format!("Deleted \"{}\"", pl.name),
                            crate::state::ToastLevel::Info,
                        );
                    }
                    if ui.button("open").clicked() {
                        app.route = Route::Playlist(pl.name.clone());
                    }
                });
            });
        });
}
