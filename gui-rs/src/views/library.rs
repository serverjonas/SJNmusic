//! Library view: song list + fuzzy filter box, multi-selection for
//! re-recognition, and an expandable detail panel that shows metadata and
//! the permanent original source of a song.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::app::SJNMusicApp;
use crate::daemon::{DaemonClient, Song};
use crate::state::ToastLevel;

const DEBOUNCE: Duration = Duration::from_millis(180);

pub fn show(ui: &mut egui::Ui, app: &mut SJNMusicApp) {
    ui.horizontal(|ui| {
        ui.heading("Library");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // ▶ Play all (shuffle): replace the queue with the whole
            // library in random order. Disabled while the daemon is
            // unreachable so the user doesn't get a silent no-op click,
            // disabled while the first library fetch is still in flight
            // (so the user sees a "loading…" message instead of a
            // silently dead button), and disabled until the fetch has
            // returned a non-empty result (so a real empty library
            // doesn't pretend to have something to shuffle).
            let can_play_all = app.state.is_online()
                && !app.library_loading
                && !app.library_songs.is_empty();
            if ui
                .add_enabled(can_play_all, egui::Button::new("▶ Play all (shuffle)"))
                .clicked()
            {
                play_all_now(app);
            }
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

    // Multi-selection actions. Only shown when something is selected so
    // the normal library layout stays uncluttered.
    if !app.selected_songs.is_empty() {
        ui.horizontal(|ui| {
            ui.label(format!("{} selected", app.selected_songs.len()));
            if ui.button("🔄 Re-recognize selected").clicked() {
                let ids: Vec<i64> = app.selected_songs.iter().copied().collect();
                for id in &ids {
                    app.fire_action(
                        "/metadata/recognize",
                        serde_json::json!({ "song_id": id, "force": false }),
                    );
                }
                app.push_toast(
                    format!("Recognition queued for {} song(s)", ids.len()),
                    ToastLevel::Info,
                );
            }
            if ui.button("Clear selection").clicked() {
                app.selected_songs.clear();
            }
        });
        ui.separator();
    }

    // Detail panel for the selected song (metadata + original source).
    if let Some(song) = app.song_detail.clone() {
        song_detail(ui, app, &song);
        ui.separator();
    }

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
                let mut selected = app.selected_songs.contains(&song.id);
                if ui.checkbox(&mut selected, "").changed() {
                    if selected {
                        app.selected_songs.insert(song.id);
                    } else {
                        app.selected_songs.remove(&song.id);
                    }
                }
                ui.label("▶");
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(&song.name).strong());
                    let mut meta = match song.duration_secs {
                        Some(d) => format!("id {} · {}", song.id, crate::fmt::mmss(d)),
                        None => format!("id {}", song.id),
                    };
                    if let Some(artist) = song.artist.as_deref() {
                        meta.push_str(" · ");
                        meta.push_str(artist);
                    }
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(meta).small().weak());
                        if let Some(status) = song.metadata_status.as_deref() {
                            ui.label(
                                egui::RichText::new(status)
                                    .small()
                                    .color(metadata_status_color(status)),
                            );
                        }
                    });
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("delete").clicked() {
                        app.fire_action("/del", serde_json::json!({"query": song.name}));
                        app.library_songs.retain(|s| s.id != song.id);
                        app.selected_songs.remove(&song.id);
                        if app.song_detail.as_ref().map(|s| s.id) == Some(song.id) {
                            app.song_detail = None;
                        }
                        app.push_toast(
                            format!("Deleted \"{}\"", song.name),
                            ToastLevel::Info,
                        );
                    }
                    if ui.button("+queue").clicked() {
                        app.fire_action("/add", serde_json::json!({"query": song.name}));
                        app.push_toast(
                            format!("Queued \"{}\"", song.name),
                            ToastLevel::Info,
                        );
                    }
                    if ui.button("▶ play").clicked() {
                        app.fire_action("/play", serde_json::json!({"query": song.name}));
                    }
                    if ui.button("details").clicked() {
                        app.song_detail = Some(song.clone());
                    }
                });
            });
        });
}

// ---------------------------------------------------------------------
// Song detail: metadata + Source / Herkunft
// ---------------------------------------------------------------------

fn song_detail(ui: &mut egui::Ui, app: &mut SJNMusicApp, song: &Song) {
    let mut close = false;
    egui::Frame::group(ui.style())
        .fill(ui.visuals().extreme_bg_color)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(
                        song.title.clone().unwrap_or_else(|| song.name.clone()),
                    )
                    .strong()
                    .size(16.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("✖").clicked() {
                        close = true;
                    }
                });
            });
            ui.label(egui::RichText::new(&song.path).small().weak());
            ui.add_space(4.0);

            ui.columns(2, |cols| {
                cols[0].label(egui::RichText::new("Metadata").strong());
                egui::Grid::new("song-detail-metadata")
                    .num_columns(2)
                    .spacing([12.0, 3.0])
                    .show(&mut cols[0], |ui| {
                        kv(ui, "Title", song.title.as_deref());
                        kv(ui, "Artist", song.artist.as_deref());
                        kv(ui, "Album", song.album.as_deref());
                        kv(ui, "Album artist", song.album_artist.as_deref());
                        kv(ui, "Composer", song.composer.as_deref());
                        kv(ui, "Genre", song.genre.as_deref());
                        kv(ui, "Released", song.release_date.as_deref());
                        if let Some(y) = song.release_year {
                            kv(ui, "Year", Some(&y.to_string()));
                        }
                        if let Some(n) = song.track_number {
                            let value = match song.track_total {
                                Some(t) => format!("{n} / {t}"),
                                None => n.to_string(),
                            };
                            kv(ui, "Track", Some(&value));
                        }
                        if let Some(n) = song.disc_number {
                            let value = match song.disc_total {
                                Some(t) => format!("{n} / {t}"),
                                None => n.to_string(),
                            };
                            kv(ui, "Disc", Some(&value));
                        }
                        kv(ui, "ISRC", song.isrc.as_deref());
                        kv(ui, "MB recording", song.musicbrainz_recording_id.as_deref());
                        kv(ui, "MB release", song.musicbrainz_release_id.as_deref());
                        kv(
                            ui,
                            "MB release group",
                            song.musicbrainz_release_group_id.as_deref(),
                        );
                        kv(ui, "MB artist", song.musicbrainz_artist_id.as_deref());
                        if let Some(score) = song.acoustid_score {
                            kv(ui, "AcoustID score", Some(&format!("{score:.3}")));
                        }
                        kv(ui, "AcoustID id", song.acoustid_id.as_deref());
                    });

                cols[1].label(egui::RichText::new("Recognition status").strong());
                egui::Grid::new("song-detail-recognition")
                    .num_columns(2)
                    .spacing([12.0, 3.0])
                    .show(&mut cols[1], |ui| {
                        kv(ui, "Status", song.metadata_status.as_deref());
                        kv(ui, "Source", song.metadata_source.as_deref());
                        if let Some(c) = song.metadata_confidence {
                            kv(ui, "Confidence", Some(&format!("{c:.3}")));
                        }
                        if let Some(ts) = song.metadata_last_updated_at {
                            kv(ui, "Updated", Some(&fmt_unix(ts)));
                        }
                        if let Some(ts) = song.metadata_last_checked_at {
                            kv(ui, "Checked", Some(&fmt_unix(ts)));
                        }
                        kv(ui, "Error", song.metadata_error.as_deref());
                    });

                cols[1].add_space(8.0);
                cols[1].label(egui::RichText::new("Source / Herkunft").strong());
                show_source(&mut cols[1], app, song);
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("🔄 Re-recognize").clicked() {
                    app.fire_action(
                        "/metadata/recognize",
                        serde_json::json!({ "song_id": song.id, "force": false }),
                    );
                    app.push_toast("Recognition queued", ToastLevel::Info);
                }
                if ui.button("♻ Force re-recognize").clicked() {
                    app.fire_action(
                        "/metadata/recognize",
                        serde_json::json!({ "song_id": song.id, "force": true }),
                    );
                    app.push_toast("Force recognition queued", ToastLevel::Info);
                }
                if ui.button("💾 Sync DB → file").clicked() {
                    app.fire_action(
                        "/metadata/sync",
                        serde_json::json!({ "song_id": song.id }),
                    );
                    app.push_toast("Tag sync queued", ToastLevel::Info);
                }
                if ui.button("Refresh").clicked() {
                    match app.daemon.song(song.id) {
                        Ok(fresh) => app.song_detail = Some(fresh),
                        Err(e) => app.push_toast(
                            format!("Could not refresh details: {e}"),
                            ToastLevel::Error,
                        ),
                    }
                }
            });
        });
    if close {
        app.song_detail = None;
    }
}

fn show_source(ui: &mut egui::Ui, app: &mut SJNMusicApp, song: &Song) {
    let stype = song.source_type.as_deref().unwrap_or("unknown");
    match stype {
        "youtube" | "url" => {
            let direct = stype == "url";
            let platform = if direct {
                song.source_platform.as_deref()
            } else {
                song.source_platform.as_deref().or(Some("YouTube"))
            };
            kv(ui, "Platform", platform);
            kv(ui, if direct { "File URL" } else { "Video URL" }, song.source_url.as_deref());
            if let Some(url) = song.source_url.as_deref() {
                ui.horizontal(|ui| {
                    if ui.link("open original URL").clicked() {
                        open_url(app, url);
                    }
                });
            }
            kv(
                ui,
                if direct { "Filename" } else { "Video ID" },
                song.source_identifier.as_deref(),
            );
            kv(ui, "Original title", song.source_title.as_deref());
        }
        "local_file" => {
            kv(ui, "Source type", Some("Local file"));
            kv(ui, "Original path", song.source_path.as_deref());
            if let Some(path) = song.source_path.as_deref() {
                let exists = Path::new(path).exists();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(exists, egui::Button::new("open original location"))
                        .clicked()
                    {
                        open_path(app, path);
                    }
                    if !exists {
                        ui.label(
                            egui::RichText::new("original file no longer available")
                                .small()
                                .weak(),
                        );
                    }
                });
            }
        }
        "import" => {
            kv(ui, "Source type", Some("Import"));
            kv(ui, "Original path", song.source_path.as_deref());
        }
        _ => {
            kv(ui, "Source type", Some("Unknown"));
            ui.label(
                egui::RichText::new("No source recorded (imported before source tracking).")
                    .small()
                    .weak(),
            );
        }
    }
    ui.label(
        egui::RichText::new(
            "Source data is permanent and is never changed by metadata scans or edits.",
        )
        .small()
        .weak(),
    );
}

fn kv(ui: &mut egui::Ui, label: &str, value: Option<&str>) {
    let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
        return;
    };
    ui.label(egui::RichText::new(label).small().weak());
    ui.label(egui::RichText::new(value).small());
    ui.end_row();
}

fn metadata_status_color(status: &str) -> egui::Color32 {
    match status {
        "recognized" => egui::Color32::from_rgb(140, 220, 160),
        "partial" => egui::Color32::from_rgb(220, 200, 140),
        "ambiguous" | "not_found" | "error" => egui::Color32::from_rgb(220, 140, 140),
        _ => egui::Color32::from_rgb(180, 180, 180),
    }
}

fn open_url(app: &mut SJNMusicApp, url: &str) {
    match std::process::Command::new("xdg-open").arg(url).spawn() {
        Ok(_) => {}
        Err(e) => app.push_toast(format!("Could not open URL: {e}"), ToastLevel::Warn),
    }
}

fn open_path(app: &mut SJNMusicApp, path: &str) {
    let p = Path::new(path);
    let target = if p.is_file() {
        p.parent().unwrap_or(p)
    } else {
        p
    };
    match std::process::Command::new("xdg-open").arg(target).spawn() {
        Ok(_) => {}
        Err(e) => app.push_toast(format!("Could not open path: {e}"), ToastLevel::Warn),
    }
}

/// Epoch seconds -> `YYYY-MM-DD HH:MM` (UTC) without pulling in a date crate.
fn fmt_unix(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// Send `POST /play/all` and surface the result as a toast.
fn play_all_now(app: &mut SJNMusicApp) {
    let daemon: Arc<DaemonClient> = app.daemon.clone();
    match daemon.play_all() {
        Ok(resp) => app.push_toast(
            format!("Shuffled {} song(s) into the queue", resp.queued),
            ToastLevel::Success,
        ),
        Err(e) => app.push_toast(format!("Play-all failed: {e}"), ToastLevel::Error),
    }
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
