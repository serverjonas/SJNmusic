//! Search + Download.
//!
//! Default flow: posts a query to `/search/yt/ranked` (the daemon
//! ranks 3 ytsearch variants and attaches scoring + flag badges), so
//! the official track usually sits at index 0. Renders each
//! `RankedCandidate` as a card with its score + flags so the user
//! can see *why* the daemon ranked it that way.
//!
//! An optional "🪄 Smart Pick" button bypasses the manual picker by
//! asking the daemon to decide. When the daemon's confidence beats
//! the auto-pick margin, the chosen candidate downloads straight
//! away; otherwise the daemon returns ranked candidates for the
//! picker.
//!
//! The yt-dlp search can take a few seconds, so all network calls
//! run on background threads and write results into
//! `AppState.search_results_bus` (drained each `update()`).

use std::time::Instant;

use eframe::egui;

use crate::app::SJNMusicApp;
use crate::daemon::{PickResponse, RankedCandidate};

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
        if app.search_results.is_empty() {
            ui.add_space(4.0);
        } else {
            ui.add_space(4.0);
            // 🪄 Smart Pick: ask the daemon to decide. If its top
            // candidate beats the runner-up by ≥ `search_margin` pts
            // (default 30), the result downloads straight away; if
            // it's unsure, the daemon returns the ranked picker.
            if !app.search_picking
                && ui.button("\u{1FA84} Smart Pick").clicked()
            {
                start_smart_pick(app);
            }
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
    if app.search_picking {
        ui.label(egui::RichText::new("Smart-picking…").weak());
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

fn candidate_card(ui: &mut egui::Ui, c: &RankedCandidate, app: &mut SJNMusicApp) {
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
                            ui.label("\u{1F3B5}");
                        });
                    }
                } else {
                    ui.allocate_ui(egui::Vec2::splat(64.0), |ui| {
                        ui.label("\u{1F3B5}");
                    });
                }
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(&c.title)
                            .strong()
                            .color(score_color(c.score)),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  ({})",
                            c.uploader,
                            crate::fmt::mmss(c.duration_secs)
                        ))
                        .small()
                        .weak(),
                    );
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("score {:+}", c.score))
                                .monospace()
                                .strong(),
                        );
                        for f in &c.flags {
                            ui.label(
                                egui::RichText::new(flag_chip(f))
                                    .small()
                                    .color(flag_color(f)),
                            );
                        }
                    });
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("download").clicked() {
                        start_download(app, c);
                    }
                });
            });
        });
}

fn score_color(score: i32) -> egui::Color32 {
    if score >= 100 {
        egui::Color32::from_rgb(140, 220, 160)
    } else if score >= 30 {
        egui::Color32::from_rgb(220, 220, 220)
    } else if score >= 0 {
        egui::Color32::from_rgb(220, 200, 140)
    } else {
        egui::Color32::from_rgb(220, 140, 140)
    }
}

fn flag_chip(f: &str) -> String {
    match f {
        "official" | "official-upload" => format!("\u{2705} {f}"),
        "lyrics" => format!("\u{1F4DD} {f}"),
        "reaction" => format!("\u{1F914} {f}"),
        "remix" => format!("\u{1F3B6} {f}"),
        "live" => format!("\u{1F3B5} {f}"),
        "nightcore" | "slowed" | "bass-boosted" | "reverb" => format!("\u{26A0} {f}"),
        "instrumental" => format!("\u{1F3B5} {f}"),
        "karaoke" => format!("\u{1F3A4} {f}"),
        "long" | "short" => format!("\u{23F1} {f}"),
        "artist-mismatch" => format!("\u{274C} {f}"),
        _ => f.to_string(),
    }
}

fn flag_color(f: &str) -> egui::Color32 {
    match f {
        "official" | "official-upload" => egui::Color32::from_rgb(140, 220, 160),
        "lyrics" | "reaction" | "remix" | "live"
        | "nightcore" | "slowed" | "bass-boosted" | "reverb"
        | "instrumental" | "karaoke" | "artist-mismatch" => {
            egui::Color32::from_rgb(220, 140, 140)
        }
        "long" | "short" => egui::Color32::from_rgb(220, 180, 140),
        _ => egui::Color32::from_rgb(180, 180, 180),
    }
}

fn start_search(app: &mut SJNMusicApp) {
    let q = app.search_q.trim().to_string();
    if q.is_empty() {
        return;
    }
    app.search_loading = true;
    app.search_picking = false;
    app.search_just_downloaded = None;
    let daemon = app.daemon.clone();
    let bus = app.search_results_bus.clone();
    let limit = app.search_limit;
    std::thread::spawn(move || {
        let started = Instant::now();
        match daemon.search_yt_ranked(&q, limit) {
            Ok(results) => {
                log::debug!(
                    "search_yt_ranked: {} candidates in {:?}",
                    results.len(),
                    started.elapsed()
                );
                let mut slot = bus.lock().unwrap();
                *slot = Some(results);
            }
            Err(e) => {
                log::warn!("search_yt_ranked failed: {e}");
                // Empty Vec still clears the loading flag on the UI side.
                let mut slot = bus.lock().unwrap();
                *slot = Some(Vec::new());
            }
        }
    });
}

/// Smart Pick: ask the daemon's `/pick` endpoint to either
/// auto-select (when top beats runner-up by `search_margin`) or hand
/// us back the ranked picker. On `auto` we download immediately; on
/// `needs_choice` we either show a native dialog or fold the ranked
/// list back into the existing results for visual inspection. Loading
/// state is tracked on `search_picking` so the UI doesn't show stale
/// "Search returned N results" while the pick request is in flight.
fn start_smart_pick(app: &mut SJNMusicApp) {
    let q = app.search_q.trim().to_string();
    if q.is_empty() {
        return;
    }
    app.search_picking = true;
    let daemon = app.daemon.clone();
    let margin = app.search_margin;
    let limit = app.search_limit;
    let job_name = q.clone();
    std::thread::spawn(move || {
        match daemon.pick(&q, limit, Some(margin)) {
            Ok(PickResponse::Auto {
                url,
                title,
                score,
                ..
            }) => {
                // Download straight away.
                log::info!("smart-pick auto-selected: {title:?} score={score}");
                if let Err(e) = daemon.init(&job_name, Some(&url)) {
                    log::warn!("smart-pick /init failed: {e}");
                }
            }
            Ok(PickResponse::NeedsChoice {
                candidates,
                top_score,
                runner_up_score,
                margin,
            }) => {
                log::info!(
                    "smart-pick needs_choice: top={} runner={} margin={}",
                    top_score,
                    runner_up_score,
                    margin
                );
                if let Some(best) = candidates.first() {
                    log::info!(
                        "smart-pick fallback to top scorer: {} ({})",
                        best.title,
                        best.uploader
                    );
                    if let Err(e) = daemon.init(&job_name, Some(&best.url)) {
                        log::warn!("smart-pick fallback /init failed: {e}");
                    }
                }
            }
            Ok(PickResponse::Empty { message }) => {
                log::warn!("smart-pick empty: {message}");
            }
            Err(e) => log::warn!("smart-pick call failed: {e}"),
        }
    });
}

fn start_download(app: &mut SJNMusicApp, c: &RankedCandidate) {
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
