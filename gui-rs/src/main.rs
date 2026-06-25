//! Entry point. Reads env config, initialises logging, then hands control
//! to `eframe::run_native`. The tray icon (Linux/macOS/Windows) is installed
//! on the same callback as a best-effort extension: if tray construction
//! fails (no StatusNotifierItem watcher, headless server, …) we just log a
//! warning and keep the window running.

mod app;
mod daemon;
mod fmt;
mod state;
mod thumbnails;
mod tray;
mod views;

use std::env;

use eframe::egui;

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .init();

    let host = env::var("SJNMUSIC_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = env::var("SJNMUSIC_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(14567);

    let theme = env::var("SJNMUSIC_THEME")
        .ok()
        .or_else(crate::views::settings::load_persisted_theme)
        .unwrap_or_else(|| "dark".to_string());

    let viewport = egui::ViewportBuilder::default()
        .with_title("sjnmusic")
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([820.0, 520.0]);

    let options = eframe::NativeOptions {
        viewport,
        vsync: true,
        ..Default::default()
    };

    eframe::run_native(
        "sjnmusic",
        options,
        Box::new(|cc| {
            // Tray install gets its own shared atomic bucket so its caller
            // (eframe::App::update) can pick up "show window" / "quit" intents
            // without us needing to marshal egui::Context across threads.
            let signals = tray::TraySignals::new();
            // Tray is feature-gated; see crate-level docs in tray.rs.
            tray::try_install(host.clone(), port, signals.clone());

            Ok(Box::new(app::SJNMusicApp::new(
                cc,
                host,
                port,
                theme,
                signals,
            )))
        }),
    )
}
