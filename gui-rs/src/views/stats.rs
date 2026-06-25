//! Stats view: total plays + total time cards + top songs list.

use eframe::egui;

use crate::app::SJNMusicApp;

pub fn show(ui: &mut egui::Ui, app: &mut SJNMusicApp) {
    ui.heading("Stats");
    ui.separator();

    if app.stats_data.is_none() && !app.stats_loading {
        match app.daemon.stats() {
            Ok(s) => app.stats_data = Some(s),
            Err(e) => {
                app.push_toast(format!("Stats failed: {}", e), crate::state::ToastLevel::Warn);
            }
        }
    }

    ui.horizontal(|ui| {
        if ui.button("Refresh").clicked() {
            app.stats_loading = true;
            match app.daemon.stats() {
                Ok(s) => app.stats_data = Some(s),
                Err(e) => app.push_toast(format!("Refresh failed: {}", e), crate::state::ToastLevel::Warn),
            }
            app.stats_loading = false;
        }
    });

    let stats = match app.stats_data.clone() {
        Some(s) => s,
        None => {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("(no stats yet)").weak());
            return;
        }
    };

    ui.add_space(8.0);
    egui::Grid::new("stats-cards")
        .num_columns(2)
        .spacing([16.0, 16.0])
        .show(ui, |ui| {
            card(ui, "Total plays", &stats.total_plays.to_string());
            card(ui, "Total time", &crate::fmt::big_secs(stats.total_secs as f64));
            ui.end_row();
        });

    ui.add_space(12.0);
    ui.label(egui::RichText::new("Top songs").strong());
    ui.separator();

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(400.0)
        .show(ui, |ui| {
            if stats.top_songs.is_empty() {
                ui.label(egui::RichText::new("No plays yet.").weak());
                return;
            }
            for (i, t) in stats.top_songs.iter().enumerate() {                    egui::Frame::group(ui.style())
                        .fill(ui.visuals().faint_bg_color)
                        .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(format!("{}.", i + 1));
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(t.name.as_deref().unwrap_or("(missing)")).strong());
                                ui.label(egui::RichText::new(format!("id {}", t.song_id)).small().weak());
                            });
                            ui.label(format!("{} plays", t.plays));
                            ui.label(crate::fmt::mmss(t.total_secs as f64));
                        });
                    });
            }
        });
}

fn card(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.label(egui::RichText::new(label).small().weak());
            ui.label(egui::RichText::new(value).strong().size(28.0));
        });
}
