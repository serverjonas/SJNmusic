//! History view: most-recent 100 entries. Loaded once on first paint;
//! manual refresh button.

use std::time::Duration;

use eframe::egui;

use crate::app::SJNMusicApp;

pub fn show(ui: &mut egui::Ui, app: &mut SJNMusicApp) {
    ui.heading("History");
    ui.separator();

    // First-paint hydration.
    if app.history_data.is_empty() && !app.history_loading {
        match app.daemon.history(100) {
            Ok(list) => app.history_data = list,
            Err(e) => {
                app.push_toast(format!("History failed: {}", e), crate::state::ToastLevel::Warn);
            }
        }
    }

    ui.horizontal(|ui| {
        if ui.button("Refresh").clicked() {
            app.history_loading = true;
            match app.daemon.history(100) {
                Ok(list) => app.history_data = list,
                Err(e) => app.push_toast(format!("Refresh failed: {}", e), crate::state::ToastLevel::Warn),
            }
            app.history_loading = false;
        }
        ui.label(egui::RichText::new(format!("{} entries", app.history_data.len())).small().weak());
    });

    ui.add_space(8.0);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(600.0)
        .show(ui, |ui| {
            if app.history_data.is_empty() {
                ui.label(egui::RichText::new("No history yet.").weak());
                return;
            }
            for h in &app.history_data.clone() {                    egui::Frame::group(ui.style())
                        .fill(ui.visuals().faint_bg_color)
                        .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let date = chrono_from_unix(h.played_at);
                            ui.label(egui::RichText::new(date).small().weak());
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(
                                    h.song_name.as_deref().unwrap_or("(missing song)"),
                                ).strong());
                                ui.label(
                                    egui::RichText::new(format!("id {}", h.song_id))
                                        .small()
                                        .weak(),
                                );
                            });
                            ui.label(
                                egui::RichText::new(crate::fmt::mmss(h.duration_secs_played as f64))
                                    .small(),
                            );
                        });
                    });
                std::thread::sleep(Duration::from_millis(0)); // yields
            }
        });
}

fn chrono_from_unix(uniq: i64) -> String {
    if uniq <= 0 {
        return "(no date)".into();
    }
    let secs = uniq as i64;
    // No chrono dep; compute a human-readable "YYYY-MM-DD HH:MM" via
    // SystemTime arithmetic + a small date helper.
    match systemtime_to_date(secs) {
        Some(s) => s,
        None => format!("#{}", uniq),
    }
}

fn systemtime_to_date(secs: i64) -> Option<String> {
    use std::time::{Duration as D, UNIX_EPOCH};
    let t = UNIX_EPOCH.checked_add(D::from_secs(secs as u64))?;
    let dur = t.duration_since(UNIX_EPOCH).ok()?;
    let total = dur.as_secs() as i64;
    let (year, month, day, hour, minute) = civil_from_days(total / 86400);
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        year, month, day, hour, minute
    ))
}

/// Howard Hinnant's `civil_from_days` from
/// <http://howardhinnant.github.io/date_algorithms.html>. Pure integer
/// arithmetic, no leap tables beyond the basic 400y cycle.
fn civil_from_days(z: i64) -> (i64, u32, u32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z / 146097 } else { (z - 146096) / 146097 };
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    let hour = 0;
    let minute = 0;
    (y, m, d, hour, minute)
}


