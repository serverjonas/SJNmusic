//! Downloads view: live status table for in-flight and recent yt-dlp
//! jobs. Reads from the polled snapshot; polls fire every second so the
//! table is fresh without needing a per-view fetch.

use std::time::{Duration, Instant};

use eframe::egui;

use crate::app::SJNMusicApp;

pub fn show(ui: &mut egui::Ui, app: &mut SJNMusicApp) {
    ui.heading("Downloads");
    ui.separator();

    if Instant::now().duration_since(app.last_downloads_refresh()) > Duration::from_secs(4) {
        if let Ok(dl) = app.daemon.downloads() {
            app.set_downloads_override(dl);
        }
    }

    ui.horizontal(|ui| {
        if ui.button("Refresh").clicked() {
            if let Ok(dl) = app.daemon.downloads() {
                app.set_downloads_override(dl);
            }
        }
    });

    ui.add_space(8.0);

    let downloads = app.current_downloads();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("downloads-grid")
            .num_columns(5)
            .spacing([10.0, 6.0])
            .striped(true)
            .min_row_height(20.0)
            .show(ui, |ui| {
                ui.strong("id");
                ui.strong("status");
                ui.strong("name");
                ui.strong("source");
                ui.strong("error / result");
                ui.end_row();
                if downloads.is_empty() {
                    ui.label("(no jobs)");
                    ui.end_row();
                } else {
                    for d in &downloads {
                        ui.label(format!("{}", d.id));
                        badge(ui, &d.status);
                        ui.label(&d.name);
                        ui.label(d.source.as_deref().unwrap_or("-"));
                        ui.label(match (&d.song_id, &d.error) {
                            (Some(id), _) => format!("song {}", id),
                            (None, Some(err)) => err.clone(),
                            _ => "-".into(),
                        });
                        ui.end_row();
                    }
                }
            });
    });
}

fn badge(ui: &mut egui::Ui, status: &str) {
    let color = match status {
        "done" => egui::Color32::from_rgb(80, 180, 110),
        "running" => egui::Color32::from_rgb(110, 170, 230),
        "failed" => egui::Color32::from_rgb(220, 110, 110),
        "queued" => egui::Color32::from_rgb(180, 170, 100),
        _other => ui.visuals().text_color(),
    };
    ui.label(egui::RichText::new(status).color(color).strong());
}

// ---------------------------------------------------------------------
// Thin extension trait so views can read the latest polled snapshot OR
// an explicit user-clicked refresh without bolting a HashMap onto the
// main AppState struct.
// ---------------------------------------------------------------------

impl SJNMusicApp {
    pub fn last_downloads_refresh(&self) -> Instant {
        *self.downloads_refresh_at.lock().unwrap()
    }

    pub fn set_downloads_override(&mut self, dl: Vec<crate::daemon::DownloadJob>) {
        *self.downloads_override.lock().unwrap() = Some(dl);
        *self.downloads_refresh_at.lock().unwrap() = Instant::now();
    }

    pub fn current_downloads(&mut self) -> Vec<crate::daemon::DownloadJob> {
        if let Some(ovr) = self.downloads_override.lock().unwrap().clone() {
            return ovr;
        }
        self.state.snapshot().downloads
    }
}
