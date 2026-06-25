//! Settings view: theme picker + daemon info card. Persists the
//! chosen theme to disk via `paths::config_dir() / "sjnmusic-gui" /
//! "theme.txt"`.

use eframe::egui;

use crate::app::SJNMusicApp;
use crate::state::Theme;

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
            egui::RichText::new(
                "Default is dark. Choice persists per-machine.",
            )
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
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("Keyboard").strong());
        ui.label(egui::RichText::new("Space → pause/resume").small());
        ui.label(egui::RichText::new("Esc → clear focus / cancel").small());
    });
}

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
