//! Best-effort system tray (Linux/macOS/Windows).
//!
//! We synthesize a 16x16 RGBA icon in memory so the binary ships without
//! image assets (matches the previous Electron version's behaviour).
//!
//! The tray itself is gated behind the `tray` cargo feature. Without
//! that feature, no `tray_icon::*` types are even compiled in — which
//! is the only way to avoid the runtime panic from `libappindicator-sys`
//! when `libappindicator3.so` isn't installed (Arch without
//! `libappindicator-gtk3`, fresh containers, locked-down macOS, …).
//! With the feature set, the actual dlopen still happens at runtime,
//! so callers need that system library on disk.
//!
//! The lightweight parts (`TraySignals`, `set_paused_hint`) stay
//! always-on so the App's polling loop can be written without
//! feature-gated code paths in app.rs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(feature = "tray")]
use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
#[cfg(feature = "tray")]
use tray_icon::TrayIconBuilder;

/// Lightweight signals so the tray menu can influence the egui app
/// without sharing the egui::Context across threads. Both fields are
/// `Ordering::Relaxed`: harmless if a couple of frames' worth of
/// "show window" / "quit" intents get coalesced.
#[derive(Clone, Debug, Default)]
pub struct TraySignals {
    pub show_window: Arc<AtomicBool>,
    pub quit: Arc<AtomicBool>,
}

impl TraySignals {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Best-effort tray install. Stays callable from main.rs regardless of
/// feature flag; the body branches on whether `--features tray` was set
/// at compile time.
pub fn try_install(host: String, port: u16, signals: TraySignals) {
    #[cfg(feature = "tray")]
    {
        match install_inner(host.clone(), port, signals) {
            Ok(()) => log::info!("tray: installed for {}:{}", host, port),
            Err(e) => log::warn!("tray: disabled ({})", e),
        }
    }
    #[cfg(not(feature = "tray"))]
    {
        // Compile the binary with `cargo build --features tray` (after
        // installing `libappindicator-gtk3` on Arch or
        // `libappindicator3-dev` on Debian) to bring the tray back.
        log::info!("tray: disabled (built without --features tray)");
        let _ = host;
        let _ = port;
        let _ = signals;
    }
}

#[cfg(feature = "tray")]
fn install_inner(
    host: String,
    port: u16,
    signals: TraySignals,
) -> Result<(), Box<dyn std::error::Error>> {
    let icon = tray_icon::Icon::from_rgba(synth_icon(), 16, 16)?;
    let menu = build_menu(false)?;
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("sjnmusic")
        .with_icon(icon)
        .build()?;
    install_menu_handler(host, port, signals);
    // Keep the tray icon alive for the lifetime of the program.
    // tray-icon uses a global registry internally, so dropping the
    // local handle does NOT remove the icon. We leak the Arc-like
    // handle into a parking-lot forever by leaking the box.
    Box::leak(Box::new(_tray));
    Ok(())
}

// `Menu::with_items` returns muda's `Error` re-exported under
// `tray_icon::menu`, NOT the top-level `tray_icon::Error`. They're
// distinct enums (muda vs tray-icon), so the return type must match.
#[cfg(feature = "tray")]
fn build_menu(paused: bool) -> Result<Menu, tray_icon::menu::Error> {
    let pause_label = if paused { "Resume" } else { "Pause" };
    let pause_item = MenuItem::with_id("pause", pause_label, true, None);
    let skip_item = MenuItem::with_id("skip", "Skip", true, None);
    let show_item = MenuItem::with_id("show", "Show window", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit", true, None);

    Menu::with_items(&[
        &pause_item,
        &skip_item,
        &PredefinedMenuItem::separator(),
        &show_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
}

#[cfg(feature = "tray")]
fn install_menu_handler(host: String, port: u16, signals: TraySignals) {
    let daemon = crate::daemon::DaemonClient::new(&host, port);
    tray_icon::menu::MenuEvent::set_event_handler(Some(move |event: tray_icon::menu::MenuEvent| {
        match event.id.as_ref() {
            "pause" => {
                let path = if paused_now() { "/resume" } else { "/pause" };
                fire_and_forget(daemon.clone(), "POST", path);
            }
            "skip" => fire_and_forget(daemon.clone(), "POST", "/skip"),
            "show" => {
                signals.show_window.store(true, Ordering::Relaxed);
            }
            "quit" => {
                signals.quit.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }));
}

/// Pause-state mirror used by the tray menu. Updated whenever the
/// App's snapshot changes; deliberately small so the tray doesn't
/// have to round-trip the daemon to decide its Pause label. Only
/// referenced from `install_menu_handler`, so it's gated to the
/// `tray` feature to avoid a `dead_code` warning in the default
/// build (where `PAUSED_HINT` is still written by `set_paused_hint`
/// even though no reader exists).
#[cfg(feature = "tray")]
fn paused_now() -> bool {
    PAUSED_HINT.load(Ordering::Relaxed)
}

static PAUSED_HINT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_paused_hint(paused: bool) {
    PAUSED_HINT.store(paused, Ordering::Relaxed);
}

/// Lightweight POST fire-and-forget. We discard the response because
/// the tray's whole point is to be quick. Errors are logged at debug
/// level so a transient transport glitch isn't noisy.
#[cfg(feature = "tray")]
fn fire_and_forget(
    daemon: crate::daemon::DaemonClient,
    method: &'static str,
    path: &'static str,
) {
    let url = format!("{}{}", daemon.base(), path);
    std::thread::spawn(move || {
        // ureq 2.12 dropped `send_empty`; for an empty POST body we
        // just `.call()` the request. Discard the response.
        let result = ureq::post(&url).call();
        if let Err(ureq::Error::Transport(e)) = &result {
            log::debug!("tray: {} {} -> {}", method, url, e);
        }
    });
}

/// Synthesize a 16x16 dark-theme play-triangle icon. The previous
/// Electron version did the same: ship no image assets, render
/// something recognisable at runtime.
#[cfg(feature = "tray")]
pub fn synth_icon() -> Vec<u8> {
    let size = 16u32;
    let mut buf = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            // Background: rgba(15,17,21,255)
            let mut r = 15;
            let mut g = 17;
            let mut b = 21;
            let a = 255u8;
            // Triangle mask centred. cy=0 is row 0, so it points right.
            let cx = x as i32 - 7;
            let cy = y as i32 - 8;
            let inside = cx.abs() <= cy && cy >= -7 && cy <= 6;
            if inside {
                r = 108;
                g = 178;
                b = 255;
            }
            buf[i] = r;
            buf[i + 1] = g;
            buf[i + 2] = b;
            buf[i + 3] = a;
        }
    }
    buf
}
