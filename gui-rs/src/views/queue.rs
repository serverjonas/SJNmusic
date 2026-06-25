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
        for (idx, song) in body.queue.iter().enumerate() {                egui::Frame::group(ui.style())
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
                                app.fire_action("/del", serde_json::json!({"query": song.name}));
                            }
                            if ui.button("▶ skip-to").clicked() {
                                app.fire_action("/play", serde_json::json!({"query": song.name}));
                            }
                        });
                    });
                });
        }
    });
}
