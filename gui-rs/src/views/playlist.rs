//! Single playlist view: header buttons (add, play, rename, duplicate,
//! delete) plus the song rows with up/down/remove.

use eframe::egui;

use crate::app::SJNMusicApp;
use crate::daemon::Playlist;
use crate::state::Route;

pub fn show(ui: &mut egui::Ui, app: &mut SJNMusicApp, name: &str) {
    ui.heading(name);
    ui.separator();

    // Hydrate on first paint if needed.
    if !app.playlist_data.contains_key(name) {
        match app.daemon.get_playlist(name) {
            Ok(pl) => {
                app.playlist_data.insert(name.to_string(), pl);
            }
            Err(e) => {
                app.push_toast(
                    format!("Open \"{}\" failed: {}", name, e),
                    crate::state::ToastLevel::Warn,
                );
                // Bounce back to the playlists list so the user sees
                // something useful instead of an empty page forever.
                app.route = Route::Playlists;
                return;
            }
        }
    }
    let pl = match app.playlist_data.get(name).cloned() {
        Some(p) => p,
        None => return,
    };

    ui.horizontal(|ui| {
        let mut add_q = String::new();
        let resp = ui.add(
            egui::TextEdit::singleline(&mut add_q)
                .hint_text("add a song by name…")
                .desired_width(f32::INFINITY),
        );
        if (ui.button("Add").clicked() || (resp.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter))))
            && !add_q.trim().is_empty()
        {
            let pl_name = name.to_string();
            let q = add_q.trim().to_string();
            let daemon = app.daemon.clone();
            // Clone `q` so the worker thread can move the original into
            // its closure while the toast below still borrows it.
            let q_for_worker = q.clone();
            std::thread::spawn(move || {
                match daemon.add_to_playlist(&pl_name, &q_for_worker) {
                    Ok(_) => log::debug!("added \"{}\" to {}", q_for_worker, pl_name),
                    Err(e) => log::warn!("add to playlist failed: {e}"),
                }
            });
            app.push_toast(format!("Added \"{}\" to {}", q, name), crate::state::ToastLevel::Info);
        }
        if ui.button("▶ Play all").clicked() {
            let pl_name = name.to_string();
            let daemon = app.daemon.clone();
            std::thread::spawn(move || {
                if let Err(e) = daemon.play_playlist(&pl_name) {
                    log::warn!("play playlist failed: {e}");
                }
            });
            app.route = Route::Queue;
        }
        if ui.button("Rename").clicked() {
            let new_name = rename_dialog(ui, name);
            if let Some(new_name) = new_name {
                let old_name = name.to_string();
                let daemon = app.daemon.clone();
                // Clone both names so the worker thread can consume
                // them while route assignment + cache invalidation
                // below still need them.
                let old_for_worker = old_name.clone();
                let new_for_worker = new_name.clone();
                std::thread::spawn(move || {
                    if let Err(e) = daemon.rename_playlist(&old_for_worker, &new_for_worker) {
                        log::warn!("rename failed: {e}");
                    }
                });
                app.route = Route::Playlist(new_name.clone());
                app.playlist_data.remove(&old_name);
                app.push_toast(format!("Renamed \"{}\" → \"{}\"", name, new_name), crate::state::ToastLevel::Success);
            }
        }
        if ui.button("Duplicate").clicked() {
            let dest = duplicate_dialog(ui, name);
            if let Some(dest) = dest {
                let src = name.to_string();
                let daemon = app.daemon.clone();
                // Clone `dest` so the worker thread can own it while
                // the route assignment below still borrows it.
                let dest_for_worker = dest.clone();
                std::thread::spawn(move || {
                    if let Err(e) = daemon.duplicate_playlist(&src, &dest_for_worker) {
                        log::warn!("duplicate failed: {e}");
                    }
                });
                app.route = Route::Playlist(dest.clone());
                app.push_toast(format!("Duplicated → \"{}\"", dest), crate::state::ToastLevel::Success);
            }
        }
        if ui.button("🗑 Delete").clicked() {
            let pl_name = name.to_string();
            let daemon = app.daemon.clone();
            std::thread::spawn(move || {
                if let Err(e) = daemon.delete_playlist(&pl_name) {
                    log::warn!("delete playlist failed: {e}");
                }
            });
            app.playlist_data.remove(name);
            app.route = Route::Playlists;
            app.push_toast(format!("Deleted \"{}\"", name), crate::state::ToastLevel::Info);
        }
    });

    ui.add_space(8.0);

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if pl.songs.is_empty() {
            ui.label(egui::RichText::new("empty playlist").weak());
            return;
        }
        for (idx, song) in pl.songs.iter().enumerate() {
            row(ui, &pl, idx, song, name, app);
        }
    });
}

fn row(
    ui: &mut egui::Ui,
    pl: &Playlist,
    idx: usize,
    song: &crate::daemon::Song,
    pl_name: &str,
    app: &mut SJNMusicApp,
) {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("{}.", idx + 1));
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(&song.name).strong());
                    ui.label(egui::RichText::new(format!("id {}", song.id)).small().weak());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("remove").clicked() {
                        let id = song.id;
                        let pl_name_owned = pl_name.to_string();
                        let daemon = app.daemon.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = daemon.remove_from_playlist(&pl_name_owned, id) {
                                log::warn!("remove failed: {e}");
                            }
                        });
                        app.playlist_data.remove(pl_name);
                        app.push_toast(format!("Removed \"{}\"", song.name), crate::state::ToastLevel::Info);
                    }
                    if idx + 1 < pl.songs.len() && ui.button("↓").clicked() {
                        // 1-based
                        let from = idx + 1;
                        let to = idx + 2;
                        let pl_name_owned = pl_name.to_string();
                        let daemon = app.daemon.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = daemon.reorder_playlist(&pl_name_owned, from, to) {
                                log::warn!("reorder failed: {e}");
                            }
                        });
                        app.playlist_data.remove(pl_name);
                    }
                    if idx > 0 && ui.button("↑").clicked() {
                        let from = idx + 1;
                        let to = idx;
                        let pl_name_owned = pl_name.to_string();
                        let daemon = app.daemon.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = daemon.reorder_playlist(&pl_name_owned, from, to) {
                                log::warn!("reorder failed: {e}");
                            }
                        });
                        app.playlist_data.remove(pl_name);
                    }
                    if ui.button("▶ play").clicked() {
                        app.fire_action("/play", serde_json::json!({"query": song.name}));
                    }
                });
            });
        });
}

fn rename_dialog(ui: &mut egui::Ui, current: &str) -> Option<String> {
    let mut new_name = current.to_string();
    let mut open = true;
    let mut result = None;
    egui::Window::new("Rename playlist")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ui.ctx(), |ui| {
            ui.horizontal(|ui| {
                ui.label("New name:");
                ui.text_edit_singleline(&mut new_name);
            });
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    let trimmed = new_name.trim();
                    if !trimmed.is_empty() && trimmed != current {
                        result = Some(trimmed.to_string());
                    }
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Cancel").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    if !open {
        result
    } else {
        None
    }
}

fn duplicate_dialog(ui: &mut egui::Ui, current: &str) -> Option<String> {
    let mut new_name = format!("{} (copy)", current);
    let mut open = true;
    let mut result = None;
    egui::Window::new("Duplicate playlist as")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ui.ctx(), |ui| {
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.text_edit_singleline(&mut new_name);
            });
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    let trimmed = new_name.trim();
                    if !trimmed.is_empty() {
                        result = Some(trimmed.to_string());
                    }
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Cancel").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    if !open {
        result
    } else {
        None
    }
}
