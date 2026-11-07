//! Queue view: shows the head + queue. Renders from the live snapshot
//! (most recent poll) when the user is on this view, so per-poll tick
//! the queue DataFrame is rebuilt without a dedicated fetch.

use eframe::egui;

use crate::app::SJNMusicApp;

pub fn show(ui: &mut egui::Ui, app: &mut SJNMusicApp) {
    ui.heading("Queue");
    ui.separator();

    ui.horizontal(|ui| {
        if ui.button("Shuffle").clicked() {
            app.fire_action("/queue/shuffle", serde_json::json!({}));
            app.push_toast("Queue shuffled", crate::state::ToastLevel::Info);
        }
        if ui.button("Clear").clicked() {
            app.fire_action("/queue/clear", serde_json::json!({}));
            app.push_toast("Queue cleared", crate::state::ToastLevel::Info);
        }
        if ui.button("Refresh").clicked() {
            app.queue_loading = true;
            let daemon = app.daemon.clone();
            // The fire_action pattern hides the response; queue needs the
            // current snapshot so we pull synchronously.
            match daemon.queue() {
                Ok(q) => app.queue_data = Some(q),
                Err(e) => app.push_toast(format!("Refresh failed: {}", e), crate::state::ToastLevel::Warn),
            }
            app.queue_loading = false;
        }
    });

    // Pull from snapshot if not refreshed.
    if app.queue_data.is_none() {
        if let Some(np) = app.state.snapshot().queue.clone() {
            app.queue_data = Some(np);
        }
    }

    let body = match app.queue_data.clone() {
        Some(snap) => snap,
        None => {
            ui.label(egui::RichText::new("(no queue data — refreshing…)").weak());
            return;
        }
    };

    if let Some(cur) = &body.current {
        ui.add_space(10.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(egui::RichText::new("Now playing").small().weak());
            ui.label(egui::RichText::new(&cur.name).strong());
        });
        ui.add_space(10.0);
    }

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if body.queue.is_empty() {
            ui.label(egui::RichText::new("queue empty").weak());
            return;
        }
        let total = body.queue.len();
        for (idx, song) in body.queue.iter().enumerate() {
            let pos = idx + 1; // 1-based, matches /queue/remove & /queue/jump
            egui::Frame::group(ui.style())
                .fill(ui.visuals().faint_bg_color)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("{pos}."));
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(&song.name).strong());
                            ui.label(egui::RichText::new(format!("id {}", song.id)).small().weak());
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Remove only from the queue — the old button
                            // posted /del, which deleted the song from the
                            // library AND the disk. /queue/remove just
                            // takes it out of the upcoming queue.
                            if ui.button("remove").clicked() {
                                app.fire_action(
                                    "/queue/remove",
                                    serde_json::json!({"pos": pos}),
                                );
                                app.push_toast(
                                    format!("Removed {} from queue", song.name),
                                    crate::state::ToastLevel::Info,
                                );
                            }
                            // Jump: start this entry now, keeping the rest
                            // of the queue after it (the old button posted
                            // /play, which rebuilt the whole queue).
                            if ui.button("▶ play").clicked() {
                                app.fire_action(
                                    "/queue/jump",
                                    serde_json::json!({"pos": pos}),
                                );
                            }
                            // Manual reordering within the queue.
                            if pos > 1 && ui.button("▲").clicked() {
                                app.fire_action(
                                    "/queue/reorder",
                                    serde_json::json!({"from": pos, "to": pos - 1}),
                                );
                            }
                            if pos < total && ui.button("▼").clicked() {
                                app.fire_action(
                                    "/queue/reorder",
                                    serde_json::json!({"from": pos, "to": pos + 1}),
                                );
                            }
                        });
                    });
                });
        }
    });
}
