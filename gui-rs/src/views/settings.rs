//! Settings view: theme picker, daemon info and the Music Metadata
//! section (AcoustID key, recognition behaviour, service tests, library
//! scans with live progress).

use std::time::{Duration, Instant};

use eframe::egui;

use crate::app::SJNMusicApp;
use crate::state::{Theme, ToastLevel};

pub fn show(ui: &mut egui::Ui, app: &mut SJNMusicApp) {
    ui.heading("Settings");
    ui.separator();

    ui.add_space(8.0);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("Theme").strong());
        ui.horizontal(|ui| {
            let dark = ui.selectable_label(app.theme == Theme::Dark, "🌙 Dark");
            if dark.clicked() {
                app.theme = Theme::Dark;
                persist_theme(Theme::Dark);
            }
            let light = ui.selectable_label(app.theme == Theme::Light, "☀ Light");
            if light.clicked() {
                app.theme = Theme::Light;
                persist_theme(Theme::Light);
            }
        });
        ui.label(
            egui::RichText::new("Default is dark. Choice persists per-machine.")
                .small()
                .weak(),
        );
    });

    ui.add_space(12.0);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("Daemon").strong());
        ui.label(
            egui::RichText::new(
                "The GUI is a remote control. It does not own audio.\n\
                 Edit ~/.sjn/music/config.toml and restart the daemon to\n\
                 change host/port, library paths, fuzzy threshold, …",
            )
            .small(),
        );
    });

    ui.add_space(12.0);
    ensure_metadata_loaded(app);
    metadata_section(ui, app);

    ui.add_space(12.0);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("Keyboard").strong());
        ui.label(egui::RichText::new("Space → pause/resume").small());
        ui.label(egui::RichText::new("Esc → clear focus / cancel").small());
    });
}

// ---------------------------------------------------------------------
// Music Metadata
// ---------------------------------------------------------------------

fn ensure_metadata_loaded(app: &mut SJNMusicApp) {
    if app.metadata_settings.is_some()
        || app.metadata_settings_loading
        || app.metadata_settings_attempted
    {
        return;
    }
    app.metadata_settings_attempted = true;
    app.metadata_settings_loading = true;
    let daemon = app.daemon.clone();
    let bus = app.metadata_settings_bus.clone();
    std::thread::spawn(move || {
        let result = daemon
            .metadata_settings()
            .and_then(|settings| {
                daemon
                    .metadata_status()
                    .map(|status| (settings, status))
            })
            .map_err(|e| e.to_string());
        if let Ok(mut slot) = bus.lock() {
            *slot = Some(result);
        }
    });
}

fn metadata_section(ui: &mut egui::Ui, app: &mut SJNMusicApp) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Music Metadata").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Reload").clicked() {
                    reload_metadata(app);
                }
            });
        });
        ui.label(
            egui::RichText::new(
                "AcoustID fingerprinting + MusicBrainz enrichment. Without an\n\
                 AcoustID key the library still works; only recognition is off.",
            )
            .small()
            .weak(),
        );
        ui.add_space(6.0);

        if app.metadata_settings_loading {
            ui.label(egui::RichText::new("Loading metadata settings…").weak());
            return;
        }

        let Some(mut s) = app.metadata_settings.clone() else {
            ui.label(
                egui::RichText::new(
                    "Metadata settings unavailable (daemon offline?).",
                )
                .color(ui.visuals().warn_fg_color),
            );
            return;
        };

        let status = app.metadata_status.clone().unwrap_or_default();

        // ---- Service status -----------------------------------------
        ui.horizontal_wrapped(|ui| {
            service_chip(ui, "AcoustID", status.acoustid_configured);
            service_chip(ui, "MusicBrainz", status.musicbrainz_enabled);
            service_chip(ui, "Chromaprint (fpcalc)", status.fpcalc_available);
            ui.label(
                egui::RichText::new(format!(
                    "Library: {} songs · {} complete · {} missing",
                    status.total_songs, status.complete_songs, status.missing_songs
                ))
                .small()
                .weak(),
            );
        });
        ui.add_space(8.0);

        // ---- API configuration --------------------------------------
        ui.label(egui::RichText::new("API configuration").strong());
        ui.horizontal(|ui| {
            ui.label("AcoustID API key:");
            let field = egui::TextEdit::singleline(&mut s.acoustid_api_key)
                .password(!app.show_acoustid_key)
                .hint_text("client API key from acoustid.org")
                .desired_width(320.0);
            ui.add(field);
            ui.checkbox(&mut app.show_acoustid_key, "👁 show");
        });
        ui.horizontal(|ui| {
            if ui.button("Test AcoustID").clicked() {
                start_service_test(app, "acoustid");
            }
            if ui.button("Test MusicBrainz").clicked() {
                start_service_test(app, "musicbrainz");
            }
            if !status.fpcalc_available {
                ui.label(
                    egui::RichText::new("fpcalc not found — install Chromaprint")
                        .small()
                        .color(ui.visuals().warn_fg_color),
                );
            }
        });
        if let Some((ok, msg)) = app.metadata_test_msg.clone() {
            ui.label(
                egui::RichText::new(msg).small().color(if ok {
                    egui::Color32::from_rgb(140, 220, 160)
                } else {
                    egui::Color32::from_rgb(220, 140, 140)
                }),
            );
        }

        ui.add_space(8.0);
        // ---- Recognition behaviour ----------------------------------
        ui.label(egui::RichText::new("Metadata recognition").strong());
        ui.checkbox(
            &mut s.auto_recognize,
            "Recognize new songs automatically on import/download",
        );
        ui.checkbox(
            &mut s.write_tags,
            "Write recognized metadata into audio files",
        );
        ui.checkbox(
            &mut s.use_musicbrainz,
            "Enrich AcoustID matches with MusicBrainz",
        );
        ui.horizontal(|ui| {
            ui.label("Minimum confidence:");
            ui.add(
                egui::Slider::new(&mut s.min_confidence, 0.0..=1.0)
                    .step_by(0.01)
                    .fixed_decimals(2),
            );
        });
        ui.horizontal(|ui| {
            ui.label("On multiple equally good matches:");
            egui::ComboBox::from_id_salt("multi-match-policy")
                .selected_text(match s.multi_match_policy.as_str() {
                    "best" => "take best match",
                    _ => "mark as ambiguous (skip)",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut s.multi_match_policy,
                        "skip".to_string(),
                        "mark as ambiguous (skip)",
                    );
                    ui.selectable_value(
                        &mut s.multi_match_policy,
                        "best".to_string(),
                        "take best match",
                    );
                });
        });

        // ---- Save ----------------------------------------------------
        let changed = app.metadata_settings.as_ref() != Some(&s);
        ui.horizontal(|ui| {
            if ui.button("💾 Save settings").clicked() {
                match app.daemon.save_metadata_settings(&s) {
                    Ok(saved) => {
                        app.metadata_settings = Some(saved);
                        app.metadata_test_msg = None;
                        app.push_toast("Metadata settings saved", ToastLevel::Success);
                    }
                    Err(e) => app.push_toast(
                        format!("Could not save settings: {e}"),
                        ToastLevel::Error,
                    ),
                }
            }
            if changed {
                ui.label(
                    egui::RichText::new("unsaved changes")
                        .small()
                        .color(ui.visuals().warn_fg_color),
                );
            }
        });

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        // ---- Scan controls ------------------------------------------
        scan_section(ui, app);
    });
}

fn service_chip(ui: &mut egui::Ui, name: &str, ok: bool) {
    let (icon, color) = if ok {
        ("✅", egui::Color32::from_rgb(140, 220, 160))
    } else {
        ("⚠", egui::Color32::from_rgb(220, 180, 140))
    };
    ui.label(
        egui::RichText::new(format!("{icon} {name}"))
            .small()
            .color(color),
    );
}

fn start_service_test(app: &mut SJNMusicApp, service: &'static str) {
    let daemon = app.daemon.clone();
    let bus = app.metadata_test_bus.clone();
    app.metadata_test_msg = Some((
        true,
        format!("Testing {}…", display_service(service)),
    ));
    std::thread::spawn(move || {
        let result = daemon
            .test_metadata_service(service)
            .map_err(|e| e.to_string());
        if let Ok(mut slot) = bus.lock() {
            *slot = Some(result);
        }
    });
}

fn display_service(service: &str) -> &'static str {
    match service {
        "acoustid" => "AcoustID",
        _ => "MusicBrainz",
    }
}

fn reload_metadata(app: &mut SJNMusicApp) {
    app.metadata_settings = None;
    app.metadata_status = None;
    app.metadata_settings_attempted = false;
    app.metadata_settings_loading = false;
    ensure_metadata_loaded(app);
}

// ---------------------------------------------------------------------
// Library scan
// ---------------------------------------------------------------------

fn scan_section(ui: &mut egui::Ui, app: &mut SJNMusicApp) {
    ui.label(egui::RichText::new("Library scan").strong());
    ui.label(
        egui::RichText::new(
            "Scans songs with missing metadata. A force rescan re-analyzes every\n\
             song; it never deletes metadata or source information.",
        )
        .small()
        .weak(),
    );

    let running = app
        .scan_status
        .as_ref()
        .map(|s| s.running)
        .unwrap_or(false);

    ui.horizontal(|ui| {
        if ui
            .add_enabled(!running, egui::Button::new("🔍 Scan library"))
            .clicked()
        {
            start_scan(app, false);
        }
        if ui
            .add_enabled(!running, egui::Button::new("♻ Force rescan"))
            .clicked()
        {
            start_scan(app, true);
        }
        if running && ui.button("⏹ Cancel scan").clicked() {
            let daemon = app.daemon.clone();
            std::thread::spawn(move || {
                let _ = daemon.cancel_scan();
            });
            app.push_toast("Cancelling scan after the current song", ToastLevel::Info);
        }
    });

    // Poll progress while a scan runs (cheap loopback request, throttled).
    if app.scan_polling {
        ui.ctx().request_repaint_after(Duration::from_millis(400));
        if app.last_scan_poll.elapsed() >= Duration::from_millis(500) {
            app.last_scan_poll = Instant::now();
            let result = app.daemon.scan_status();
            match result {
                Ok(st) => {
                    if !st.running {
                        app.scan_polling = false;
                    }
                    app.scan_status = Some(st);
                }
                Err(e) => {
                    app.scan_polling = false;
                    app.push_toast(format!("Scan status: {e}"), ToastLevel::Warn);
                }
            }
        }
    }

    let Some(st) = app.scan_status.clone() else {
        return;
    };

    ui.add_space(6.0);
    if let Some(msg) = st.message.as_ref() {
        ui.label(egui::RichText::new(msg).small().weak());
    }
    let total = st.total.max(1);
    let frac = (st.processed as f32 / total as f32).clamp(0.0, 1.0);
    let bar_text = format!("{} / {}", st.processed, st.total);
    ui.add(
        egui::ProgressBar::new(frac)
            .desired_width(f32::INFINITY)
            .text(bar_text),
    );
    egui::Grid::new("scan-progress")
        .num_columns(2)
        .spacing([18.0, 4.0])
        .show(ui, |ui| {
            ui.label("Total songs");
            ui.label(st.total.to_string());
            ui.end_row();
            ui.label("Already complete");
            ui.label(st.already_complete.to_string());
            ui.end_row();
            ui.label("Missing metadata");
            ui.label(st.missing.to_string());
            ui.end_row();
            ui.label("Recognized");
            ui.label(st.recognized.to_string());
            ui.end_row();
            ui.label("Not recognized");
            ui.label(st.not_recognized.to_string());
            ui.end_row();
            ui.label("Skipped");
            ui.label(st.skipped.to_string());
            ui.end_row();
            ui.label("Errors");
            ui.label(st.errors.to_string());
            ui.end_row();
            ui.label("Updated database");
            ui.label(st.updated_database.to_string());
            ui.end_row();
            ui.label("Updated files");
            ui.label(st.updated_files.to_string());
            ui.end_row();
        });

    if let Some(step) = st
        .current_song
        .clone()
        .or_else(|| st.current_step.clone())
    {
        ui.label(egui::RichText::new(step).small().weak());
    }

    if !st.running && st.finished {
        if let Some(r) = st.result.as_ref() {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(if r.cancelled {
                    "Scan cancelled — processed songs were kept."
                } else {
                    "Scan completed."
                })
                .strong(),
            );
        }
    }
}

fn start_scan(app: &mut SJNMusicApp, force: bool) {
    match app.daemon.start_scan(force) {
        Ok(st) => {
            app.scan_status = Some(st);
            app.scan_polling = true;
            app.last_scan_poll = Instant::now() - Duration::from_secs(10);
            app.metadata_test_msg = None;
            app.push_toast(
                if force {
                    "Force rescan started"
                } else {
                    "Library scan started"
                },
                ToastLevel::Info,
            );
        }
        Err(e) => app.push_toast(format!("Could not start scan: {e}"), ToastLevel::Error),
    }
}

// ---------------------------------------------------------------------
// Theme persistence
// ---------------------------------------------------------------------

fn persist_theme(theme: Theme) {
    if let Some(path) = theme_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, theme.as_str());
    }
}

fn theme_path() -> Option<std::path::PathBuf> {
    let mut p = dirs::config_dir()?;
    p.push("sjnmusic-gui");
    p.push("theme.txt");
    Some(p)
}

/// Load the persisted theme if the file exists, otherwise None.
pub fn load_persisted_theme() -> Option<String> {
    let path = theme_path()?;
    match std::fs::read_to_string(&path) {
        Ok(s) if s.trim().eq_ignore_ascii_case("light") => Some("light".to_string()),
        Ok(_) => Some("dark".to_string()),
        Err(_) => None,
    }
}
