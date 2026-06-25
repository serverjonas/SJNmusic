//! Search + Download. Posts a query to `/search/yt`, renders the
//! resulting list of `YtCandidate`s with their `thumbnail` decoded into a
//! `TextureHandle`, and on click POSTs `/init` with the chosen URL.
//!
//! The yt-dlp search can take a few seconds, so it runs on a worker
//! thread and writes results into `AppState.search_results_bus` (drained
//! each `update()`).

use std::time::Instant;

use eframe::egui;

use crate::app::SJNMusicApp;
use crate::daemon::YtCandidate;

pub fn show(ui: &mut egui::Ui, app: &mut SJNMusicApp) {
    ui.heading("Search + Download");
    ui.separator();

    ui.horizontal(|ui| {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut app.search_q)
                .hint_text("Search YouTube via yt-dlp…")
                .desired_width(f32::INFINITY),
        );
        ui.add(
            egui::DragValue::new(&mut app.search_limit)
                .range(1..=20)
                .prefix(" n="),
        );
        if ui.button("Search").clicked()
            || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
        {
            start_search(app);
        }
    });

    ui.add_space(8.0);

    // Confirmation panel after a download click: shows the message and
    // offers a "back to search" button so the user isn't locked out.
    if let Some(info) = app.search_just_downloaded.clone() {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(egui::RichText::new(&info).strong());
            ui.horizontal(|ui| {
                if ui.button("Search again").clicked() {
                    app.search_just_downloaded = None;
                    start_search(app);
                }
                if ui.button("Clear").clicked() {
                    app.search_just_downloaded = None;
                    app.search_q.clear();
                    app.search_results.clear();
                }
            });
        });
        return;
    }

    if app.search_loading && app.search_results_bus.lock().unwrap().is_none() {
        ui.label(egui::RichText::new("Searching yt-dlp…").weak());
        return;
    }

    if app.search_results.is_empty() {
        ui.label(
            egui::RichText::new("Type a query above and press Search.")
                .weak(),
        );
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let snapshot = app.search_results.clone();
            for c in &snapshot {
                candidate_card(ui, c, app);
            }
        });
}

fn candidate_card(ui: &mut egui::Ui, c: &YtCandidate, app: &mut SJNMusicApp) {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(url) = c.thumbnail.clone() {
                    if !app.thumb_cache.contains_key(&url) {
                        crate::thumbnails::spawn_fetch(
                            url.clone(),
                            app.image_tx.clone(),
                        );
                    }
                    if let Some(tex) = app.thumb_cache.get(&url) {
                        ui.add(
                            egui::Image::from_texture(tex)
                                .fit_to_exact_size(egui::Vec2::splat(64.0)),
                        );
                    } else {
                        ui.allocate_ui(egui::Vec2::splat(64.0), |ui| {
                            ui.label("🎵");
                        });
                    }
                } else {
                    ui.allocate_ui(egui::Vec2::splat(64.0), |ui| {
                        ui.label("🎵");
                    });
                }
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(&c.title).strong());
                    ui.label(egui::RichText::new(&c.uploader).small().weak());
                    ui.label(
                        egui::RichText::new(crate::fmt::mmss(c.duration_secs))
                            .small()
                            .weak(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("download").clicked() {
                        start_download(app, c);
                    }
                });
            });
        });
}

fn start_search(app: &mut SJNMusicApp) {
    let q = app.search_q.trim().to_string();
    if q.is_empty() {
        return;
    }
    app.search_loading = true;
    app.search_just_downloaded = None;
    let daemon = app.daemon.clone();
    let bus = app.search_results_bus.clone();
    let limit = app.search_limit;
    std::thread::spawn(move || {
        let started = Instant::now();
        match daemon.search_yt(&q, limit) {
            Ok(results) => {
                log::debug!(
                    "search_yt: {} candidates in {:?}",
                    results.len(),
                    started.elapsed()
                );
                let mut slot = bus.lock().unwrap();
                *slot = Some(results);
            }
            Err(e) => {
                log::warn!("search_yt failed: {e}");
                // Empty Vec still clears the loading flag on the UI side.
                let mut slot = bus.lock().unwrap();
                *slot = Some(Vec::new());
            }
        }
    });
}

fn start_download(app: &mut SJNMusicApp, c: &YtCandidate) {
    let name = app.search_q.clone();
    let url = c.url.clone();
    app.search_just_downloaded = Some(format!(
        "Queued \"{}\". Watch Downloads for live status.",
        name
    ));
    let daemon = app.daemon.clone();
    let job_name = name.clone();
    std::thread::spawn(move || match daemon.init(&job_name, Some(&url)) {
        Ok(resp) => log::info!("init accepted: job {}", resp.job_id),
        Err(e) => log::warn!("init failed: {e}"),
    });
}
